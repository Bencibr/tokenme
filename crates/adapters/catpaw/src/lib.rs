//! catpaw adapter.
//!
//! One source: `~/.sankuai/CatPawAI/sqliteDB/globalCache.sqlite`, table
//! `t_ui_messages` — one row per chat block, where every row carrying a
//! `tokenUsage` object is one billed model call with **real** token numbers
//! (`prompt_tokens` / `cacheReadTokens` / `completion_tokens`, the exclusive
//! convention: the cached prefix is not folded into the prompt). The
//! transcripts under `~/.catpaw/projects` are prose and carry no usage at all;
//! see [`paths`] for the survey and [`db`] for the row contract, including why
//! a corrupted store degrades to "read nothing, keep the cursor".
//!
//! Owned by the catpaw-adapter workstream. The public surface (`CatpawAdapter`,
//! `TOOL_ID`) is frozen — `usage-adapter-all` and the CLI link against it.

mod db;
mod paths;

use std::path::Path;

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile,
};

pub const TOOL_ID: &str = "catpaw";
pub(crate) const DISPLAY_NAME: &str = "CatPaw";

#[derive(Debug, Default, Clone, Copy)]
pub struct CatpawAdapter;

impl SourceAdapter for CatpawAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            usage_form: usage_core::UsageForm::PerCall,
            meter: usage_core::Meter::Tokens,
            // `actualUseModelName` sits inside the same content JSON; opaque
            // numeric ids are passed verbatim rather than invented around.
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        probe()
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        discover(filter)
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        db::read(&file.path, cursor, &file.key()).map(|(outcome, _)| outcome)
    }
}

fn probe() -> Option<DetectedSource> {
    let db = paths::db_path()?;
    let roots = paths::data_root().into_iter().chain(paths::transcripts_dir()).collect();
    Some(DetectedSource {
        id: TOOL_ID.to_string(),
        display: DISPLAY_NAME.to_string(),
        roots,
        hint: Some(format!("globalCache ({})", human_size(&db))),
    })
}

fn discover(filter: &DateFilter) -> Vec<SourceFile> {
    let mut out = Vec::new();
    let Some(db) = paths::db_path() else { return out };
    // One append-only file for every conversation; `DateFilter` can only prune
    // by mtime, the exact cut happens on the event stream.
    let Ok(meta) = std::fs::metadata(&db) else { return out };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|age| age.as_millis() as i64)
        .unwrap_or(0);
    // A file written before `until_ms` cannot hold a newer call.
    if filter.until_ms.is_some_and(|until| mtime > until) {
        return out;
    }
    out.push(SourceFile { path: db, kind: usage_core::FileKind::Sqlite, size: meta.len(), mtime_ms: mtime });
    out
}

fn human_size(path: &Path) -> String {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let (value, unit) = match bytes {
        b if b >= 1 << 30 => (b as f64 / (1 << 30) as f64, "GB"),
        b if b >= 1 << 20 => (b as f64 / (1 << 20) as f64, "MB"),
        b if b >= 1 << 10 => (b as f64 / (1 << 10) as f64, "KB"),
        b => (b as f64, "B"),
    };
    if unit == "B" { format!("{bytes} {unit}") } else { format!("{value:.1} {unit}") }
}

#[cfg(test)]
mod smoke {
    use std::collections::BTreeSet;

    use usage_core::{DateFilter, Meter, ReadCursor, SourceAdapter, TokenCounts};

    #[test]
    fn semantics_and_identity_are_frozen() {
        let a = crate::CatpawAdapter;
        assert_eq!(a.id(), "catpaw");
        assert_eq!(a.display_name(), "CatPaw");
        let s = a.semantics();
        assert_eq!(s.meter, Meter::Tokens);
        assert!(s.dedupes_by_id);
        assert!(!s.reports_quota);
    }

    /// The whole pipeline on a real-shaped db: usage rows bill once, plain
    /// rows vanish, the cursor resumes with no double count.
    #[test]
    fn reads_a_fixture_db_end_to_end() {
        let _env = crate::paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(crate::paths::ENV_CATPAW_DATA, dir.path());
        let db = dir.path().join("sqliteDB/globalCache.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE t_conversation (id INTEGER PRIMARY KEY AUTOINCREMENT, conversation_id TEXT NOT NULL UNIQUE,
             history_title TEXT, ts INTEGER NOT NULL, project_path TEXT);
             CREATE TABLE t_ui_messages (id INTEGER PRIMARY KEY AUTOINCREMENT, conversation_id TEXT, message_id TEXT,
             message_type TEXT, content TEXT, create_time INTEGER);
             INSERT INTO t_conversation (conversation_id, ts, project_path) VALUES ('conv-1', 1, '/Users/me/work/neuro');
             INSERT INTO t_ui_messages (conversation_id, message_id, message_type, content, create_time) VALUES
             ('conv-1', 'msg-1', 'text', '{\"role\":\"assistant\",\"actualUseModelName\":\"100000000013\",\"tokenUsage\":{\"completion_tokens\":163,\"prompt_tokens\":366,\"cacheReadTokens\":156416,\"cacheWriteTokens\":0,\"total_tokens\":529}}', 1787000000000),
             ('conv-1', 'msg-2', 'tool',  '{\"tokenUsage\":{\"completion_tokens\":57,\"prompt_tokens\":26104,\"cacheReadTokens\":0,\"total_tokens\":26161}}', 1787000001000),
             ('conv-1', 'msg-3', 'thinking', '{\"text\":\"reasoning has no numbers\"}', 1787000002000),
             ('conv-1', 'msg-4', 'tool',  '{\"tokenUsage\":{\"completion_tokens\":0,\"prompt_tokens\":0,\"cacheReadTokens\":0,\"total_tokens\":0}}', 1787000003000),
             ('conv-1', 'msg-5', 'tool',  'not json at all', 1787000004000);",
        )
        .unwrap();
        drop(conn);

        let adapter = crate::CatpawAdapter;
        let sources = adapter.discover(&DateFilter::default());
        assert_eq!(sources.len(), 1, "the db is discovered");
        let outcome = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert_eq!(outcome.events.len(), 2, "one usage row plus the zero row are not billable");
        let e = &outcome.events[0];
        assert_eq!(e.tool, "catpaw");
        assert_eq!(e.session, "conv-1");
        assert_eq!(e.counts, TokenCounts { input: 366.0, cache_creation: 0.0, cache_read: 156_416.0, output: 163.0, reasoning: 0.0, credits: 0.0 });
        assert_eq!(e.model.as_deref(), Some("100000000013"), "the IDE's own model id, verbatim");
        assert_eq!(e.project.as_deref(), Some("neuro"), "basename of t_conversation.project_path");
        assert_eq!(e.dedupe_key.as_deref(), Some("conv-1#msg-1"));
        assert_eq!(e.ts_ms, 1_787_000_000_000);
        assert_eq!(e.meter, Meter::Tokens);
        assert_eq!(outcome.cursor, ReadCursor(5));
        // Resuming consumes nothing new; the dedupe keys never collide.
        let again = adapter.read(&sources[0], outcome.cursor).unwrap();
        assert!(again.events.is_empty());
        let keys: BTreeSet<_> = outcome.events.iter().filter_map(|e| e.dedupe_key.clone()).collect();
        assert_eq!(keys.len(), outcome.events.len());
    }
}

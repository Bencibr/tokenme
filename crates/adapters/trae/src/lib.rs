//! Trae adapter (ByteDance's AI IDE, international and CN builds): usage read
//! out of the agent's **encrypted** SQLite store,
//! `<data-dir>/Trae/ModularData/ai-agent/database.db`.
//!
//! The store is a SQLCipher 4 variant with a **constant** key (see
//! [`crate::crypto`]); this adapter decrypts it in memory — main pages plus the
//! newest WAL frames — and reads one usage record per completed chat turn.
//!
//! ## What a turn meters
//!
//! Two sources, in priority order, both the IDE's own bookkeeping:
//!
//! 1. `chat_turn.context` JSON → `token_usage` — the per-turn breakdown
//!    (`prompt_tokens`, `completion_tokens`, `reasoning_tokens`,
//!    `cache_read_input_tokens`, `cache_creation_input_tokens`) the CN
//!    community's schema guide documents. Written when a turn completes; this
//!    machine's build (3.5.104) leaves it empty on a free account, so:
//! 2. fallback — `history_v2.token_usage` joined by message id: the turn's
//!    user message (`reply_to_message_id`, content source `user_input`) counts
//!    the prompt sent, the assistant message (`response_message_id`,
//!    content source `llm_default`) the reply. Measured live: one turn on this
//!    machine reads 1852 in / 29 out, and the assistant row's number matches
//!    `server_history_info.item_token_usage` for the same message exactly.
//!
//! Model comes from the turn's `persist_user_message_context.model_info`
//! (`config_name`, falling back to `model_name` minus its `__dollar__…`
//! suffix). The project is the session's `project.absolute_path` basename.
//!
//! Turns only ever count once: `turn_status = 'completed'` is the terminal
//! state the status column itself declares, `deleted_at = 0` excludes removed
//! ones, and all-zero usage is silence, not a session. The dedupe key
//! `trae#<turn-id>` makes the full rescan each pass idempotent.

pub mod crypto;

use std::path::PathBuf;

use serde_json::Value;
use usage_core::{
    DateFilter, DetectedSource, Error, FileKind, Meter, ModelAttr, ReadCursor, ReadOutcome,
    Semantics, SourceAdapter, SourceFile, TokenCounts, UsageEvent, UsageForm,
};

pub const TOOL_ID: &str = "trae";

/// Path to a `database.db` (or a directory holding one), replacing every
/// discovered install — the fixture and relocated-install escape hatch.
pub const ENV_TRAE_AGENT_DB: &str = "TRAE_AGENT_DB";

/// `ModularData/ai-agent/database.db` under each edition's data dir. The
/// international build uses `Trae`; the CN builds document theirs as
/// `Trae CN` and `TRAE SOLO CN` — same store, same constant key.
const EDITIONS: [&str; 3] = ["Trae", "Trae CN", "TRAE SOLO CN"];

#[derive(Debug, Default, Clone, Copy)]
pub struct TraeAdapter;

/// The agent database of every installed edition. `TRAE_AGENT_DB` replaces the
/// list with one path (a file, or a directory that holds `database.db`).
fn agent_dbs() -> Vec<PathBuf> {
    if let Some(over) = std::env::var_os(ENV_TRAE_AGENT_DB) {
        let path = PathBuf::from(&over);
        if over.is_empty() {
            return Vec::new();
        }
        return if path.is_dir() { vec![path.join("database.db")] } else { vec![path] }
            .into_iter()
            .filter(|p| p.is_file())
            .collect();
    }
    let Some(base) = dirs::data_dir() else { return Vec::new() };
    EDITIONS
        .iter()
        .map(|edition| base.join(edition).join("ModularData").join("ai-agent").join("database.db"))
        .filter(|p| p.is_file())
        .collect()
}

fn wal_of(db: &PathBuf) -> Option<PathBuf> {
    let mut name = db.file_name()?.to_os_string();
    name.push("-wal");
    let wal = db.with_file_name(name);
    wal.is_file().then_some(wal)
}

impl SourceAdapter for TraeAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "Trae"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            usage_form: UsageForm::PerCall,
            meter: Meter::Tokens,
            model_attr: ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let dbs = agent_dbs();
        dbs.first()?;
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: dbs,
            hint: Some("agent database".into()),
        })
    }

    fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
        agent_dbs()
            .into_iter()
            .filter_map(|path| {
                let meta = std::fs::metadata(&path).ok()?;
                let mtime_ms = meta
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_millis() as i64;
                // SQLite kind: the WAL fold is what makes fresh turns visible
                // between the agent's checkpoints.
                Some(SourceFile { path, kind: FileKind::Sqlite, size: meta.len(), mtime_ms }.with_wal_activity())
            })
            .collect()
    }

    fn read(&self, file: &SourceFile, _cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        // Every pass decrypts the whole store, so the cursor is decorative;
        // dedupe keys make the rescan idempotent.
        let done = ReadCursor(file.size as u64);
        let wal = wal_of(&file.path);
        let Some(image) = crypto::database_image(&file.path, wal.as_deref()) else {
            return Ok(ReadOutcome { events: Vec::new(), cursor: done });
        };
        let Some((conn, temp)) = crypto::open_image(&image) else {
            return Ok(ReadOutcome { events: Vec::new(), cursor: done });
        };
        let events = read_turns(&conn);
        drop(conn);
        let _ = std::fs::remove_file(temp);
        Ok(ReadOutcome { events, cursor: done })
    }
}

/// The completed, alive turns joined to their session's project and their two
/// history rows (prompt sent / reply produced).
const TURN_QUERY: &str = "\
SELECT t.turn_id, t.created_at, t.context,
       hu.token_usage, ho.token_usage,
       p.absolute_path, p.name
FROM chat_turn t
JOIN chat_session s ON s.session_id = t.session_id AND s.deleted_at = 0
LEFT JOIN project p ON p.project_id = s.project_id
LEFT JOIN history_v2 hu
       ON hu.session_id = t.session_id AND hu.message_id = t.reply_to_message_id AND hu.deleted_at = 0
LEFT JOIN history_v2 ho
       ON ho.session_id = t.session_id AND ho.message_id = t.response_message_id AND ho.deleted_at = 0
WHERE t.deleted_at = 0 AND t.turn_status = 'completed'";

fn read_turns(conn: &rusqlite::Connection) -> Vec<UsageEvent> {
    let mut stmt = match conn.prepare(TURN_QUERY) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt
        .query_map([], |row| {
            Ok(TurnRow {
                turn_id: row.get::<_, String>(0).unwrap_or_default(),
                created_at: row.get::<_, i64>(1).unwrap_or(0),
                context: row.get::<_, String>(2).unwrap_or_default(),
                prompt_history: row.get::<_, Option<i64>>(3).ok().flatten(),
                reply_history: row.get::<_, Option<i64>>(4).ok().flatten(),
                absolute_path: row.get::<_, Option<String>>(5).ok().flatten(),
                project_name: row.get::<_, Option<String>>(6).ok().flatten(),
            })
        })
        .ok();
    let Some(rows) = rows else { return Vec::new() };
    rows.flatten().filter_map(|row| row.event()).collect()
}

struct TurnRow {
    turn_id: String,
    created_at: i64,
    context: String,
    /// `history_v2.token_usage` of the turn's user message — the prompt sent.
    prompt_history: Option<i64>,
    /// …of the turn's assistant message — the reply produced.
    reply_history: Option<i64>,
    absolute_path: Option<String>,
    project_name: Option<String>,
}

impl TurnRow {
    fn event(self) -> Option<UsageEvent> {
        if self.turn_id.is_empty() {
            return None;
        }
        let ctx: Value = serde_json::from_str(&self.context).unwrap_or(Value::Null);
        let counts = counts_from(&ctx, self.prompt_history, self.reply_history);
        if counts.is_zero() {
            return None;
        }
        let mut event = UsageEvent::new(TOOL_ID, self.created_at.saturating_mul(1000), &self.turn_id);
        event.counts = counts;
        event.meter = Meter::Tokens;
        event.model = model_from(&ctx);
        event.project = self
            .absolute_path
            .as_deref()
            .and_then(basename)
            .or(self.project_name)
            .filter(|p| !p.is_empty());
        event.dedupe_key = Some(format!("trae#{}", self.turn_id));
        Some(event)
    }
}

/// Priority to the documented breakdown; the history join is the fallback this
/// build actually answers with.
fn counts_from(ctx: &Value, prompt_history: Option<i64>, reply_history: Option<i64>) -> TokenCounts {
    let num = |v: Option<&Value>| v.and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0);
    let usage = ctx.get("token_usage").filter(|u| u.is_object());
    if let Some(usage) = usage {
        return TokenCounts {
            input: num(usage.get("prompt_tokens")),
            output: num(usage.get("completion_tokens")),
            cache_read: num(usage.get("cache_read_input_tokens")),
            cache_creation: num(usage.get("cache_creation_input_tokens")),
            reasoning: num(usage.get("reasoning_tokens")),
            credits: 0.0,
        };
    }
    TokenCounts {
        input: prompt_history.map(|n| n.max(0) as f64).unwrap_or(0.0),
        output: reply_history.map(|n| n.max(0) as f64).unwrap_or(0.0),
        cache_read: 0.0,
        cache_creation: 0.0,
        reasoning: 0.0,
        credits: 0.0,
    }
}

/// `config_name` is the clean product name (`Dola-Seed-2.0-Code`); `model_name`
/// carries a wire suffix (`…__dollar__dev`) that reads fine once cut at `__`.
fn model_from(ctx: &Value) -> Option<String> {
    let info = ctx.pointer("/persist_user_message_context/model_info")?;
    let clean = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(|s| s.split("__").next().unwrap_or(s).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    clean(info.get("config_name")).or_else(|| clean(info.get("model_name")))
}

fn basename(path: &str) -> Option<String> {
    path.rsplit(['/', '\\']).find(|s| !s.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::testing::encrypt_image;
    use rusqlite::Connection;

    /// The schema subset the adapter reads, in one fixture db: one session in
    /// a project, one completed turn with both history rows, plus a streaming
    /// and a deleted turn that must stay out.
    fn fixture_plain(turn_context: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("plain.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "PRAGMA page_size = 4096;
             CREATE TABLE chat_session (session_id TEXT, project_id TEXT, deleted_at BIGINT DEFAULT 0);
             CREATE TABLE project (project_id TEXT, absolute_path TEXT, name TEXT);
             CREATE TABLE chat_turn (turn_id TEXT, session_id TEXT, turn_status TEXT,
               context TEXT, created_at BIGINT, deleted_at BIGINT DEFAULT 0,
               reply_to_message_id TEXT, response_message_id TEXT);
             CREATE TABLE history_v2 (session_id TEXT, message_id TEXT, token_usage BIGINT, deleted_at BIGINT DEFAULT 0);
             INSERT INTO project VALUES ('p1', '/Users/me/workspace/bug-hunter', 'hunter');
             INSERT INTO chat_session VALUES ('s1', 'p1', 0);
             INSERT INTO chat_turn VALUES ('t1', 's1', 'completed', '{}', 1790609380, 0, 'm-user', 'm-bot');
             INSERT INTO chat_turn VALUES ('t-live', 's1', 'streaming', '{}', 1790609381, 0, '', '');
             INSERT INTO chat_turn VALUES ('t-gone', 's1', 'completed', '{}', 1790609382, 1790609390, '', '');
             INSERT INTO history_v2 VALUES ('s1', 'm-user', 1852, 0);
             INSERT INTO history_v2 VALUES ('s1', 'm-bot', 29, 0);",
        )
        .unwrap();
        conn.execute("UPDATE chat_turn SET context = ?1 WHERE turn_id = 't1'", [turn_context])
            .unwrap();
        drop(conn);
        // Declare the vendor's 80-byte per-page reserve, then let SQLite
        // itself rewrite the file to it (cells move below 4016) — the true
        // plaintext `encrypt_image` expects.
        let mut plain = std::fs::read(&db).unwrap();
        plain[20] = 80;
        std::fs::write(&db, &plain).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("VACUUM;").unwrap();
        drop(conn);
        std::fs::read(&db).unwrap()
    }

    #[test]
    fn a_completed_turn_becomes_one_per_call_event() {
        let _env = crate::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let plain = fixture_plain("{}");
        // The fixture turn carries a model in its context, not tokens.
        let db = dir.path().join("database.db");
        std::fs::write(&db, encrypt_image(&plain)).unwrap();
        std::env::set_var(ENV_TRAE_AGENT_DB, &db);
        let adapter = TraeAdapter;
        let sources = adapter.discover(&DateFilter::default());
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].kind, FileKind::Sqlite);
        let out = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 1, "streaming and deleted turns stay out: {:?}", out.events);
        let e = &out.events[0];
        assert_eq!(e.tool, "trae");
        assert_eq!(e.session, "t1");
        assert_eq!(e.counts.input, 1852.0, "{:?}", e.counts);
        assert_eq!(e.counts.output, 29.0);
        assert_eq!(e.dedupe_key.as_deref(), Some("trae#t1"));
        assert_eq!(e.project.as_deref(), Some("bug-hunter"));
        assert_eq!(e.meter, Meter::Tokens);
        // Re-read re-emits the same key: the indexer replaces, never doubles.
        let again = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert_eq!(again.events[0].dedupe_key, e.dedupe_key);
        std::env::remove_var(ENV_TRAE_AGENT_DB);
    }

    /// The documented `token_usage` breakdown, when a build writes it, wins
    /// over the history fallback — and the model comes from the turn context.
    #[test]
    fn a_documented_breakdown_takes_precedence() {
        let _env = crate::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let plain = fixture_plain(
            r#"{"token_usage":{"prompt_tokens":900,"completion_tokens":80,
                 "reasoning_tokens":20,"cache_read_input_tokens":5000,
                 "cache_creation_input_tokens":120},
               "persist_user_message_context":{"model_info":{
                 "config_name":"Dola-Seed-2.0-Code",
                 "model_name":"Dola-Seed-2.0-Code__dollar__dev"}}}"#,
        );
        let db = dir.path().join("database.db");
        std::fs::write(&db, encrypt_image(&plain)).unwrap();
        std::env::set_var(ENV_TRAE_AGENT_DB, &db);
        let out = TraeAdapter.read(
            &SourceFile {
                path: db.clone(),
                kind: FileKind::Sqlite,
                size: std::fs::metadata(&db).unwrap().len(),
                mtime_ms: 0,
            },
            ReadCursor(0),
        )
        .unwrap();
        assert_eq!(out.events.len(), 1);
        let e = &out.events[0];
        assert_eq!(e.counts.input, 900.0);
        assert_eq!(e.counts.output, 80.0);
        assert_eq!(e.counts.reasoning, 20.0);
        assert_eq!(e.counts.cache_read, 5000.0);
        assert_eq!(e.counts.cache_creation, 120.0);
        assert_eq!(e.model.as_deref(), Some("Dola-Seed-2.0-Code"));
        std::env::remove_var(ENV_TRAE_AGENT_DB);
    }

    /// `config_name` empty, `model_name` suffixed: the suffix is cut, not kept.
    #[test]
    fn the_wire_model_name_is_trimmed_of_its_routing_suffix() {
        let ctx: Value = serde_json::from_str(
            r#"{"persist_user_message_context":{"model_info":{"config_name":"",
                 "model_name":"kimi-k2.5__dollar__dev"}}}"#,
        )
        .unwrap();
        assert_eq!(model_from(&ctx).as_deref(), Some("kimi-k2.5"));
        assert_eq!(model_from(&Value::Null), None);
    }

    #[test]
    fn an_undecryptable_store_is_silence_not_an_error() {
        let _env = crate::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("database.db");
        std::fs::write(&db, b"not a database at all").unwrap();
        std::env::set_var(ENV_TRAE_AGENT_DB, &db);
        let adapter = TraeAdapter;
        let sources = adapter.discover(&DateFilter::default());
        assert_eq!(sources.len(), 1);
        let out = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert!(out.events.is_empty());
        std::env::remove_var(ENV_TRAE_AGENT_DB);
    }
}

#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod live {
    use super::*;

    #[test]
    #[ignore = "reads the real Trae database on this machine"]
    fn the_live_store_round_trips_through_wal() {
        std::env::remove_var(ENV_TRAE_AGENT_DB);
        let adapter = TraeAdapter;
        let sources = adapter.discover(&DateFilter::default());
        let f = &sources[0];
        println!("source: {}", f.path.display());
        println!("stat: size={} mtime={}", f.size, f.mtime_ms);
        let out = adapter.read(f, ReadCursor(0)).unwrap();
        println!("events: {}", out.events.len());
        for e in &out.events {
            println!(
                "  {} ts={} in={} out={} reasoning={} model={:?}",
                e.session, e.ts_ms, e.counts.input, e.counts.output, e.counts.reasoning, e.model
            );
        }
        assert!(!out.events.is_empty(), "the live store yields events");
    }
}

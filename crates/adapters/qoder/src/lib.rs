//! qoder adapter.
//!
//! Two sources under one tool id, because Qoder writes its usage twice and the
//! copies are complementary rather than redundant:
//!
//! 1. `~/.qoder/projects/**/*.jsonl` (append-only, one JSON record per line) under
//!    `$QODER_CONFIG_DIR`. Claude-Code shaped, but every token field in it is 0 and
//!    the only live number is `message.usage.credits`, so its events carry
//!    `Meter::Credits` — 7 605 of them on this machine, all credit-metered.
//! 2. The **IDE**'s SQLite cache,
//!    `<App Support>/Qoder{,CN}/SharedClientCache/cache/db/local.db`, table
//!    `chat_message`, whose `token_info` column carries real per-call
//!    `{prompt_tokens, cached_tokens, completion_tokens}` numbers with the model on
//!    the same row. Those events carry `Meter::Tokens`. See [`cache_db`] for the
//!    field mapping, the four independent implementations that agree on it, and the
//!    malformed-JSON trap the column hides.
//!
//! Both stores set their own per-event `meter` and share one dedupe namespace
//! (`qoder#<usage.request_id>`), which is what stops a call the IDE wrote down twice
//! from being billed twice. `usage-index` reads the per-event meter, never the tool's
//! [`Semantics`], so the mixed pair prices correctly: the credits fold into
//! `Summary::credit_cost` at the vendor's plan rate and the tokens go through the
//! normal price table.
//!
//! The public surface (`TOOL_ID`, `QoderAdapter`, this trait impl) is frozen:
//! `usage-adapter-all` and the CLI already link against it. One tool id it stays —
//! the second source is a second `SourceFile`, not a second adapter.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use usage_core::{
    DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

mod cache_db;
mod parser;
mod paths;

pub const TOOL_ID: &str = "qoder";

/// The dedupe namespace both sources share, so a call the transcript and the IDE
/// cache db both wrote lands on one key in the indexer's global `event_dedupe`
/// index. The cache db can add a message id after it (see [`cache_db`]) when one
/// request covers several calls; the prefix is what makes the pair unambiguous.
pub(crate) const DEDUPE_PREFIX: &str = "qoder#";

#[derive(Debug, Default, Clone, Copy)]
pub struct QoderAdapter;

impl SourceAdapter for QoderAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "Qoder"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            usage_form: usage_core::UsageForm::PerCall,
            // The transcript tree is the source with the volume (7 605 billable
            // records here, against a cache db that only the IDE's chat/Quest path
            // writes), so the tool as a whole is credit-metered. The cache db
            // overrides per event, on [`UsageEvent::meter`], which is what the
            // money math actually reads.
            meter: usage_core::Meter::Credits,
            model_attr: usage_core::ModelAttr::Inline,
            // Both stores carry the vendor's `request_id` for a call.
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let projects = paths::config_root()
            .map(|root| paths::projects_dir(&root))
            .filter(|dir| dir.is_dir());
        let db = paths::cache_db_path();
        let log = projects.as_ref().and_then(|dir| first_log(dir));
        let stats = db.as_deref().and_then(cache_db::stats);
        if log.is_none() && stats.is_none() {
            return None;
        }
        // One small query per store: `probe` runs at startup and must not touch all
        // 98 MB of transcripts, and the cache db census is one scan of a table that
        // is 675 KB on this machine.
        let mut parts: Vec<String> = Vec::new();
        if let Some(log) = &log {
            let logs = projects.as_ref().map(|dir| count_logs(dir)).unwrap_or(1).max(1);
            match paths::version_hint(log) {
                Some(version) => parts.push(format!("{logs} logs v{version}")),
                None => parts.push(format!("{logs} logs")),
            }
        }
        if let Some(stats) = &stats {
            parts.push(format!(
                "cache db {} rows, {} token messages, {} requests",
                stats.rows, stats.usage_rows, stats.requests
            ));
        }
        let mut roots: Vec<PathBuf> = projects.into_iter().collect();
        roots.extend(db);
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots,
            hint: Some(parts.join(" · ")),
        })
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        let mut files = match paths::config_root() {
            Some(root) => discover_in(&paths::projects_dir(&root), filter),
            None => Vec::new(),
        };
        if let Some(path) = paths::cache_db_path() {
            if let Some((size, mtime_ms)) = paths::stat_file(&path) {
                // One database holds every session, so `DateFilter` can only bound
                // it by the file's own last write — the same rule the transcript
                // walk uses.
                let sf = SourceFile { path, kind: FileKind::Sqlite, size, mtime_ms }.with_wal_activity();
                if filter.since_ms.is_none_or(|since| sf.mtime_ms >= since) {
                    files.push(sf);
                }
            }
        }
        files
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        match file.kind {
            FileKind::Jsonl => parser::read_file(file, cursor),
            // Deliberately infallible too: Qoder owns this database while the IDE
            // runs, and a locked file must not abort the pass for the other source.
            FileKind::Sqlite => Ok(cache_db::read(file, cursor)?.0),
            FileKind::Tree => Ok(ReadOutcome { events: Vec::new(), cursor }),
        }
    }
}

/// Session logs live under the mangled project dir, in `<session>/subagents/` for
/// sidechain transcripts and in `transcript/` for task runs, so this is a walk and
/// not a listing.
fn discover_in(projects: &Path, filter: &DateFilter) -> Vec<SourceFile> {
    if !projects.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<SourceFile> = WalkDir::new(projects)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && is_jsonl(entry.path()))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some(SourceFile {
                path: entry.into_path(),
                kind: FileKind::Jsonl,
                size: meta.len(),
                mtime_ms: paths::mtime_ms(&meta)?,
            })
        })
        // Project dirs are keyed by workspace rather than date, so a file's mtime
        // is the only time signal available before parsing it.
        .filter(|file| filter.since_ms.is_none_or(|since| file.mtime_ms >= since))
        .collect();
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Walk stops at the first readable `*.jsonl`: `probe` runs at startup and must
/// not touch all 75 MB of transcripts.
fn first_log(projects: &Path) -> Option<PathBuf> {
    let mut walker = WalkDir::new(projects).follow_links(false).into_iter();
    while let Some(Ok(entry)) = walker.next() {
        if !entry.file_type().is_file() || !is_jsonl(entry.path()) {
            continue;
        }
        if readable(entry.path()) {
            return Some(entry.into_path());
        }
    }
    None
}

/// How many logs the transcript root holds, for `probe`'s hint: metadata only, no
/// file is opened, so this stays cheap next to the walk `first_log` already did.
fn count_logs(projects: &Path) -> usize {
    if !projects.is_dir() {
        return 0;
    }
    WalkDir::new(projects)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && is_jsonl(entry.path()))
        .count()
}

fn is_jsonl(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("jsonl"))
}

fn readable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
        && std::fs::File::open(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_absent_tree_probes_as_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join(paths::PROJECTS_DIR);
        assert!(discover_in(&projects, &DateFilter::default()).is_empty());
        assert!(first_log(&projects).is_none());
        std::fs::create_dir_all(
            projects
                .join("-Users-demo-proj")
                .join("sess")
                .join("subagents"),
        )
        .unwrap();
        // A zero-byte log is not evidence of a working install, so `probe` skips
        // it, but `discover` still lists it: the indexer decides what to do with
        // an empty file instead of the adapter silently hiding it.
        let nested = projects
            .join("-Users-demo-proj")
            .join("sess")
            .join("subagents")
            .join("agent-a.jsonl");
        std::fs::write(&nested, "").unwrap();
        assert!(first_log(&projects).is_none());
        assert_eq!(discover_in(&projects, &DateFilter::default()).len(), 1);
        std::fs::write(&nested, b"{}\n").unwrap();
        assert_eq!(first_log(&projects).as_deref(), Some(nested.as_path()));
        // Side files sit next to the logs and are not logs.
        std::fs::write(nested.with_extension("meta.json"), "{}").unwrap();
        assert_eq!(discover_in(&projects, &DateFilter::default()).len(), 1);
    }

    #[test]
    fn a_sqlite_source_is_read_as_a_database_not_a_log() {
        let dir = tempfile::tempdir().unwrap();
        let file = cache_db::tests::fixture(dir.path());
        let outcome = QoderAdapter.read(&file, ReadCursor(0)).unwrap();
        assert_eq!(outcome.events.len(), 8, "the fixture's billable calls");
        assert_eq!(outcome.cursor, ReadCursor(13), "rowid, not a byte offset");
        // A `Tree` file is not something this adapter can read at all, and must say
        // so by returning nothing rather than by mis-parsing a database.
        let wrong = SourceFile { kind: FileKind::Tree, ..file.clone() };
        assert!(QoderAdapter.read(&wrong, ReadCursor(4)).unwrap().events.is_empty());
    }
}

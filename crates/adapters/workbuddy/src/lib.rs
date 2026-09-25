//! WorkBuddy AI adapter (Tencent's desktop agent, `com.workbuddy.workbuddy-ai`).
//!
//! One source: `~/.workbuddy-ai/workbuddy.db`, the app's own SQLite store.
//! WorkBuddy meters in **credits**, never tokens — the traces it writes carry
//! `totalTokens: 0` and `session_usage.used/size` is context fullness, so the
//! only consumption a session records is `credit_json`, written at close.
//! What the indexer gets is therefore one [`Meter::Credits`] event per
//! credited, completed session; the quota side of the story (the account's
//! remaining credits) is the `usage-quota` provider's job, not this adapter's.
//!
//! The public surface (`TOOL_ID`, `WorkBuddyAdapter`) follows the same frozen
//! contract as the other adapters: `usage-adapter-all` links it.

use usage_core::{DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile};

mod db;
mod paths;

pub const TOOL_ID: &str = "workbuddy";

/// The dedupe namespace for session credit events.
pub(crate) const DEDUPE_PREFIX: &str = "workbuddy#";

#[derive(Debug, Default, Clone, Copy)]
pub struct WorkBuddyAdapter;

impl SourceAdapter for WorkBuddyAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "WorkBuddy"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            // One row per session, carrying the session's whole credit total.
            usage_form: usage_core::UsageForm::Cumulative,
            meter: usage_core::Meter::Credits,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let path = paths::db_path()?;
        let sessions = db::count_sessions(&path).unwrap_or(0);
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: vec![path],
            hint: Some(format!("{sessions} sessions")),
        })
    }

    fn discover(&self, _filter: &usage_core::DateFilter) -> Vec<SourceFile> {
        let Some(path) = paths::db_path() else { return Vec::new() };
        let Some((size, mtime_ms)) = paths::stat_file(&path) else { return Vec::new() };
        // One database holds every session: the date filter can only bound it
        // by the file's own last write, same rule the transcript walks use.
        vec![SourceFile { path, kind: FileKind::Sqlite, size, mtime_ms }]
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        match file.kind {
            FileKind::Sqlite => Ok(db::read(file, cursor)?.0),
            _ => Ok(ReadOutcome { events: Vec::new(), cursor }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_database_probes_as_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("WORKBUDDY_CONFIG_DIR", dir.path());
        assert!(WorkBuddyAdapter.probe().is_none());
        assert!(WorkBuddyAdapter.discover(&usage_core::DateFilter::default()).is_empty());
        std::env::remove_var("WORKBUDDY_CONFIG_DIR");
    }

    #[test]
    fn a_database_is_discovered_as_one_sqlite_source() {
        let dir = tempfile::tempdir().unwrap();
        let file = db::tests::fixture(dir.path());
        std::env::set_var("WORKBUDDY_CONFIG_DIR", dir.path());
        let files = WorkBuddyAdapter.discover(&usage_core::DateFilter::default());
        std::env::remove_var("WORKBUDDY_CONFIG_DIR");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].kind, FileKind::Sqlite);
        assert_eq!(files[0].path, file.path);
    }
}

//! Hermes Agent adapter (Nous Research, `hermes-agent`).
//!
//! One source: `<HERMES_HOME>/state.db`, the tool's own SQLite store, and more
//! precisely its `session_model_usage` table — the per-(session, model, task)
//! cumulative token ledger Hermes keeps itself, including cache read/write and
//! reasoning splits. Usage lives there whether the credential behind it is an
//! API key or an OAuth pool entry, which is why this is an adapter and not a
//! `usage-quota` probe: Hermes introduces no quota window of its own, and the
//! vendor windows behind its credential pool are the other adapters' territory
//! (zai is already covered by the zcode adapter).
//!
//! The tool resolves its home as `HERMES_HOME` → `%LOCALAPPDATA%\hermes` on
//! Windows → `~/.hermes`; this adapter mirrors that ladder in `paths`. The
//! public surface (`TOOL_ID`, `HermesAdapter`) follows the same frozen contract
//! as the other adapters: `usage-adapter-all` links it.

use usage_core::{DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile};

mod db;
mod paths;

pub const TOOL_ID: &str = "hermes";

/// The dedupe namespace for per-session model-slice events.
pub(crate) const DEDUPE_PREFIX: &str = "hermes#";

#[derive(Debug, Default, Clone, Copy)]
pub struct HermesAdapter;

impl SourceAdapter for HermesAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "Hermes"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            // One row per (session, model, task) slice, carrying that slice's
            // whole-to-date totals; the indexer replaces on the dedupe key.
            usage_form: usage_core::UsageForm::Cumulative,
            meter: usage_core::Meter::Tokens,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let path = paths::db_path()?;
        let slices = db::count_usage(&path).unwrap_or(0);
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: vec![path],
            hint: Some(format!("{slices} model slices")),
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
    use usage_core::DateFilter;

    #[test]
    fn an_absent_database_probes_as_not_installed() {
        let _guard = paths::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HERMES_HOME", dir.path());
        assert!(HermesAdapter.probe().is_none());
        assert!(HermesAdapter.discover(&DateFilter::default()).is_empty());
        std::env::remove_var("HERMES_HOME");
    }

    #[test]
    fn the_usage_ledger_is_one_sqlite_source_with_cumulative_slices() {
        let _guard = paths::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = db::tests::fixture(dir.path());
        std::env::set_var("HERMES_HOME", dir.path());

        let files = HermesAdapter.discover(&DateFilter::default());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].kind, FileKind::Sqlite);

        // Nothing ingested yet: the cursor starts at zero and stays there.
        let first = HermesAdapter.read(&files[0], ReadCursor::default()).unwrap();
        assert!(first.events.is_empty());

        db::tests::insert_usage(&file, "s1", "glm-5.3-flash", "chat", 34250, 7600, 1790568283.31);
        let pass = HermesAdapter.read(&files[0], ReadCursor::default()).unwrap();
        assert_eq!(pass.events.len(), 1);
        let event = &pass.events[0];
        assert_eq!(event.model.as_deref(), Some("glm-5.3-flash"));
        assert_eq!(event.counts.input, 34250.0);
        assert_eq!(event.counts.output, 7600.0);
        assert_eq!(event.ts_ms, 1790568283310);
        assert_eq!(
            event.dedupe_key.as_deref(),
            Some("hermes#s1#glm-5.3-flash##chat"),
            "session × model × provider × task names the slice"
        );
        assert_eq!(pass.cursor, ReadCursor(1790568283310));

        // The slice grows in place; replaying from the new cursor sees it only
        // because `last_seen` moved, and the dedupe key stays stable.
        db::tests::insert_usage(&file, "s1", "glm-5.3-flash", "chat", 40000, 9000, 1790569000.0);
        let again = HermesAdapter.read(&files[0], pass.cursor).unwrap();
        assert_eq!(again.events.len(), 1);
        assert_eq!(again.events[0].dedupe_key, event.dedupe_key);
        assert_eq!(again.events[0].counts.input, 40000.0);

        // An opened-but-empty session is skipped but advances the cursor.
        db::tests::insert_usage(&file, "s2", "glm-5.3-flash", "chat", 0, 0, 1790569100.0);
        let empty = HermesAdapter.read(&files[0], again.cursor).unwrap();
        assert!(empty.events.is_empty());
        assert_eq!(empty.cursor, ReadCursor(1790569100000));

        std::env::remove_var("HERMES_HOME");
    }
}

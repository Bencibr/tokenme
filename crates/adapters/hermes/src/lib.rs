//! Hermes Agent adapter (Nous Research, `hermes-agent`).
//!
//! One source: `<HERMES_HOME>/state.db`, the tool's own SQLite store, and more
//! precisely its `session_model_usage` table — the cumulative token ledger
//! Hermes keeps itself, one row per `(session, model, billing_provider,
//! billing_base_url, billing_mode, task)` slice, including cache read/write and
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
            // One row per (session, model, provider, base_url, mode, task)
            // slice — the whole primary key — carrying that slice's
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
        vec![SourceFile { path, kind: FileKind::Sqlite, size, mtime_ms }.with_wal_activity()]
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
    use std::collections::{BTreeMap, BTreeSet};
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
            Some("hermes#s1#glm-5.3-flash####chat"),
            "session × model × provider × base_url × mode × task names the slice, \
             and the unset billing columns stay empty segments, not missing ones"
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

    /// Two slices that differ **only** in `billing_base_url` (with and without
    /// the trailing slash Hermes has been seen writing both forms of) or only
    /// in `billing_mode` are two rows of the source's primary key, so they must
    /// be two dedupe keys. Under the old four-field key the second one folded
    /// into the first, and because the index only ever replaces on a *larger*
    /// total, the smaller slice's tokens were dropped forever — an
    /// under-count no re-read could recover.
    #[test]
    fn slices_differing_only_by_base_url_or_billing_mode_keep_their_own_keys() {
        let _guard = paths::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = db::tests::fixture(dir.path());
        std::env::set_var("HERMES_HOME", dir.path());
        let files = HermesAdapter.discover(&DateFilter::default());
        let source = &files[0];

        let sid = "20260928_115328_e0edbf";
        // Straight from `state.db`: the same session, model and provider under
        // `…/v4` and `…/v4/`.
        db::tests::insert_slice(
            &file, sid, "glm-5.3-flash", "zai", "https://open.bigmodel.cn/api/paas/v4", "", "",
            131033, 28907, 1790573420.626,
        );
        db::tests::insert_slice(
            &file, sid, "glm-5.3-flash", "zai", "https://open.bigmodel.cn/api/paas/v4/", "", "approval",
            1027, 1955, 1790573420.627,
        );
        // The same slice once more, now differing *only* in the trailing slash:
        // `task`, `model`, `session` and `provider` all match, so this is the
        // pair the old key collided. The slashed one is the smaller slice.
        db::tests::insert_slice(
            &file, "t", "m", "p", "https://h/v2", "api", "chat", 900, 90, 1790573420.628,
        );
        db::tests::insert_slice(
            &file, "t", "m", "p", "https://h/v2/", "api", "chat", 300, 30, 1790573420.629,
        );
        // Same URL, same everything else, different `billing_mode`.
        db::tests::insert_slice(
            &file, "s", "m", "p", "https://h/v1", "api", "", 500, 50, 1790573420.630,
        );
        db::tests::insert_slice(
            &file, "s", "m", "p", "https://h/v1", "oauth", "", 300, 30, 1790573420.631,
        );

        let pass = HermesAdapter.read(source, ReadCursor::default()).unwrap();
        assert_eq!(pass.events.len(), 6, "one event per primary-key slice");
        let keys: BTreeSet<&str> = pass.events.iter().map(|e| e.dedupe_key.as_deref().unwrap()).collect();
        assert_eq!(keys.len(), 6, "no two slices may share a dedupe key: {keys:?}");

        // Each slice keeps its own totals, keyed by the exact shape the index
        // stores: `:` is escaped, `/` stays literal, the empty fields stay
        // empty segments.
        let totals: BTreeMap<&str, (f64, f64)> = pass
            .events
            .iter()
            .map(|e| (e.dedupe_key.as_deref().unwrap(), (e.counts.input, e.counts.output)))
            .collect();
        assert_eq!(
            totals["hermes#t#m#p#https%3A//h/v2#api#chat"],
            (900.0, 90.0),
            "the unslashed slice"
        );
        assert_eq!(
            totals["hermes#t#m#p#https%3A//h/v2/#api#chat"],
            (300.0, 30.0),
            "the slashed slice survives with its own (smaller) total instead of being swallowed"
        );
        assert_eq!(totals["hermes#s#m#p#https%3A//h/v1#api#"], (500.0, 50.0));
        assert_eq!(totals["hermes#s#m#p#https%3A//h/v1#oauth#"], (300.0, 30.0));
        assert_eq!(
            totals["hermes#20260928_115328_e0edbf#glm-5.3-flash#zai#https%3A//open.bigmodel.cn/api/paas/v4##"],
            (131033.0, 28907.0),
            "the real ledger row"
        );

        // Idempotent under a re-read from byte 0: the same slices, the same
        // keys, so the indexer replaces rather than double-counts.
        let replay = HermesAdapter.read(source, ReadCursor::default()).unwrap();
        let replay_keys: BTreeSet<&str> = replay.events.iter().map(|e| e.dedupe_key.as_deref().unwrap()).collect();
        assert_eq!(replay_keys, keys, "a re-read reproduces the same keys");
        let replay_totals: BTreeMap<&str, (f64, f64)> = replay
            .events
            .iter()
            .map(|e| (e.dedupe_key.as_deref().unwrap(), (e.counts.input, e.counts.output)))
            .collect();
        assert_eq!(replay_totals, totals, "and the same totals ride on them");

        // The slice that grew in place keeps its key across the growth — the
        // cursor may replay it freely.
        db::tests::insert_slice(&file, "t", "m", "p", "https://h/v2/", "api", "chat", 1200, 40, 1790573600.0);
        let grown = HermesAdapter.read(source, pass.cursor).unwrap();
        let grown_slice = grown
            .events
            .iter()
            .find(|e| e.dedupe_key.as_deref() == Some("hermes#t#m#p#https%3A//h/v2/#api#chat"))
            .expect("the grown slice comes back under its own key, not a new one");
        assert_eq!(grown_slice.counts.input, 1200.0);

        std::env::remove_var("HERMES_HOME");
    }

    /// The key is an encoding of a six-field *tuple*, so which field is empty
    /// has to be readable from it, and a value holding a separator has to be
    /// escaped rather than shuffled.
    #[test]
    fn the_key_separates_fields_and_escapes_a_separator_inside_a_value() {
        let _guard = paths::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = db::tests::fixture(dir.path());
        std::env::set_var("HERMES_HOME", dir.path());
        let files = HermesAdapter.discover(&DateFilter::default());
        let source = &files[0];

        // Provider filled and URL empty, against URL filled and provider empty.
        db::tests::insert_slice(&file, "u", "m", "x", "", "api", "chat", 10, 1, 1790573420.1);
        db::tests::insert_slice(&file, "u", "m", "", "x", "api", "chat", 20, 2, 1790573420.2);
        // A `#` inside `task`, and a task/`mode` split that spells the same
        // text when nothing escapes it.
        db::tests::insert_slice(&file, "v", "m", "p", "https://h/v4", "", "a#b", 30, 3, 1790573420.3);
        db::tests::insert_slice(&file, "v", "m", "p", "https://h/v4", "a", "b", 40, 4, 1790573420.4);
        // A port — a second colon in the URL — and a literal `%3A` already in
        // a value, which must not read back as a colon.
        db::tests::insert_slice(&file, "w", "m", "p", "https://h:9000/v4", "", "chat", 50, 5, 1790573420.5);
        db::tests::insert_slice(&file, "w", "m", "p", "https://h%3A//v4", "", "chat", 60, 6, 1790573420.6);

        let pass = HermesAdapter.read(source, ReadCursor::default()).unwrap();
        let keys: BTreeSet<&str> = pass.events.iter().map(|e| e.dedupe_key.as_deref().unwrap()).collect();
        assert_eq!(keys.len(), 6, "every one of these is a distinct slice: {keys:?}");
        assert!(keys.contains("hermes#u#m#x##api#chat") && keys.contains("hermes#u#m##x#api#chat"));
        assert!(
            keys.contains("hermes#v#m#p#https%3A//h/v4##a%23b") && keys.contains("hermes#v#m#p#https%3A//h/v4#a#b"),
            "a `#` inside a value is escaped, so field order stays readable"
        );
        assert!(
            keys.contains("hermes#w#m#p#https%3A//h%3A9000/v4##chat")
                && keys.contains("hermes#w#m#p#https%3A//h%253A//v4##chat"),
            "escaping stays reversible"
        );

        std::env::remove_var("HERMES_HOME");
    }
}

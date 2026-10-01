//! zcode adapter.
//!
//! Primary source: `~/.zcode/cli/db/db.sqlite`, table `model_usage` — one row per
//! billed model call (18,112 rows / 151 sessions here), read as
//! [`FileKind::Sqlite`] with the highest consumed `rowid` as cursor. Fallback:
//! `~/.zcode/cli/rollout/model-io-sess_*.jsonl`, `type:"model_io"` records with
//! usage under `response.providerMetadata.<provider>.usage`, used only when the db
//! is absent — every rollout record also lives in `model_usage`, so reading both
//! would double-bill. See [`paths`] for the measurements and [`db`]/[`parser`] for
//! the two opposite token conventions.
//!
//! Owned by the zcode-adapter workstream.
//! The public surface (`ZcodeAdapter`, `TOOL_ID`) is frozen — `usage-adapter-all`
//! and the CLI already link against it.

mod db;
mod loader;
mod parser;
mod paths;

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

pub const TOOL_ID: &str = "zcode";
pub(crate) const DISPLAY_NAME: &str = "ZCode";

#[derive(Debug, Default, Clone, Copy)]
pub struct ZcodeAdapter;

impl SourceAdapter for ZcodeAdapter {
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
            // `model_id` sits on the same row as the counts; no join is needed.
            model_attr: usage_core::ModelAttr::Inline,
            // `logical_request_id` + `attempt_index`, or `requestId` + `attempt`.
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        loader::probe()
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        loader::discover(filter)
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        loader::read(file, cursor)
    }
}

#[cfg(test)]
mod smoke {
    use std::collections::BTreeSet;

    use usage_core::{DateFilter, Meter, ReadCursor, TokenCounts};

    use super::*;

    /// Reads the real `~/.zcode/cli` and reports the totals the db holds.
    #[test]
    #[ignore = "reads ~/.zcode/cli"]
    fn real_db_matches_zcodes_own_totals() {
        let adapter = ZcodeAdapter;
        let files = adapter.discover(&DateFilter::default());
        assert_eq!(files.len(), 1, "the db is the single active source: {files:?}");
        assert_eq!(files[0].kind, usage_core::FileKind::Sqlite);
        let outcome = adapter.read(&files[0], ReadCursor(0)).unwrap();
        let (db_outcome, census) = db::read(&files[0].path, ReadCursor(0), &files[0].key()).unwrap();
        assert_eq!(db_outcome.events.len(), census.events);
        assert_eq!(census.inclusive_violations, 0, "input_tokens >= cache_read_input_tokens on every row");
        let mut totals = TokenCounts::default();
        let mut models = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let mut projects = BTreeSet::new();
        for e in &outcome.events {
            assert!(e.dedupe_key.as_ref().is_some_and(|k| keys.insert(k.clone())), "duplicate dedupe key: {:?}", e.dedupe_key);
            assert_eq!(e.meter, Meter::Tokens);
            assert_eq!(e.tool, "zcode");
            assert_eq!(e.counts.credits, 0.0, "money is computed centrally");
            models.insert(e.model.clone().unwrap_or_default());
            sessions.insert(e.session.clone());
            projects.insert(e.project.clone().unwrap_or_default());
            totals += &e.counts;
        }
        assert_eq!(totals, census.totals);
        // ZCode's own per-turn roll-up (`turn_usage`) is cumulative: it is never
        // an event source, but its totals are the yardstick for ours.
        let conn = rusqlite::Connection::open_with_flags(&files[0].path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let turn: (i64, i64) = conn
            .query_row("SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(cache_read_input_tokens), 0) FROM turn_usage", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        let stored_prompt: i64 = conn.query_row(&format!("SELECT COALESCE(SUM(input_tokens), 0) FROM {}", paths::USAGE_TABLE), [], |r| r.get(0)).unwrap();
        drop(conn);
        assert_eq!(totals.input as i64 + totals.cache_read as i64, stored_prompt, "Σ stored input_tokens, billed exactly once");
        // `turn_usage` is a per-turn roll-up of the same calls (and would be a
        // `Cumulative` source if we read it). Measured drift: it is 1.1% *below*
        // `model_usage`, because rows land in the per-call table as calls finish
        // and the turn rows lag, so it is a sanity bound here, not an identity.
        let stored = totals.input as i64 + totals.cache_read as i64;
        assert!(turn.0 * 10 > stored * 9, "turn_usage roll-up ({}) drifted far from model_usage ({stored})", turn.0);
        println!("[zcode] file={} bytes={:.1}MiB rows={} events={} zero_rows={} sessions={} projects={} models={:?}", files[0].path.display(), files[0].size as f64 / 1_048_576.0, census.rows, census.events, census.zero_rows, sessions.len(), projects.len(), models);
        println!("[zcode] input(uncached)={:.0} cacheRead={:.0} cacheWrite={:.0} reasoning={:.0} output={:.0} total={:.0} | stored prompt incl. cache={stored_prompt} (would be {:.2}x if cache were double-counted)", totals.input, totals.cache_read, totals.cache_creation, totals.reasoning, totals.output, totals.total(), (totals.input + 2.0 * totals.cache_read + totals.output) / totals.total());
        // Baselines measured on this machine (see the project verification note);
        // the table only ever grows, so they are floors.
        assert!(census.rows >= 18_112 && census.events >= 17_978, "rows {} events {}", census.rows, census.events);
        assert!(totals.input >= 121_150_276.0, "net input {}", totals.input);
        assert!(totals.cache_read >= 4_605_001_344.0, "cache read {}", totals.cache_read);
        assert!(totals.output >= 9_633_477.0, "output {}", totals.output);
        assert!(totals.reasoning > 0.0 && totals.reasoning < totals.output, "reasoning {} must stay a sub-breakdown of output {}", totals.reasoning, totals.output);
        assert!(totals.cache_creation == 0.0, "ZCode never reports a cache write: {}", totals.cache_creation);
        assert!(totals.cache_read > 1.0e9, "…and billions of cached ones, got {}", totals.cache_read);
    }

    /// The evidence behind the source choice: the `rollout/` buffer is a strict
    /// subset of `model_usage`, in a different id space. If it ever stops being
    /// one, reading only the db would be dropping billable calls.
    #[test]
    #[ignore = "reads ~/.zcode/cli"]
    fn the_rollout_buffer_is_contained_in_the_db() {
        let Some(dir) = paths::rollout_dir() else { return };
        let db = paths::db_path().expect("the db is present on this machine");
        let mut files = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| paths::is_rollout_file(p)).collect::<Vec<_>>();
        files.sort();
        assert!(!files.is_empty(), "no rollout files to compare against");
        let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        // The containment is proved on the token triple: the provider's
        // (uncached, cached, output) must appear as some row's
        // (input − cache_read, cache_read, output) for the same session within a
        // minute of the same instant. `trace_id` is per turn and `requestId` is not
        // the db's `logical_request_id`, so the numbers are the only shared key.
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM {usage} WHERE session_id = ?1 AND cache_read_input_tokens = ?2 AND output_tokens = ?3 AND input_tokens = ?4 AND ABS(completed_at - ?5) < 60000)",
            usage = paths::USAGE_TABLE
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let mut checked = 0;
        for path in files {
            let meta = std::fs::metadata(&path).unwrap();
            let file = SourceFile { path, kind: usage_core::FileKind::Jsonl, size: meta.len(), mtime_ms: 0 };
            for call in ZcodeAdapter.read(&file, ReadCursor(0)).unwrap().events {
                let found: bool = stmt
                    .query_row(rusqlite::params![call.session, call.counts.cache_read as i64, call.counts.output as i64, (call.counts.input + call.counts.cache_read) as i64, call.ts_ms], |r| r.get(0))
                    .unwrap_or(false);
                assert!(found, "rollout call is missing from the db: {:?} {:?}", call.dedupe_key, call.counts);
                checked += 1;
            }
        }
        println!("[zcode] rollout containment: {checked} calls verified inside model_usage");
        assert!(checked >= 151, "expected the whole buffer to be covered, got {checked}");
    }
}

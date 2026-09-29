//! AgnesCode adapter.
//!
//! Primary source: `~/.agnes/data/sessions/sessions.db`, table `usage_ledger` —
//! one row per billed model call (554 rows / 4 sessions on this machine), read as
//! [`FileKind::Sqlite`] with the highest consumed `id` (which is the rowid) as
//! cursor. Fallback: `~/.agnes/state/logs/llm/<session>/<ms>-<request>-<purpose>.jsonl`,
//! whose ~900 KB request/response streams hold exactly one `usage` record each —
//! 118 of them here, 86 of which are already inside the ledger under a different id
//! space, so the two trees are never both read ([`paths`] holds the measurements and
//! [`loader`] the cursor and locking rules).
//!
//! The ledger's `input_tokens` is a **total** prompt: `input + output =
//! total_tokens` in 554 of 554 rows with `cache_read_tokens <= input_tokens` in all
//! of them, so the cached prefix is netted back out into its own stage before the row
//! leaves this crate ([`parser`]). `cost` and `cost_source` are NULL in every row and
//! are never imported: tokenme prices centrally from the shared models.dev table.
//!
//! Owned by the agnes-adapter workstream. The public surface (`AgnesAdapter`,
//! `TOOL_ID`) follows the sibling adapters, and nothing registers this crate yet.

mod loader;
mod parser;
mod paths;

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

pub const TOOL_ID: &str = "agnes";
pub(crate) const DISPLAY_NAME: &str = "AgnesCode";

#[derive(Debug, Default, Clone, Copy)]
pub struct AgnesAdapter;

impl SourceAdapter for AgnesAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            // One row per billed call, and the row's numbers describe that call: the
            // `Cumulative` shape lives in `sessions.accumulated_*`, which is never an
            // event source.
            usage_form: usage_core::UsageForm::PerCall,
            meter: usage_core::Meter::Tokens,
            // `model` sits on the same row as the counts; no join is needed.
            model_attr: usage_core::ModelAttr::Inline,
            // `id INTEGER PRIMARY KEY AUTOINCREMENT`: stable per call, and never
            // reissued after a delete (measured `sqlite_sequence.usage_ledger = 554`).
            dedupes_by_id: true,
            // No quota window anywhere in this tree: the vendor keeps no rate-limit
            // record, and `sessions.accumulated_cost` is NULL like `usage_ledger.cost`.
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

    use rusqlite::{Connection, OpenFlags};
    use usage_core::{FileKind, Meter, ReadCursor, TokenCounts, UsageForm};

    use super::*;

    /// Reads the live `~/.agnes` ledger and checks the imported totals against a
    /// direct `SELECT` of the same columns.
    #[test]
    #[ignore = "reads ~/.agnes"]
    fn the_live_ledger_imports_exactly_what_it_selects() {
        let adapter = AgnesAdapter;
        let files = adapter.discover(&DateFilter::default());
        assert_eq!(files.len(), 1, "the ledger is the single active source: {files:?}");
        assert_eq!(files[0].kind, FileKind::Sqlite);
        assert!(files[0].path.to_string_lossy().ends_with("data/sessions/sessions.db"));

        let (outcome, census) = loader::read_ledger(&files[0].path, ReadCursor(0), &files[0].key()).unwrap();
        let events = outcome.events;
        let via_trait = adapter.read(&files[0], ReadCursor(0)).unwrap();
        assert_eq!(via_trait.events, events, "the trait path and the census path decode identically");
        assert_eq!(via_trait.cursor, outcome.cursor);

        // The same columns, summed by SQLite itself, with the netting spelled out in
        // SQL: this is the yardstick the adapter is not allowed to drift from.
        let conn = Connection::open_with_flags(&files[0].path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let direct: (i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), \
                        SUM(input_tokens - COALESCE(cache_read_tokens, 0) - COALESCE(cache_write_tokens, 0)), \
                        SUM(COALESCE(cache_read_tokens, 0)), SUM(COALESCE(cache_write_tokens, 0)), \
                        SUM(output_tokens), SUM(total_tokens) \
                 FROM usage_ledger WHERE input_tokens + COALESCE(output_tokens, 0) > 0 \
                    OR COALESCE(cache_read_tokens, 0) + COALESCE(cache_write_tokens, 0) > 0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM usage_ledger", [], |r| r.get(0)).unwrap();
        let compaction: (i64, i64) = conn
            .query_row("SELECT COUNT(*), COALESCE(SUM(total_tokens), 0) FROM usage_ledger WHERE COALESCE(is_compaction, 0) <> 0", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        let with_cost: i64 = conn.query_row("SELECT COUNT(cost) FROM usage_ledger", [], |r| r.get(0)).unwrap();
        let identities: (i64, i64) = conn
            .query_row(
                "SELECT SUM(input_tokens + COALESCE(output_tokens, 0) = total_tokens), \
                        SUM(COALESCE(cache_read_tokens, 0) + COALESCE(cache_write_tokens, 0) <= input_tokens) \
                 FROM usage_ledger",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // The vendor's own per-session roll-up of the same rows.
        let rollup: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions s JOIN (SELECT session_id, \
                        SUM(input_tokens) i, SUM(output_tokens) o, \
                        SUM(COALESCE(cache_read_tokens, 0)) c, SUM(total_tokens) t \
                 FROM usage_ledger GROUP BY session_id) l ON l.session_id = s.id \
                 WHERE s.accumulated_input_tokens = l.i AND s.accumulated_output_tokens = l.o \
                    AND s.accumulated_cache_read_tokens = l.c AND s.accumulated_total_tokens = l.t",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let sessions: i64 = conn.query_row("SELECT COUNT(DISTINCT session_id) FROM usage_ledger", [], |r| r.get(0)).unwrap();
        drop(conn);

        assert_eq!(census.rows as i64, rows, "every row of the table was visited");
        assert_eq!(census.events as i64, direct.0, "one event per row that reported a token");
        assert_eq!(census.totals.input as i64, direct.1, "Σ (input − cache_read − cache_write)");
        assert_eq!(census.totals.cache_read as i64, direct.2, "Σ cache_read_tokens");
        assert_eq!(census.totals.cache_creation as i64, direct.3, "Σ cache_write_tokens");
        assert_eq!(census.totals.output as i64, direct.4, "Σ output_tokens");
        assert_eq!(census.totals.total() as i64, direct.5, "Σ total_tokens, every stage billed once");
        assert_eq!((identities.0, identities.1), (rows, rows), "`input + output = total` and `cache <= input` on every row");
        assert_eq!(census.inclusive_violations, 0);
        assert_eq!(census.total_mismatches, 0);
        assert_eq!(census.compaction_rows as i64, compaction.0, "the is_compaction calls are imported, not filtered");
        assert_eq!(with_cost as usize, census.rows_with_cost, "cost stays where it is");
        assert_eq!(rollup, sessions, "the vendor's own `sessions.accumulated_*` roll-up equals the ledger sums we import");
        assert!(census.batches >= 2, "{} rows at BATCH_ROWS={} must take more than one page", census.rows, loader::BATCH_ROWS);
        // Idempotence on the real file, and the cursor is a rowid.
        assert_eq!(outcome.cursor, ReadCursor(census.max_id as u64));
        let again = adapter.read(&files[0], outcome.cursor).unwrap();
        assert!(again.events.is_empty() && again.cursor == outcome.cursor);

        let mut models = BTreeSet::new();
        let mut projects = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for e in &events {
            assert_eq!((e.tool.as_str(), e.meter), ("agnes", Meter::Tokens));
            assert_eq!(e.counts.credits, 0.0, "money is computed centrally");
            assert!(e.dedupe_key.as_ref().is_some_and(|k| keys.insert(k.clone())), "duplicate dedupe key: {:?}", e.dedupe_key);
            assert!(e.ts_ms > 1_700_000_000_000, "seconds promoted to ms: {}", e.ts_ms);
            models.insert(e.model.clone().unwrap_or_default());
            projects.insert(e.project.clone().unwrap_or_default());
        }
        println!(
            "[agnes] file={} bytes={:.1}MiB rows={} events={} batches={} zero_rows={} compaction_rows={} ({} tokens) sessions={} models={:?} projects={:?}",
            files[0].path.display(),
            files[0].size as f64 / 1_048_576.0,
            census.rows,
            census.events,
            census.batches,
            census.zero_rows,
            census.compaction_rows,
            compaction.1,
            sessions,
            models,
            projects
        );
        println!(
            "[agnes] input(uncached)={:.0} cacheRead={:.0} cacheWrite={:.0} output={:.0} total={:.0} | rows_with_cost={} would be {:.2}x if the cached prefix were double-counted",
            census.totals.input,
            census.totals.cache_read,
            census.totals.cache_creation,
            census.totals.output,
            census.totals.total(),
            census.rows_with_cost,
            (census.totals.input + 2.0 * census.totals.cache_read + census.totals.output) / census.totals.total()
        );
        // Baselines measured on this machine (2026-09-24); the ledger only grows, so
        // they are floors.
        assert!(census.rows >= 554 && census.events >= 554, "rows {} events {}", census.rows, census.events);
        assert!(census.totals.input >= 11_933_979.0, "net input {}", census.totals.input);
        assert!(census.totals.cache_read >= 83_684_352.0, "cache read {}", census.totals.cache_read);
        assert!(census.totals.output >= 568_292.0, "output {}", census.totals.output);
        assert_eq!(census.totals.cache_creation, 0.0, "cache_write_tokens is NULL in every row measured");
        assert!(census.totals.total() >= 96_186_623.0, "total {}", census.totals.total());
    }

    /// The fallback tree, live: how many usage records it holds and what they sum
    /// to. `#[ignore]`d because it reads ~79 MB of request logs.
    #[test]
    #[ignore = "reads ~/.agnes/state/logs"]
    fn the_fallback_logs_hold_one_usage_record_per_file() {
        let Some(logs) = paths::llm_log_dir() else { return };
        let mut totals = TokenCounts::default();
        let mut calls = 0;
        let mut keys = BTreeSet::new();
        let mut files = 0;
        for path in loader::log_entries(&logs) {
            files += 1;
            let meta = std::fs::metadata(&path).unwrap();
            let file = SourceFile { path, kind: FileKind::Jsonl, size: meta.len(), mtime_ms: 0 };
            let out = AgnesAdapter.read(&file, ReadCursor(0)).unwrap();
            for e in out.events {
                assert!(keys.insert(e.dedupe_key.clone().unwrap()), "one request id, one record");
                totals += &e.counts;
                calls += 1;
            }
        }
        println!("[agnes] fallback tree: {files} log files, {calls} usage records, {:.0} tokens, {} distinct request ids", totals.total(), keys.len());
        assert_eq!(files, 125, "measured on this machine");
        assert_eq!(calls, 118, "measured: 118 usage records across 125 log files");
        assert_eq!(keys.len(), 118);
        // Our total() is the disjoint sum of the stages, i.e. the vendor's own
        // input + output once the cached prefix has been netted and re-added.
        assert_eq!(totals.total(), 7_690_349.0, "Σ (input_tokens + output_tokens) over those records");
        assert_eq!(totals.input, 1_060_883.0, "…and only that much of it was a fresh prompt");
        assert_eq!(totals.cache_read, 6_571_264.0, "85% of the fallback tree's prompt tokens were cache reads");
        assert_eq!(totals.output, 58_202.0);
    }

    #[test]
    fn public_surface_is_frozen() {
        let adapter = AgnesAdapter;
        assert_eq!(TOOL_ID, "agnes");
        assert_eq!(adapter.id(), "agnes");
        assert_eq!(adapter.display_name(), "AgnesCode");
        assert_eq!(
            adapter.semantics(),
            Semantics {
                usage_form: UsageForm::PerCall,
                meter: Meter::Tokens,
                model_attr: usage_core::ModelAttr::Inline,
                dedupes_by_id: true,
                reports_quota: false,
            }
        );
        assert_eq!(adapter.semantics(), Semantics::TOKENS_PER_CALL_INLINE, "the shared shape this source matches exactly");
        let cloned: AgnesAdapter = Default::default();
        assert_eq!(cloned.id(), adapter.id(), "Copy + Default derive");
        assert!(format!("{cloned:?}").contains("AgnesAdapter"));
        // No quota server, no cached auth file: the default is the answer.
        assert!(adapter.quota().is_empty());
    }
}

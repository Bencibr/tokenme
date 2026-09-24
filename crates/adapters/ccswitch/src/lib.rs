//! CC Switch adapter — the local proxy's **own** accounting ledger as a source.
//!
//! Source: `~/.cc-switch/cc-switch.db`, table `proxy_request_logs` (SQLite,
//! [`FileKind::Sqlite`], cursor = highest consumed `rowid`). Honouring the app's own
//! path overrides: `CC_SWITCH_TEST_HOME` and the in-app data-directory setting.
//!
//! CC Switch sits in front of Claude Code / Codex / Gemini CLI / OpenCode / Pi /
//! Grok Build / Mcode, switches providers on failure, and meters what actually
//! crossed the wire. `Semantics` therefore says `PerCall / Tokens / Inline` — one
//! row per billed call, model on the same row — with `dedupes_by_id` on the
//! vendor's `request_id TEXT PRIMARY KEY` and `reports_quota` false until the
//! per-provider spend limits (`providers.limit_daily_usd`, which **do** exist here)
//! are read in a follow-up task.
//!
//! # What is additive, and where double counting stops
//!
//! Every row carries `data_source` (`database/schema.rs:213`). Two families live in
//! this one table:
//!
//! * `proxy` — the gateway watched this call itself. `session_log_sync` shows why
//!   the rest do not: the same call is *also* in the tool's own JSONL.
//! * `<tool>_session` / `session_log` — CC Switch walked the tool's own session log
//!   and re-derived the call (`services/session_usage.rs:1-8` reads
//!   `~/.claude/projects`, `session_usage_codex.rs:1592` the Codex tree, and so on;
//!   the file list it used is still in `session_log_sync`). For `claude`, `codex`,
//!   `opencode` and `pi` tokenme has a native adapter for exactly those files, so
//!   these rows are a second copy of events the sibling adapters already emit:
//!   [`loader::MIRRORED_DATA_SOURCES`] drops them, which on this machine is 85,011
//!   of 92,754 rows. Mirrors of tools tokenme *cannot* read (`gemini_session`,
//!   `grok_session`, `mcode_session`) stay — this table is their only source.
//!
//! `session_usage_dedup` is not the cross-adapter mechanism it looks like: all
//! 16,946 of its rows are `pi_session`, and it exists so Pi's own re-import does
//! not double count rows the 30-day prune has already deleted
//! (`services/session_usage_pi.rs:36-49,785-796`, `database/schema.rs:320-331`).
//! The vendor's cross-source gate is `data_source` plus a token-fingerprint match
//! against a live `proxy` row within ±10 minutes
//! (`services/usage_stats.rs:225,307-343,364-373`), and it is what guarantees a
//! mirror and a proxied row for one call never coexist in the first place.
//!
//! # Money
//!
//! The five `*_cost_usd` columns are TEXT decimals computed by the vendor from its
//! own `model_pricing` table (`proxy/usage/calculator.rs`), scaled by
//! `providers.cost_multiplier` and keyed on `pricing_model` rather than `model`. None
//! of it reaches an event — tokenme prices every stage centrally — but the sum is
//! kept in [`loader::Census::vendor_cost_usd`] precisely so the two independent
//! pricing paths can be diffed against each other.

mod loader;
mod parser;
mod paths;

use usage_core::{
    DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

pub const TOOL_ID: &str = "ccswitch";
pub(crate) const DISPLAY_NAME: &str = "CC Switch (网关)";

#[derive(Debug, Default, Clone, Copy)]
pub struct CcSwitchAdapter;

impl SourceAdapter for CcSwitchAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn semantics(&self) -> Semantics {
        // A derived view of proxied traffic: one row per call that crossed the
        // gateway, model on that row, `request_id` as its identity, and no quota
        // reported until the spend-limit follow-up lands.
        Semantics::TOKENS_PER_CALL_INLINE
    }

    fn probe(&self) -> Option<DetectedSource> {
        let db = paths::db_path()?;
        paths::stat_file(&db)?;
        // One query, one scan: total rows, how many of them are the gateway's own
        // measurements, and which routed tools appear at all.
        let conn = paths::open_readonly(&db, paths::USABLE_SQL).ok()?;
        let total = loader::count_rows(&conn).unwrap_or(0);
        let mix = loader::source_mix(&conn).unwrap_or_default();
        let primary = mix
            .iter()
            .filter(|(source, _, _)| source == loader::SOURCE_PROXY)
            .map(|(_, _, rows)| *rows)
            .sum::<i64>();
        let mut apps: Vec<&str> = mix.iter().map(|(_, app, _)| app.as_str()).collect();
        apps.sort_unstable();
        apps.dedup();
        if total == 0 {
            return None;
        }
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: vec![db],
            hint: Some(format!("{total} rows · {primary} proxied · apps: {}", apps.join(","))),
        })
    }

    fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
        // One database holds every period, and the 30-day prune rewrites it in
        // place, so `DateFilter` cannot prune the listing: the rowid cursor is what
        // bounds the read.
        let Some(db) = paths::db_path() else { return Vec::new() };
        let Some((size, mtime_ms)) = paths::stat_file(&db) else { return Vec::new() };
        vec![SourceFile { path: db, kind: FileKind::Sqlite, size, mtime_ms }]
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        // Deliberately infallible: CC Switch owns this database while serving
        // traffic, and a locked ledger must not abort the pass for other sources.
        Ok(loader::read(&file.path, cursor, &file.key(), false)?.0)
    }
}

#[cfg(test)]
mod smoke {
    use std::collections::BTreeMap;

    use rusqlite::Connection;
    use usage_core::{DateFilter, FileKind, Meter, PricingMap, ReadCursor, TokenCounts};

    use super::*;

    /// Full-file mirror of the live `~/.cc-switch/cc-switch.db`, built by the
    /// hermetic fixture in [`loader::tests`].
    #[test]
    fn the_adapter_reads_a_real_shaped_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let db = loader::tests::fixture(dir.path());
        let meta = std::fs::metadata(&db).unwrap();
        let file = SourceFile { path: db, kind: FileKind::Sqlite, size: meta.len(), mtime_ms: 0 };
        let adapter = CcSwitchAdapter;
        assert_eq!(adapter.id(), "ccswitch");
        assert_eq!(adapter.display_name(), "CC Switch (网关)");
        assert_eq!(adapter.semantics(), Semantics::TOKENS_PER_CALL_INLINE);
        let outcome = adapter.read(&file, ReadCursor(0)).unwrap();
        assert_eq!(outcome.events.len(), 6);
        assert_eq!(outcome.cursor, ReadCursor(11));
        assert_eq!(adapter.read(&file, outcome.cursor).unwrap().events.len(), 0, "idempotent");
        assert_eq!(adapter.discover(&DateFilter::default()).len(), 1);
    }

    /// The real 262 MB ledger: totals, the traffic mix, and the one number nobody
    /// can fake — CC Switch's own USD against what tokenme's price table makes of
    /// the same netted tokens.
    #[test]
    #[ignore = "reads ~/.cc-switch/cc-switch.db"]
    fn the_live_gateway_ledger_and_its_own_price_sheet() {
        let adapter = CcSwitchAdapter;
        let source = adapter.probe().expect("CC Switch is installed and its ledger is queryable");
        println!("[ccswitch] probe {:?}", source.roots);
        println!("[ccswitch] hint = {:?}", source.hint);
        let files = adapter.discover(&DateFilter::default());
        assert_eq!(files.len(), 1, "one ledger: {files:?}");
        assert_eq!(files[0].kind, FileKind::Sqlite);

        let (outcome, census) = loader::read(&files[0].path, ReadCursor(0), &files[0].key(), true).unwrap();
        assert_eq!(outcome.events.len(), census.events);
        // Cursor discipline on the real file: a second pass adds nothing.
        let again = adapter.read(&files[0], outcome.cursor).unwrap();
        assert!(again.events.is_empty() && again.cursor == outcome.cursor, "{:?}", again.cursor);

        let mut models = 0;
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        let mut totals = TokenCounts::default();
        for e in &outcome.events {
            assert_eq!(e.tool, "ccswitch");
            assert_eq!(e.meter, Meter::Tokens);
            assert_eq!(e.counts.credits, 0.0, "money is computed centrally");
            assert!(e.dedupe_key.is_some(), "request_id is the vendor's primary key");
            models += usize::from(e.model.is_some());
            *seen.entry(e.dedupe_key.as_deref().unwrap()).or_default() += 1;
            totals += &e.counts;
        }
        assert!(seen.values().all(|n| *n == 1), "duplicate dedupe key: {seen:?}");

        let conn = Connection::open_with_flags(&files[0].path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let total_rows = loader::count_rows(&conn).unwrap();
        let mix = loader::source_mix(&conn).unwrap();
        drop(conn);
        println!(
            "[ccswitch] {} MiB · rows={total_rows} visited={} batches={} → events={} mirrored={} zero={} non-2xx={} guard-fails={}",
            files[0].size / 1_048_576,
            census.rows,
            census.batches,
            census.events,
            census.mirrored,
            census.zero_rows,
            census.non_success,
            census.semantics_guards_failed
        );
        println!("[ccswitch] data_source mix (all rows): {mix:?}");
        println!("[ccswitch] app_types emitted: {:?}", census.app_types);
        println!("[ccswitch] distinct models={models} sessions={}", outcome.events.iter().map(|e| e.session.as_str()).collect::<std::collections::BTreeSet<_>>().len());
        println!(
            "[ccswitch] tokens input(uncached)={:.0} cacheWrite={:.0} cacheRead={:.0} reasoning={:.0} output={:.0} total={:.0}",
            totals.input, totals.cache_creation, totals.cache_read, totals.reasoning, totals.output, totals.total()
        );
        assert_eq!(totals, census.totals);
        // A gateway that only fronts tools tokenme reads directly contributes
        // **nothing**, and that is the correct answer rather than a failure: measured
        // on this machine, all 92,754 ledger rows are either a re-imported session log
        // or a proxied call that Claude Code / Codex / OpenCode / Pi already billed in
        // its own log. Emitting them here would have double-counted 1.45 B tokens.
        if census.events == 0 {
            println!("[ccswitch] 0 events: every row duplicates a source tokenme reads directly");
        }
        assert!(
            census.mirrored + census.events > 0,
            "the ledger had {} rows and none were classified: {:?}",
            census.rows,
            census.sources
        );
        assert_eq!(
            census.mirrored + census.zero_rows + census.events,
            census.rows,
            "every visited row is mirrored, unbilled or emitted: {:?}",
            census.sources
        );

        // ---- the cross-check: two independent pricing paths, same rows ----
        if census.costed.is_empty() {
            println!("[ccswitch] cost cross-check skipped: no row survived the dedupe gate to price");
            assert!(total_rows > 50_000, "{total_rows} rows: is this the real ledger?");
            return;
        }
        let price_table = PricingMap::load(&usage_core::pricing::PricingOptions::default());
        println!("[ccswitch] pricing source={:?} keys={}", price_table.meta().source, price_table.key_count());
        let mut ours = 0.0;
        let mut theirs = 0.0;
        let mut comparable = 0;
        let mut unpriced = 0;
        for (model, counts, their_cost, multiplier) in &census.costed {
            let Some(base) = model.as_deref().and_then(|m| price_table.cost(Some(m), counts)) else {
                unpriced += 1;
                continue;
            };
            // The vendor's `total_cost_usd` is base cost × `cost_multiplier`
            // (`services/usage_stats.rs:1902-1929`), so ours must be scaled too.
            let k = multiplier.trim().parse::<f64>().unwrap_or(1.0);
            ours += base * k;
            theirs += their_cost;
            comparable += 1;
        }
        let delta = if theirs > 0.0 { (ours - theirs) / theirs * 100.0 } else { f64::NAN };
        println!(
            "[ccswitch] cost cross-check: comparable rows={comparable} unpriced-by-us={unpriced} multiplied={}",
            census.multiplied_rows
        );
        println!("[ccswitch]   CC Switch total_cost_usd = ${theirs:.6}");
        println!("[ccswitch]   tokenme price table     = ${ours:.6}   → delta {delta:+.3}%");
        assert!(comparable > 0, "at least one proxied row must be priceable by both sides");
        assert!(delta.is_finite(), "the delta must be computable: ours={ours} theirs={theirs}");

        // History-shape assertions, floors only: the ledger grows and prunes.
        assert!(total_rows > 50_000, "{total_rows} rows: is this the real ledger?");
        assert!(census.mirrored > 50_000, "the session mirrors are the bulk of the table: {:?}", census.sources);
    }
}

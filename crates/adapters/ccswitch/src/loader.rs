//! Batched read-only ingestion of `proxy_request_logs`, with the double-count
//! gate and the vendor's own row lifecycle documented.
//!
//! ## Cursor
//!
//! `cursor.0` is the highest consumed `rowid`. The table has no INTEGER PRIMARY
//! KEY, so its `request_id TEXT PRIMARY KEY` (`database/schema.rs:199`) is a
//! *unique index*, not the rowid: rows keep an ordinary monotonically assigned
//! rowid and that is what we page over.
//!
//! Four of the vendor's write paths matter for that choice:
//!
//! * live traffic is appended (`proxy/usage/logger.rs:167-206`, `INSERT OR
//!   IGNORE`) and a byte-identical repeat of a call is dropped before it reaches
//!   SQL (`:139-146`), so appends dominate;
//! * money is backfilled with `UPDATE … WHERE request_id = ?`
//!   (`services/usage_stats.rs:1944-1960`) — the rowid survives, and the money
//!   columns are never read into an event anyway;
//! * one path *moves* a row: when a proxied call arrives whose `request_id` was
//!   already taken by a session-log import, the vendor rewrites it with `INSERT OR
//!   REPLACE` (`logger.rs:161-166`), which allocates a new rowid. That is exactly
//!   the transition we want to see twice — the row goes from `session_log` (a
//!   mirror of a file tokenme already reads, hence gated out) to `proxy` (the
//!   gateway's own measurement, hence billed) — and re-emitting it is harmless
//!   because the event keeps the same `dedupe_key`, which the indexer's
//!   `UNIQUE INDEX event_dedupe` + `INSERT OR IGNORE` fold away;
//! * history *disappears* underneath the cursor: after 30 days the detail rows are
//!   aggregated into `usage_daily_rollups` and deleted
//!   (`database/dao/usage_rollup.rs:62,177`, run from `database/mod.rs:155`), so
//!   this source is a rolling 30-day window by construction. A cursor past the row
//!   space (a restored backup, or a prune that took the newest row) is therefore
//!   not an error here: it is clamped, unlike the zcode adapter which reports one.
//!
//! ## Why the batches are drained inside one `read`
//!
//! `usage-index` stops batching as soon as a pass produces no *events*, and on this
//! ledger 85 of every 92 thousand rows produce nothing but the mirror gate. Reading
//! a single batch per call would therefore park the cursor inside the mirrored
//! block, so one call walks batches until one comes back short.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, Row};
use usage_core::{Error, ReadCursor, ReadOutcome, TokenCounts};

use crate::parser::{self, Row as UsageRow};
use crate::paths;

/// Rows per query. The live ledger holds 92,754 of them, so a pass is 47 batches.
pub(crate) const BATCH_ROWS: i64 = 2000;

/// Safety valve, so a ledger being rewritten under us cannot hold one pass open
/// forever (`BATCH_ROWS * MAX_BATCHES` rows, i.e. 400k here).
const MAX_BATCHES: usize = 200;

/// `data_source` of a row the gateway itself saw on the wire (`schema.rs:213`,
/// where it is also the column default; `data_source_expr` in
/// `services/usage_stats.rs:232` treats a NULL the same way for pre-v9 files).
pub(crate) const SOURCE_PROXY: &str = "proxy";

/// The `data_source` values that are CC Switch **re-importing a tool's own session
/// log** — `session_log` from `~/.claude/projects` (`services/session_usage.rs:1-8`,
/// whose `file_path`s are still listed in `session_log_sync`), `codex_session`
/// (`session_usage_codex.rs:1592`), `opencode_session` (`session_usage_opencode.rs:430`)
/// and `pi_session` (`session_usage_pi.rs:24`). Every one of those files is already
/// a tokenme source in its own right, so those rows are the *same calls*: taking
/// them here too would bill them twice. `session_usage_dedup`'s 16,946 rows are all
/// `pi_session`, i.e. the same verdict from the vendor's own ledger.
///
/// What is deliberately absent: `gemini_session`, `grok_session`, `mcode_session`
/// and any future importer for a tool tokenme cannot read — for those this table is
/// the only place their usage exists, so they stay.
pub(crate) const MIRRORED_DATA_SOURCES: &[&str] = &["session_log", "codex_session", "opencode_session", "pi_session"];

/// The `app_type`s of the tools tokenme reads **directly**, i.e. whose own logs
/// already carry the same calls. The gateway saw these on the wire, but that does
/// not make them extra spend: a proxied Claude call is written to
/// `~/.claude/projects` by Claude Code itself, so billing the `proxy` row as well
/// would count it twice. Measured here: 7,743 `proxy` rows, all `app_type = claude`,
/// 1.45 B tokens — the same traffic the `claude` adapter already emits.
///
/// Anything the gateway routes that tokenme cannot read (`gemini`, `grokbuild`,
/// `mcode`, …) stays: for that traffic this ledger is the only source there is.
pub(crate) const NATIVE_APP_TYPES: &[&str] = &["claude", "codex", "opencode", "pi"];

/// Whether a row duplicates a call a sibling adapter already bills.
pub(crate) fn is_mirrored(data_source: &str) -> bool {
    MIRRORED_DATA_SOURCES.contains(&data_source)
}

/// The whole double-counting gate: a re-imported session log, or a proxied call
/// whose tool tokenme watches itself.
pub(crate) fn is_duplicate(data_source: &str, app_type: &str) -> bool {
    is_mirrored(data_source)
        || (data_source == SOURCE_PROXY && NATIVE_APP_TYPES.contains(&app_type))
}

/// `SELECT` of every usage column this adapter needs, plus the two money columns
/// the cross-check needs, with `data_source` NULL-normalised the vendor's way.
const SELECT: &str = "SELECT rowid, request_id, provider_id, app_type, \
     COALESCE(data_source, 'proxy'), model, request_model, session_id, created_at, \
     status_code, input_token_semantics, input_tokens, cache_read_tokens, \
     cache_creation_tokens, output_tokens, total_cost_usd, cost_multiplier \
     FROM proxy_request_logs WHERE rowid > ?1 ORDER BY rowid LIMIT ?2";

/// Row census plus the vendor's own money, both kept out of the events.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Census {
    /// Rows the cursor paged over, before any gate.
    pub rows: usize,
    /// `fetch_batch` calls it took to get there — non-zero proof that the batches
    /// really were walked rather than assumed.
    pub batches: usize,
    /// Events produced.
    pub events: usize,
    /// Rows dropped by [`is_mirrored`]: re-imports of files tokenme already reads.
    pub mirrored: usize,
    /// Rows with every token stage at 0 (failed or usage-less requests).
    pub zero_rows: usize,
    /// Emitted rows with a non-2xx `status_code`: a provider that billed a call it
    /// still answered with an error.
    pub non_success: usize,
    /// Emitted rows whose `input_token_semantics` guard failed, so the cached
    /// prefix could not be netted (see [`parser::net_input`]).
    pub semantics_guards_failed: usize,
    /// `data_source` → rows visited, so a smoke run can name the traffic mix.
    pub sources: BTreeMap<String, usize>,
    /// `app_type` → rows emitted.
    pub app_types: BTreeMap<String, usize>,
    /// Highest `rowid` visited, i.e. the new cursor.
    pub max_rowid: i64,
    pub totals: TokenCounts,
    /// Σ `total_cost_usd` over the emitted rows; the pricing cross-check.
    pub vendor_cost_usd: f64,
    /// Emitted rows whose `cost_multiplier` is not 1 — their vendor cost is then
    /// not comparable with a plain per-token price.
    pub multiplied_rows: usize,
    /// `(model, netted counts, vendor total_cost_usd, cost_multiplier)` per emitted
    /// row. Only filled when `read` is called with `collect_costs`.
    pub costed: Vec<(Option<String>, TokenCounts, f64, String)>,
}

/// Read every `proxy_request_logs` row appended after `cursor`.
///
/// Deliberately infallible: CC Switch owns this database and keeps it open while
/// serving traffic, so a lock, a schema migration in flight or a half-written
/// journal all mean "nothing this pass", never "abort the other sources".
pub(crate) fn read(path: &Path, cursor: ReadCursor, source_key: &str, collect_costs: bool) -> Result<(ReadOutcome, Census), Error> {
    read_with(path, cursor, source_key, collect_costs, BATCH_ROWS)
}

fn read_with(path: &Path, cursor: ReadCursor, source_key: &str, collect_costs: bool, batch: i64) -> Result<(ReadOutcome, Census), Error> {
    let conn = match paths::open_readonly(path, paths::USABLE_SQL) {
        Ok(conn) => conn,
        Err(_) => return Ok((ReadOutcome { events: Vec::new(), cursor }, Census::default())),
    };
    let mut events = Vec::new();
    let mut census = Census::default();
    let mut here = i64::try_from(cursor.0).unwrap_or(i64::MAX);
    if let Some(max) = max_rowid(&conn) {
        here = here.min(max);
    }

    for _ in 0..MAX_BATCHES {
        let rows = match fetch_batch(&conn, here, batch) {
            Ok(rows) => rows,
            // Busy, locked, or a column this build has not added yet: keep the
            // events already decoded and park the cursor where we got to.
            Err(_) => break,
        };
        census.batches += 1;
        let short = (rows.len() as i64) < batch;
        for row in rows {
            census.rows += 1;
            here = here.max(row.rowid);
            *census.sources.entry(row.data_source.clone()).or_default() += 1;
            if is_duplicate(&row.data_source, &row.app_type) {
                census.mirrored += 1;
                continue;
            }
            let counts = parser::token_counts(&row);
            let Some(event) = parser::build_event(&row, source_key) else {
                census.zero_rows += 1;
                continue;
            };
            census.events += 1;
            census.totals += &counts;
            *census.app_types.entry(row.app_type.clone()).or_default() += 1;
            census.non_success += usize::from(!(200..300).contains(&row.status_code));
            census.semantics_guards_failed += usize::from(parser::net_input(&row).1);
            census.vendor_cost_usd += row.vendor_cost_usd;
            census.multiplied_rows += usize::from(!is_neutral_multiplier(&row.cost_multiplier));
            if collect_costs {
                census.costed.push((event.model.clone(), counts, row.vendor_cost_usd, row.cost_multiplier));
            }
            events.push(event);
        }
        census.max_rowid = here;
        if short {
            break;
        }
    }

    Ok((ReadOutcome { events, cursor: ReadCursor(u64::try_from(here).unwrap_or(u64::MAX)) }, census))
}

/// `cost_multiplier` is TEXT in the vendor's schema (`schema.rs:212`) and `'1'` or
/// `'1.0'` in every live row here.
fn is_neutral_multiplier(raw: &str) -> bool {
    raw.trim().parse::<f64>().is_ok_and(|v| (v - 1.0).abs() < 1e-9)
}

fn fetch_batch(conn: &Connection, after: i64, limit: i64) -> rusqlite::Result<Vec<UsageRow>> {
    let mut stmt = conn.prepare_cached(SELECT)?;
    let rows = stmt.query_map(rusqlite::params![after, limit], take_row)?;
    rows.collect()
}

/// `Option<>` on every column even though the vendor declares most of them
/// `NOT NULL`: a restored pre-migration file (the live data dir keeps `.bak-*`
/// copies of the whole database) can hand back a NULL, and one unexpected value
/// must not abort the ingest pass for every other source.
fn take_row(row: &Row<'_>) -> rusqlite::Result<UsageRow> {
    let text = |i: usize| row.get::<_, Option<String>>(i).unwrap_or(None).unwrap_or_default();
    let int = |i: usize| row.get::<_, Option<i64>>(i).unwrap_or(None).unwrap_or(0);
    Ok(UsageRow {
        rowid: row.get(0)?,
        request_id: text(1),
        provider_id: text(2),
        app_type: text(3),
        data_source: {
            let s = text(4);
            if s.is_empty() { SOURCE_PROXY.to_string() } else { s }
        },
        model: text(5),
        request_model: text(6),
        session_id: text(7),
        created_at: int(8),
        status_code: int(9),
        semantics: int(10),
        input: int(11),
        cache_read: int(12),
        cache_creation: int(13),
        output: int(14),
        vendor_cost_usd: text(15).trim().parse().unwrap_or(0.0),
        cost_multiplier: text(16),
    })
}

/// `max(rowid)` is a btree seek, not a scan.
fn max_rowid(conn: &Connection) -> Option<i64> {
    conn.query_row(&format!("SELECT max(rowid) FROM {}", paths::TABLE), [], |r| r.get::<_, Option<i64>>(0))
        .ok()
        .flatten()
}

/// Total rows in the ledger, for `probe`'s hint.
pub(crate) fn count_rows(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(&format!("SELECT count(*) FROM {}", paths::TABLE), [], |r| r.get(0))
}

/// Traffic mix of the ledger: `(data_source, app_type, rows)`, busiest first. Used
/// by `probe` to name what the gateway is actually watching.
pub(crate) fn source_mix(conn: &Connection) -> rusqlite::Result<Vec<(String, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(data_source, 'proxy') AS ds, app_type, count(*) FROM proxy_request_logs \
         GROUP BY ds, app_type ORDER BY 3 DESC",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use rusqlite::Connection;
    use usage_core::UsageEvent;

    use super::*;

    /// `CREATE TABLE` copied out of the live `~/.cc-switch/cc-switch.db` with
    /// `.schema proxy_request_logs`, column order included: the vendor added
    /// `input_token_semantics` **last** via ALTER in migration v13 (`schema.rs:739`
    /// vs the fresh-install DDL at `:205`), so a real file does not match the
    /// in-code DDL and this fixture must not either.
    const DDL: &str = "CREATE TABLE proxy_request_logs (
            request_id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, app_type TEXT NOT NULL, model TEXT NOT NULL,
            request_model TEXT,
            pricing_model TEXT,
            input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
            input_cost_usd TEXT NOT NULL DEFAULT '0', output_cost_usd TEXT NOT NULL DEFAULT '0',
            cache_read_cost_usd TEXT NOT NULL DEFAULT '0', cache_creation_cost_usd TEXT NOT NULL DEFAULT '0',
            total_cost_usd TEXT NOT NULL DEFAULT '0', latency_ms INTEGER NOT NULL, first_token_ms INTEGER,
            duration_ms INTEGER, status_code INTEGER NOT NULL, error_message TEXT, session_id TEXT,
            provider_type TEXT, is_streaming INTEGER NOT NULL DEFAULT 0,
            cost_multiplier TEXT NOT NULL DEFAULT '1.0', created_at INTEGER NOT NULL,
            data_source TEXT NOT NULL DEFAULT 'proxy'
        , \"input_token_semantics\" INTEGER NOT NULL DEFAULT 0);";

    /// The vendor's own column list, so a row built here is shaped like a row the
    /// app wrote (`proxy/usage/logger.rs:169-176`).
    const INSERT: &str = "INSERT INTO proxy_request_logs (
            request_id, provider_id, app_type, model, request_model, pricing_model,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
            input_token_semantics,
            input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd,
            latency_ms, first_token_ms, status_code, error_message, session_id,
            provider_type, is_streaming, cost_multiplier, created_at, data_source
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26)";

    /// A seed shaped like one `log_request` call.
    #[derive(Debug, Clone)]
    struct Seed {
        id: &'static str,
        app: &'static str,
        source: &'static str,
        semantics: i64,
        input: i64,
        read: i64,
        write: i64,
        output: i64,
        cost: &'static str,
        status: i64,
        multiplier: &'static str,
        /// `None` writes a SQL NULL, which several live paths do.
        session: Option<&'static str>,
        created_at: i64,
    }

    impl Seed {
        fn new(id: &'static str, app: &'static str, source: &'static str, input: i64, read: i64, output: i64) -> Self {
            Self {
                id,
                app,
                source,
                semantics: parser::SEMANTICS_FRESH,
                input,
                read,
                write: 0,
                output,
                cost: "0",
                status: 200,
                multiplier: "1.0",
                session: Some("s-generated"),
                created_at: 1_790_165_270,
            }
        }
    }

    fn seed(conn: &Connection, s: &Seed) {
        conn.execute(
            INSERT,
            rusqlite::params![
                s.id,
                format!("_{}", s.source),
                s.app,
                format!("{}-model", s.app),
                "",
                "",
                s.input,
                s.output,
                s.read,
                s.write,
                s.semantics,
                "0",
                "0",
                "0",
                "0",
                s.cost,
                12i64,
                3i64,
                s.status,
                None::<String>,
                s.session,
                "openai",
                1i64,
                s.multiplier,
                s.created_at,
                s.source
            ],
        )
        .unwrap();
    }

    /// The representative mix, one shape per live quirk: proxied Claude traffic
    /// (FRESH input), mirrors of tools tokenme reads itself, mirrors of tools it
    /// cannot see, the only TOTAL-semantics row, a non-2xx row that still billed,
    /// an all-zero failure, a superseded `request_id`, and NULLs.
    pub(crate) fn fixture(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("cc-switch.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        let seeds: Vec<Seed> = vec![
            // 1 proxied Claude call, live shape: 1941 fresh input beside 179264
            //   cached, exactly like `glm-5.3-flash`'s rows in the real ledger.
            Seed { cost: "0.42", session: Some("e533282f"), ..Seed::new("session:msg_aaaa", "grokbuild", "proxy", 1941, 179_264, 5877) },
            // 2 Codex mirror, LEGACY stamp, the real row's numbers.
            Seed { semantics: parser::SEMANTICS_LEGACY, cost: "1.5", created_at: 1_790_165_280, ..Seed::new("codex_session:thread-v1:01a02e2d:461", "codex", "codex_session", 179_580, 178_944, 41) },
            // 3 Gemini mirror: tokenme has no Gemini adapter, so it stays — and it
            //   is the only TOTAL row, so both cache stages come off the input.
            Seed { semantics: parser::SEMANTICS_TOTAL, write: 2000, cost: "0.01", created_at: 1_790_165_290, ..Seed::new("gemini_session:sess-7:2", "gemini", "gemini_session", 10_000, 6000, 300) },
            // 4 proxied Claude call the vendor itself priced at nothing.
            Seed { created_at: 1_790_165_300, ..Seed::new("session:msg_bbbb", "grokbuild", "proxy", 100, 0, 20) },
            // 5 a 429 that still reported usage, on a 2.5x provider.
            Seed { status: 429, cost: "0.0001", multiplier: "2.5", created_at: 1_790_165_310, ..Seed::new("session:msg_cccc", "grokbuild", "proxy", 10, 0, 5) },
            // 6 a 500 with no usage at all.
            Seed { status: 500, created_at: 1_790_165_320, ..Seed::new("session:msg_dddd", "grokbuild", "proxy", 0, 0, 0) },
            // 7 Pi mirror, NULL session_id (its importer keys off the entry, not a session).
            Seed { session: None, cost: "0.002", created_at: 1_790_165_330, ..Seed::new("pi_session:9e09b943b6fa", "pi", "pi_session", 1503, 58_368, 155) },
            // 8/9 the same call twice as the vendor stores it: 8 is the session-log
            //   import, 9 is the live proxied row. `request_id` is the PRIMARY KEY, so
            //   the gateway overwrites with `INSERT OR REPLACE` (`logger.rs:161-166`)
            //   and two rows can only coexist under distinct ids — which is exactly the
            //   case the mirror gate has to catch: the `session_log` row is dropped and
            //   the `proxy` row bills.
            Seed { semantics: parser::SEMANTICS_LEGACY, cost: "0.1", created_at: 1_790_165_340, ..Seed::new("session_log:msg_eeee", "claude", "session_log", 900, 100, 30) },
            Seed { cost: "0.12", created_at: 1_790_165_345, ..Seed::new("session:msg_eeee", "grokbuild", "proxy", 800, 100, 25) },
            // 9b the same Claude call as the gateway saw it: Claude Code's own
            //   transcript already carries it, so this row must never be billed.
            Seed { cost: "0.2", created_at: 1_790_165_346, ..Seed::new("session:msg_ff00", "claude", "proxy", 5000, 1000, 90) },
            // 10 an `mcode` mirror: no tokenme adapter, NULL session_id, FRESH.
            Seed { session: None, cost: "0.03", created_at: 1_790_165_350, ..Seed::new("mcode_session:thread-9:3", "mcode", "mcode_session", 500, 400, 60) },
        ];
        for s in &seeds {
            seed(&conn, s);
        }
        drop(conn);
        path
    }

    fn read_all(path: &Path, cursor: ReadCursor) -> (Vec<UsageEvent>, Census) {
        let (outcome, census) = read(path, cursor, "key", true).unwrap();
        (outcome.events, census)
    }

    fn keys(events: &[UsageEvent]) -> Vec<Option<String>> {
        events.iter().map(|e| e.dedupe_key.clone()).collect()
    }

    #[test]
    fn the_mirror_gate_keeps_double_counting_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let (events, census) = read_all(&path, ReadCursor(0));
        assert_eq!(census.rows, 11, "every row was visited");
        assert_eq!(
            census.mirrored, 4,
            "the claude/codex/pi session mirrors, plus the gateway's own copy of a proxied Claude call"
        );
        assert_eq!(census.zero_rows, 1, "the 500 with no usage is not billable");
        assert_eq!(census.events, 6, "{:?}", keys(&events));
        let got = keys(&events);
        assert!(!got.contains(&Some("ccswitch#codex_session:thread-v1:01a02e2d:461".into())), "{got:?}");
        assert!(!got.contains(&Some("ccswitch#pi_session:9e09b943b6fa".into())), "{got:?}");
        assert!(!got.contains(&Some("ccswitch#session:msg_dddd".into())), "a zero-usage failure is not a call");
        // Kept, because no tokenme adapter can see this traffic:
        assert!(got.contains(&Some("ccswitch#gemini_session:sess-7:2".into())), "{got:?}");
        assert!(got.contains(&Some("ccswitch#mcode_session:thread-9:3".into())), "{got:?}");
        assert_eq!(census.app_types.get("codex"), None, "routed codex traffic is not re-billed");
        assert_eq!(census.app_types.get("pi"), None);
        assert_eq!(census.app_types.get("claude"), None, "proxied Claude traffic is not re-billed");
        assert_eq!(census.app_types.get("grokbuild"), Some(&4), "a routed tool with no adapter of its own stays");
        assert_eq!(census.app_types.get("gemini"), Some(&1));
        assert_eq!(census.sources["session_log"], 1, "the superseded import is still visible in the census");
        assert_eq!(census.sources["proxy"], 6);
        // `is_mirrored` is the single place that decides, keyed on the vendor's own
        // provenance column rather than on `app_type`.
        assert!(MIRRORED_DATA_SOURCES.iter().all(|s| is_mirrored(s)));
        assert!(!is_mirrored(SOURCE_PROXY), "wire truth always stays");
        // ... but the wire is only a source for tools tokenme cannot read itself.
        assert!(is_duplicate(SOURCE_PROXY, "claude"), "Claude Code logs its own calls");
        assert!(is_duplicate(SOURCE_PROXY, "codex") && is_duplicate(SOURCE_PROXY, "opencode") && is_duplicate(SOURCE_PROXY, "pi"));
        assert!(!is_duplicate(SOURCE_PROXY, "grokbuild") && !is_duplicate(SOURCE_PROXY, "gemini"));
        assert!(!is_mirrored("grok_session") && !is_mirrored("gemini_session") && !is_mirrored("mcode_session"));
    }

    #[test]
    fn stages_are_netted_per_the_vendors_own_semantics_enum() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let (events, census) = read_all(&path, ReadCursor(0));
        let by_id = |id: &str| events.iter().find(|e| e.dedupe_key.as_deref() == Some(id)).unwrap().clone();
        // FRESH (2): `input_tokens` is already the uncached remainder.
        let claude = by_id("ccswitch#session:msg_aaaa");
        assert_eq!(claude.counts.input, 1941.0);
        assert_eq!(claude.counts.cache_read, 179_264.0);
        assert_eq!(claude.counts.total(), 1941.0 + 179_264.0 + 5877.0, "the cached prefix is billed once");
        assert_eq!(claude.counts.reasoning, 0.0, "the vendor records no reasoning stage at all");
        // TOTAL (1): both cache stages come off, line for line `fresh_input_sql`.
        let gemini = by_id("ccswitch#gemini_session:sess-7:2");
        assert_eq!(gemini.counts.input, 2000.0, "10000 - 6000 read - 2000 write");
        assert_eq!(gemini.counts.cache_creation, 2000.0);
        assert_eq!(gemini.counts.total(), 10_300.0, "Σ of the row's own columns, no double count");
        // The same call as seen twice: the mirror gate drops the 900-token import.
        let replaced = events.iter().filter(|e| e.dedupe_key.as_deref() == Some("ccswitch#session:msg_eeee")).collect::<Vec<_>>();
        assert_eq!(replaced.len(), 1, "one event per request_id even though the row moved: {:?}", keys(&events));
        assert_eq!(replaced[0].counts.input, 800.0);
        assert_eq!(replaced[0].counts.total(), 925.0);
        assert_eq!(census.semantics_guards_failed, 0, "no row in this fixture breaks a guard");
        assert_eq!(
            census.totals,
            TokenCounts { input: 5351.0, cache_creation: 2000.0, cache_read: 185_764.0, output: 6287.0, reasoning: 0.0, credits: 0.0 }
        );
    }

    #[test]
    fn an_inclusive_row_whose_guard_fails_is_reported_not_invented() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cc-switch.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        // grokbuild is cache-inclusive and TOTAL here, but `input >= read + write`
        // does not hold, so the vendor's CASE falls through to the raw column.
        seed(&conn, &Seed { semantics: parser::SEMANTICS_TOTAL, ..Seed::new("grok_session:t:1", "grokbuild", "grok_session", 100, 400, 10) });
        drop(conn);
        let (events, census) = read_all(&path, ReadCursor(0));
        assert_eq!(events.len(), 1, "grokbuild has no tokenme adapter, so its mirror stays");
        assert_eq!(events[0].counts.input, 100.0, "never negative, never silently netted");
        assert_eq!(census.semantics_guards_failed, 1);
    }

    #[test]
    fn event_shape_is_the_gateway_not_the_routed_tool() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let (events, _) = read_all(&path, ReadCursor(0));
        let e = events.iter().find(|e| e.dedupe_key.as_deref() == Some("ccswitch#session:msg_aaaa")).unwrap();
        assert_eq!(e.tool, crate::TOOL_ID, "stays `ccswitch`, so the panel gets its own row");
        assert_eq!(e.project.as_deref(), Some("grokbuild/_proxy"), "app_type/provider_id ride along here");
        assert_eq!(e.session, "msg_aaaa", "the id that names the owning Claude message");
        assert_eq!(e.model.as_deref(), Some("grokbuild-model"));
        assert_eq!(e.ts_ms, 1_790_165_270_000, "created_at is seconds and must be promoted");
        assert_eq!(e.source, "key#proxy_request_logs#1");
        assert_eq!(e.meter, usage_core::Meter::Tokens);
        // Rows 7 and 10 have a NULL session_id, so the request_id has to speak.
        let mcode = events.iter().find(|e| e.dedupe_key.as_deref() == Some("ccswitch#mcode_session:thread-9:3")).unwrap();
        assert_eq!(mcode.session, "thread-9");
        assert_eq!(mcode.project.as_deref(), Some("mcode/_mcode_session"));
        let gemini = events.iter().find(|e| e.dedupe_key.as_deref() == Some("ccswitch#gemini_session:sess-7:2")).unwrap();
        assert_eq!(gemini.session, "sess-7", "the thread, not the trailing call index");
        // The 429 still bills the tokens it reported.
        let failed = events.iter().find(|e| e.dedupe_key.as_deref() == Some("ccswitch#session:msg_cccc")).unwrap();
        assert_eq!(failed.counts.total(), 15.0);
        for e in &events {
            assert_eq!(e.counts.credits, 0.0, "vendor money never becomes a bill: {e:?}");
            assert!(e.quota.is_none());
        }
    }

    #[test]
    fn the_cursor_pages_in_batches_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        // Three rows per query: 10 rows + the short batch that ends it = 4 batches
        // walked inside a single pass, which is the point of the internal loop.
        let (outcome, census) = read_with(&path, ReadCursor(0), "key", false, 3).unwrap();
        assert_eq!(census.batches, 4, "the batches really were walked: {:?}", census.sources.keys().count());
        assert_eq!(census.rows, 11);
        assert_eq!(outcome.cursor, ReadCursor(11), "highest consumed rowid, not a row count");
        assert_eq!(outcome.events.len(), 6);
        // Resuming from that cursor learns nothing, twice over.
        for _ in 0..2 {
            let again = read(&path, outcome.cursor, "key", false).unwrap();
            assert!(again.0.events.is_empty() && again.0.cursor == outcome.cursor, "{:?}", again.0);
        }
        // A second pass over the tail replays the same keys, never new ones.
        let tail = read(&path, ReadCursor(4), "key", false).unwrap();
        assert_eq!(tail.0.cursor, ReadCursor(11));
        assert_eq!(keys(&tail.0.events), keys(&outcome.events)[3..], "rows 5..10 continue where 1..4 stopped");
        // Two half passes stitched together equal the whole table.
        let head = read_with(&path, ReadCursor(0), "key", false, 4).unwrap();
        assert!(head.0.cursor.0 >= 4, "a batch-sized stop is a cursor, not an error: {:?}", head.0.cursor);
        let rest = read(&path, head.0.cursor, "key", false).unwrap();
        let mut stitched = keys(&head.0.events);
        stitched.extend(keys(&rest.0.events));
        assert_eq!(stitched, keys(&outcome.events), "no event lost or doubled across the seam");
        // A cursor past the (prunable) row space is clamped, not an error.
        let beyond = read(&path, ReadCursor(9999), "key", false).unwrap();
        assert_eq!(beyond.0.cursor, ReadCursor(11), "a restored or pruned ledger is business as usual");
        assert!(beyond.0.events.is_empty());
    }

    #[test]
    fn a_short_batch_is_the_stop_signal() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(fetch_batch(&conn, 0, BATCH_ROWS).unwrap().len(), 11, "one full batch takes the lot");
        assert!(fetch_batch(&conn, 11, BATCH_ROWS).unwrap().is_empty(), "nothing past the last rowid");
        assert_eq!(fetch_batch(&conn, 7, 3).unwrap().iter().map(|r| r.rowid).collect::<Vec<_>>(), vec![8, 9, 10]);
        let ids: Vec<String> = fetch_batch(&conn, 0, BATCH_ROWS).unwrap().into_iter().map(|r| r.request_id).collect();
        assert_eq!(
            ids.iter().filter(|i| i.ends_with("msg_eeee")).count(),
            2,
            "the import and the live proxy row coexist as two rowids with distinct keys"
        );
    }

    #[test]
    fn a_locked_or_foreign_or_malformed_database_degrades_to_ok() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file.
        let missing = dir.path().join("nope.db");
        let (outcome, census) = read(&missing, ReadCursor(7), "key", false).unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(7));
        assert_eq!(census, Census::default());
        // Valid db without our table (CC Switch keeps its config beside it).
        let foreign = dir.path().join("other.db");
        let conn = Connection::open(&foreign).unwrap();
        conn.execute_batch("CREATE TABLE providers (id TEXT PRIMARY KEY, settings_config TEXT);").unwrap();
        drop(conn);
        let (outcome, census) = read(&foreign, ReadCursor(3), "key", false).unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(3), "not our ledger");
        assert_eq!(census, Census::default());
        // Empty table: probe-able, but nothing to emit and the cursor holds.
        let empty = dir.path().join("empty.db");
        let conn = Connection::open(&empty).unwrap();
        conn.execute_batch(DDL).unwrap();
        drop(conn);
        let (outcome, census) = read(&empty, ReadCursor(0), "key", false).unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(0));
        assert_eq!(census.rows, 0, "USABLE_SQL must not fail on an empty table");
        assert_eq!(census.batches, 1, "one short batch, which is also the stop signal");
        assert_eq!(census.events, 0);
        // Bytes that are not a database at all.
        let garbage = dir.path().join("garbage.db");
        std::fs::write(&garbage, b"not sqlite, not even close").unwrap();
        assert!(read(&garbage, ReadCursor(0), "key", false).unwrap().0.events.is_empty(), "a torn file is nothing, not an error");
    }

    #[test]
    fn null_and_garbage_columns_degrade_instead_of_aborting_the_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        conn.execute(
            "INSERT INTO proxy_request_logs (request_id, provider_id, app_type, model, input_tokens, \
             output_tokens, cache_read_tokens, cache_creation_tokens, input_token_semantics, total_cost_usd, \
             latency_ms, status_code, created_at, cost_multiplier, data_source, session_id) \
             VALUES ('', '', 'grokbuild', '', 0, 5, 'x', 0, 9, 'not-a-number', 1, 500, 1790165270, '', 'proxy', NULL)",
            [],
        )
        .unwrap();
        // A row whose `data_source` is empty, i.e. the vendor's own column default.
        conn.execute(
            "INSERT INTO proxy_request_logs (request_id, provider_id, app_type, model, input_tokens, output_tokens, \
             total_cost_usd, latency_ms, status_code, created_at, cost_multiplier, data_source) \
             VALUES ('legacy-null-source', 'p', 'grokbuild', 'gpt-5.6-luna', 900, 30, '0.5', 3, 200, 1790165280, '1', '')",
            [],
        )
        .unwrap();
        drop(conn);
        let (events, census) = read_all(&path, ReadCursor(0));
        assert_eq!(census.rows, 2);
        assert_eq!(events.len(), 2, "NULLs are 0/None, they do not abort the pass");
        assert_eq!(census.mirrored, 0, "an empty source is not one of the named mirrors");
        assert_eq!(census.sources["proxy"], 2, "the empty source normalises onto the gateway's own row");
        let junk = &events[0];
        assert_eq!(junk.dedupe_key.as_deref(), Some("ccswitch#row1"), "an empty request_id falls back to the rowid");
        assert_eq!(junk.session, "ccswitch-row1", "no session_id and no request_id to read it from");
        assert_eq!(junk.project.as_deref(), Some("grokbuild"), "empty provider drops the separator, app stays");
        assert_eq!(junk.model, None, "an empty model stays unknown; request_model is empty too");
        assert_eq!(junk.counts, TokenCounts { input: 0.0, cache_creation: 0.0, cache_read: 0.0, output: 5.0, reasoning: 0.0, credits: 0.0 });
        assert_eq!(census.vendor_cost_usd, 0.5, "the unparseable cell is 0, the parseable one still counts");
        // `is_neutral_multiplier` only excuses a multiplier that parses to 1.0, so the
        // empty one is flagged as unproven while codex's literal "1" is not.
        assert_eq!(census.multiplied_rows, 1, "an unparseable multiplier is not assumed neutral");
        assert_eq!(census.non_success, 1, "a 500 that still billed is counted, not dropped");
        // Row 2 is an *older* pre-v9 shape: no semantics column value, so 0 (LEGACY),
        // and its reads column is empty, so the stored 900 is the billed input.
        assert_eq!(events[1].counts.input, 900.0);
        assert_eq!(events[1].dedupe_key.as_deref(), Some("ccswitch#legacy-null-source"));
    }

    #[test]
    fn reads_while_a_writer_holds_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        // The running app: an exclusive write transaction, uncommitted.
        let writer = Connection::open(&path).unwrap();
        writer.execute("BEGIN EXCLUSIVE", []).unwrap();
        let outcome = read(&path, ReadCursor(0), "key", false).unwrap();
        assert_eq!(outcome.0.events.len(), 6, "a rollback-journal reader is never blocked");
        assert_eq!(outcome.0.cursor, ReadCursor(11));
        seed(&writer, &Seed { created_at: 1_790_165_400, ..Seed::new("session:msg_new", "grokbuild", "proxy", 50, 0, 7) });
        assert_eq!(read(&path, ReadCursor(0), "key", false).unwrap().0.events.len(), 6, "an uncommitted row stays invisible");
        writer.execute("COMMIT", []).unwrap();
        drop(writer);
        // Rowid 11 is the mcode mirror and 12 the committed row; the proxied Claude
        // call sits at 10 and is the one the gate drops.
        let after = read(&path, ReadCursor(11), "key", false).unwrap();
        assert_eq!(after.0.events.len(), 1, "the row the app just committed is picked up");
        assert_eq!(after.0.cursor, ReadCursor(12));
        assert_eq!(keys(&after.0.events), vec![Some("ccswitch#session:msg_new".to_string())]);
    }

    #[test]
    fn vendor_money_is_measured_but_never_returned_as_a_bill() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let (events, census) = read_all(&path, ReadCursor(0));
        assert_eq!(census.costed.len(), 6);
        // 0.42 + 0.01 + 0 + 0.0001 + 0.12 + 0.03, over exactly the emitted rows.
        assert!((census.vendor_cost_usd - 0.5801).abs() < 1e-9, "{:?}", census.vendor_cost_usd);
        assert_eq!(census.non_success, 1, "the 429 that still reported usage");
        assert_eq!(census.multiplied_rows, 1, "that row's cost_multiplier is 2.5");
        assert!(events.iter().all(|e| e.counts.credits == 0.0));
        // The cross-check sample has what it needs: netted stages and their money.
        let (model, counts, cost, multiplier) = &census.costed[0];
        assert_eq!(model.as_deref(), Some("grokbuild-model"));
        assert_eq!(counts.input, 1941.0, "netted before the comparison, as the price table will see it");
        assert!((*cost - 0.42).abs() < 1e-9);
        assert_eq!(multiplier, "1.0");
        assert_eq!(count_rows(&Connection::open(&path).unwrap()).unwrap(), 11);
        let mix = source_mix(&Connection::open(&path).unwrap()).unwrap();
        assert_eq!(mix.len(), 7, "one entry per (data_source, app_type) pair: {mix:?}");
        assert_eq!(mix[0].2, 5, "the proxied rows of the unreadable tool are the busiest");
    }
}

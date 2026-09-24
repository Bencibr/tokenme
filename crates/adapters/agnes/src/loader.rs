//! Walk + read for both AgnesCode trees, with exactly one of them active.
//!
//! ## Cursor: the highest consumed `id`, which is the rowid
//! [`usage_core::FileKind::Sqlite`] means the cursor is a rowid and
//! `usage_ledger.id` is `INTEGER PRIMARY KEY AUTOINCREMENT`, so the two are the
//! same column: ids 1..=554 contiguous on this machine, `sqlite_sequence` at 554.
//! `read` pages `WHERE id > cursor ORDER BY id LIMIT batch`, so a pass costs
//! `rows/batch` seeks rather than one full materialisation, and the ceiling is
//! pinned to the `MAX(id)` measured when the pass started — a ledger being appended
//! to while we read can never make this loop chase forever.
//!
//! A cursor ahead of `MAX(id)` is **clamped, not an error** (the zcode adapter
//! reports one): `usage_ledger.session_id … ON DELETE CASCADE` means deleting a
//! session deletes its ledger rows, so a cursor can legitimately end up past the
//! newest row after a prune, a restore, or a `VACUUM`. Nothing is re-emitted by the
//! clamp — the next batch starts above every id that exists — so the price of not
//! erroring is one empty pass.
//!
//! ## A lock is never an error either
//! AgnesCode owns this database and keeps it open while it works, so every failure
//! to open or read it is reported as "nothing this pass" with the cursor frozen
//! where it was ([`crate::paths::open_ledger`] falls back to a private temp copy for
//! exactly that reason) and a batch that errors mid-scan keeps the events already
//! decoded and parks the cursor at the last row actually consumed. The only `Err`
//! this module produces is the fallback's cursor-past-EOF, where the manifest itself
//! is wrong.
//!
//! ## Exactly one tree is active
//! `discover` hands out the db when it exists and the `state/logs/llm/**/*.jsonl`
//! request logs only when it does not: the logs are a per-call debug buffer holding
//! 118 usage records against the ledger's 554, they carry the same calls under a
//! different id space (`<session>#<request_id>` vs `<session>#<ledger id>`), and each
//! file is ~900 KB of prompt/response echo for one line of usage. Reading both would
//! double-bill the overlap and the index could not fold it — see [`crate::paths`].
//! What the fallback loses is reported by [`Census`] and documented in the module:
//! the 30 `tool_pair_summary` and 2 `session_naming` usage records on this machine
//! are internal helper calls the vendor never entered in its own ledger, so the
//! fallback reports a few calls the ledger omits.

use std::io::{BufRead, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OptionalExtension, Row as SqlRow};
use usage_core::{DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, SourceFile, TokenCounts};

use crate::parser::{self, Head, Row};
use crate::paths::{self, USAGE_TABLE};

/// Ledger rows per query. The live table holds 554, so a full pass is two batches.
pub(crate) const BATCH_ROWS: i64 = 500;

/// Safety valve: `BATCH_ROWS * MAX_BATCHES` rows per `read`, i.e. 12.5 k here, well
/// above anything one ingest pass of this ledger needs. The indexer calls `read`
/// again as long as the cursor keeps moving, so a full pass is never lost — only
/// spread out.
const MAX_BATCHES: usize = 25;

/// Line buffer ceiling for the fallback logs: a single record embeds a whole
/// streamed response and has been seen at 300 KB.
const READ_BUF: usize = 256 * 1024;

/// What one pass over the ledger actually saw, so a test can diff the events
/// against the table itself instead of against a hand-tuned expectation.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Census {
    /// Rows paged over, before any gate.
    pub rows: usize,
    /// Queries it took — non-zero proof the batches were really walked.
    pub batches: usize,
    /// Events produced.
    pub events: usize,
    /// Rows with every stage at 0, which visit but never bill.
    pub zero_rows: usize,
    /// Rows with `is_compaction` set, imported by decision (see [`crate::parser`]).
    pub compaction_rows: usize,
    /// Rows where `input_tokens < cache_read + cache_write`, i.e. the inclusive
    /// convention [`parser::net_input`] rests on broke down. 0 of 554 measured.
    pub inclusive_violations: usize,
    /// Rows whose netted stages do not reproduce the vendor's `total_tokens`.
    pub total_mismatches: usize,
    /// Rows with `cost IS NOT NULL` — 0 of 554 measured, and imported never.
    pub rows_with_cost: usize,
    /// Rows with `cost_source IS NOT NULL`.
    pub rows_with_cost_source: usize,
    /// Highest `id` visited, i.e. the cursor a pass ends on.
    pub max_id: i64,
    /// Σ of the emitted events' stages.
    pub totals: TokenCounts,
}

pub fn probe() -> Option<DetectedSource> {
    let root = paths::root()?;
    if let Some(db) = paths::db_path() {
        return Some(DetectedSource {
            id: crate::TOOL_ID.to_string(),
            display: crate::DISPLAY_NAME.to_string(),
            roots: vec![root],
            hint: Some(format!("usage_ledger db ({})", human_size(&db))),
        });
    }
    let logs = paths::llm_log_dir()?;
    // Short-circuits on the first usable file: `probe` runs at startup.
    let first = log_entries(&logs).next()?;
    Some(DetectedSource {
        id: crate::TOOL_ID.to_string(),
        display: crate::DISPLAY_NAME.to_string(),
        roots: vec![root],
        hint: Some(format!("llm request logs, fallback ({}…)", first.file_name().and_then(|n| n.to_str()).unwrap_or("?"))),
    })
}

pub fn discover(filter: &DateFilter) -> Vec<SourceFile> {
    let mut out = Vec::new();
    if let Some(db) = paths::db_path() {
        // The ledger is one append-only file for every session, so `DateFilter` can
        // only prune it by mtime; the exact cut happens on the event stream.
        push_file(&mut out, &db, FileKind::Sqlite, filter);
        return out;
    }
    let Some(logs) = paths::llm_log_dir() else { return out };
    for path in log_entries(&logs) {
        push_file(&mut out, &path, FileKind::Jsonl, filter);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

pub fn read(file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
    match file.kind {
        FileKind::Sqlite => read_ledger(&file.path, cursor, &file.key()).map(|(outcome, _)| outcome),
        _ => read_log(file, cursor),
    }
}

/// Read every `usage_ledger` row appended after `cursor`.
///
/// Deliberately infallible for a source this adapter does not own: a locked,
/// half-migrated or foreign database yields an empty outcome with the cursor
/// untouched, which is what "not now" has to look like to the indexer.
pub(crate) fn read_ledger(path: &Path, cursor: ReadCursor, source_key: &str) -> Result<(ReadOutcome, Census), Error> {
    read_ledger_with(path, cursor, source_key, BATCH_ROWS)
}

fn read_ledger_with(path: &Path, cursor: ReadCursor, source_key: &str, batch: i64) -> Result<(ReadOutcome, Census), Error> {
    let Some(ledger) = paths::open_ledger(path) else {
        return Ok((ReadOutcome { events: Vec::new(), cursor }, Census::default()));
    };
    let conn = ledger.conn();
    let max_id = match scalar(conn, &format!("SELECT COALESCE(MAX(id), 0) FROM {USAGE_TABLE}")) {
        Ok(v) => v,
        Err(_) => return Ok((ReadOutcome { events: Vec::new(), cursor }, Census::default())),
    };
    // Clamped, never an error: see the module docs. A cursor above the row space is
    // a prune or a restore, and the next batch simply starts above every id there is.
    let ceiling = max_id;
    let mut here = i64::try_from(cursor.0).unwrap_or(i64::MAX).min(ceiling);
    let sql = select_sql(conn);
    let mut events = Vec::new();
    let mut census = Census::default();
    for _ in 0..MAX_BATCHES {
        let rows = match fetch_batch(conn, &sql, here, ceiling, batch) {
            Ok(rows) => rows,
            // Busy, locked, or a column this build has not added yet: keep what was
            // already decoded and park the cursor at the last row consumed.
            Err(_) => break,
        };
        census.batches += 1;
        let short = (rows.len() as i64) < batch;
        for row in rows {
            census.rows += 1;
            here = here.max(row.id);
            let counts = parser::stages(&row);
            let flags = parser::flags(&row, &counts);
            census.zero_rows += usize::from(flags.zero);
            census.compaction_rows += usize::from(flags.compaction);
            census.inclusive_violations += usize::from(flags.inclusive_violation);
            census.total_mismatches += usize::from(flags.total_mismatch);
            census.rows_with_cost += usize::from(row.cost.is_some());
            census.rows_with_cost_source += usize::from(row.cost_source.is_some());
            let Some(event) = parser::event_from_row(&row, &source_for(source_key, row.id)) else { continue };
            census.events += 1;
            census.totals += &counts;
            events.push(event);
        }
        census.max_id = here;
        if short {
            break;
        }
    }
    Ok((ReadOutcome { events, cursor: ReadCursor(u64::try_from(here).unwrap_or(u64::MAX)) }, census))
}

/// The batched `SELECT`, with the `working_dir` join present only when the
/// `sessions` table is: a ledger row is billable without a project label.
///
/// `id` *is* the rowid here (`INTEGER PRIMARY KEY`), so the paging predicate and the
/// cursor speak the same column and no `rowid` alias is needed.
pub(crate) fn select_sql(conn: &Connection) -> String {
    let columns = "id, session_id, created_timestamp, model, input_tokens, output_tokens, \
                   total_tokens, cache_read_tokens, cache_write_tokens, cost, cost_source, \
                   is_compaction";
    if table_exists(conn, paths::SESSIONS_TABLE) {
        format!(
            "SELECT l.id, l.session_id, l.created_timestamp, l.model, l.input_tokens, \
             l.output_tokens, l.total_tokens, l.cache_read_tokens, l.cache_write_tokens, \
             l.cost, l.cost_source, l.is_compaction, s.working_dir \
             FROM {USAGE_TABLE} l LEFT JOIN {} s ON s.id = l.session_id \
             WHERE l.id > ?1 AND l.id <= ?2 ORDER BY l.id LIMIT ?3",
            paths::SESSIONS_TABLE
        )
    } else {
        format!(
            "SELECT {columns}, NULL AS working_dir FROM {USAGE_TABLE} \
             WHERE id > ?1 AND id <= ?2 ORDER BY id LIMIT ?3"
        )
    }
}

/// One page of the ledger, newest-last, strictly inside `(after, ceiling]`.
fn fetch_batch(conn: &Connection, sql: &str, after: i64, ceiling: i64, limit: i64) -> rusqlite::Result<Vec<Row>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(rusqlite::params![after, ceiling, limit], take_row)?;
    rows.collect()
}

fn take_row(row: &SqlRow<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: row.get::<_, i64>("id")?,
        session_id: row.get::<_, Option<String>>("session_id")?,
        created_timestamp: row.get::<_, Option<i64>>("created_timestamp")?,
        model: row.get::<_, Option<String>>("model")?,
        input_tokens: row.get::<_, Option<i64>>("input_tokens")?,
        output_tokens: row.get::<_, Option<i64>>("output_tokens")?,
        total_tokens: row.get::<_, Option<i64>>("total_tokens")?,
        cache_read_tokens: row.get::<_, Option<i64>>("cache_read_tokens")?,
        cache_write_tokens: row.get::<_, Option<i64>>("cache_write_tokens")?,
        cost: row.get::<_, Option<f64>>("cost")?,
        cost_source: row.get::<_, Option<String>>("cost_source")?,
        is_compaction: row.get::<_, Option<i64>>("is_compaction")?,
        working_dir: row.get::<_, Option<String>>("working_dir")?,
    })
}

/// `event.source` is the manifest key of the origin plus the row, so a ledger that
/// loses rows can purge exactly its own stale events.
fn source_for(source_key: &str, id: i64) -> String {
    format!("{source_key}#{USAGE_TABLE}#{id}")
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .and_then(|mut s| s.query_row([name], |_| Ok(())).optional())
        .unwrap_or(None)
        .is_some()
}

fn scalar(conn: &Connection, sql: &str) -> rusqlite::Result<i64> {
    conn.query_row(sql, [], |r| r.get::<_, i64>(0))
}

/// Sequential JSONL read of one fallback log: `[cursor, last complete line]`.
fn read_log(file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
    let key = file.key();
    let mut handle = match std::fs::File::open(&file.path) {
        Ok(f) => f,
        Err(_) => return Ok(ReadOutcome { events: Vec::new(), cursor }),
    };
    let len = match handle.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Ok(ReadOutcome { events: Vec::new(), cursor }),
    };
    if cursor.0 > len {
        return Err(Error::Cursor { path: file.path.clone(), cursor: cursor.0 });
    }
    if cursor.0 == len {
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    }
    if handle.seek(SeekFrom::Start(cursor.0)).is_err() {
        return Ok(ReadOutcome { events: Vec::new(), cursor });
    }
    // The file name is what a resumed (cursor > 0) read still knows; a `meta` record
    // in the window overrides it.
    let mut head = Head::from_path(&file.path);
    let mut reader = BufReader::with_capacity(READ_BUF, (&mut handle).take(len - cursor.0));
    let mut events = Vec::new();
    let mut consumed = 0_u64;
    let mut buf = Vec::with_capacity(16 * 1024);
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if buf.last() != Some(&b'\n') {
            // Torn tail: the writer is mid-record. Stop before it so the cursor only
            // ever lands on a line boundary.
            break;
        }
        consumed += n as u64;
        buf.pop();
        let text = String::from_utf8_lossy(&buf);
        if let Some(event) = parser::fold_line(&text, &mut head, &key) {
            events.push(event);
        }
    }
    Ok(ReadOutcome { events, cursor: ReadCursor(cursor.0 + consumed) })
}

/// Every `llm/**.jsonl`. The tree is exactly `llm/<session id>/<file>.jsonl`, so a
/// bounded two-level scan replaces a recursive walk (and drops the `walkdir`
/// dependency this crate does not have); loose files directly under `llm/`, if a
/// build ever writes them, are picked up too.
pub(crate) fn log_entries(dir: &Path) -> impl Iterator<Item = PathBuf> {
    log_files_in(dir).into_iter().chain(
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .flat_map(|e| {
                let path = e.path();
                log_files_in(&path)
            }),
    )
}

fn log_files_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| paths::is_llm_log_file(p))
        .collect()
}

fn push_file(out: &mut Vec<SourceFile>, path: &Path, kind: FileKind, filter: &DateFilter) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    if !meta.is_file() {
        return;
    }
    let mtime = mtime_ms(&meta);
    // Both trees carry per-call timestamps inside the records, so mtime is only a
    // cheap upper bound: a file written before `until_ms` cannot hold a newer call.
    if filter.until_ms.is_some_and(|until| mtime > until) {
        return;
    }
    out.push(SourceFile { path: path.to_path_buf(), kind, size: meta.len(), mtime_ms: mtime });
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn human_size(path: &Path) -> String {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
}

/// Every ledger column this adapter reads, with the vendor's own nullability.
#[cfg(test)]
pub(crate) const DDL: &str = "CREATE TABLE usage_ledger (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                created_timestamp INTEGER NOT NULL,
                model TEXT,
                input_tokens INTEGER,
                output_tokens INTEGER,
                total_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                cost REAL,
                cost_source TEXT,
                is_compaction INTEGER DEFAULT 0
            );";

/// The `INSERT` matching [`DDL`]: `NULL` written out where the live table keeps
/// `NULL` (`cache_write_tokens`, `cost`, `cost_source` are NULL in all 554 rows).
#[cfg(test)]
pub(crate) const INSERT: &str = "INSERT INTO usage_ledger \
            (session_id, created_timestamp, model, input_tokens, output_tokens, total_tokens, \
             cache_read_tokens, cache_write_tokens, cost, cost_source, is_compaction) \
            VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, ?);";

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use usage_core::UsageEvent;

    use super::*;

    /// A ledger seeded from the rows measured on this machine, in the same order:
    /// the first two are the real ids 1 and 2, id 3 is the real compaction row (86),
    /// ids 4-5 are two of the five real `cache_read_tokens IS NULL` rows, id 6 is a
    /// zero-token row of the shape the vendor leaves for an aborted call, and ids
    /// 7-8 close out the paging case.
    pub(crate) fn fixture_db(dir: &Path) -> PathBuf {
        let path = dir.join("sessions.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        for seed in SEEDS {
            conn.execute(INSERT, rusqlite::params![seed.session, seed.ts, seed.model, seed.input, seed.output, seed.total, seed.read, seed.compaction])
                .unwrap();
        }
        // A project label lives on the session, exactly like the vendor's schema.
        conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, working_dir TEXT NOT NULL); INSERT INTO sessions VALUES ('20260806_1', '/Users/dev/workspace/fucai'), ('20260902_1', '/Users/dev/.agnes/temporary/2026-09-02/20260902_1/work');")
            .unwrap();
        drop(conn);
        path
    }

    #[derive(Clone, Copy)]
    struct Seed {
        session: &'static str,
        ts: i64,
        model: &'static str,
        input: i64,
        read: Option<i64>,
        output: i64,
        total: i64,
        compaction: i64,
    }

    const SEEDS: &[Seed] = &[
        Seed { session: "20260806_1", ts: 1_786_011_974, model: "agnes-2.5-flash", input: 17402, read: Some(17152), output: 169, total: 17571, compaction: 0 },
        Seed { session: "20260806_1", ts: 1_786_011_982, model: "agnes-2.5-flash", input: 21611, read: Some(17152), output: 661, total: 22272, compaction: 0 },
        Seed { session: "20260806_1", ts: 1_786_017_623, model: "agnes-2.0-flash", input: 96430, read: None, output: 2556, total: 98986, compaction: 1 },
        Seed { session: "20260902_1", ts: 1_788_348_486_108 / 1000, model: "agnes-2.5-flash", input: 58464, read: None, output: 146, total: 58610, compaction: 0 },
        Seed { session: "20260902_1", ts: 1_788_349_029, model: "agnes-2.5-flash", input: 2442, read: Some(1024), output: 200, total: 2642, compaction: 0 },
        // The zero row: an aborted call the vendor still ledgered.
        Seed { session: "20260902_1", ts: 1_788_349_030, model: "agnes-2.5-flash", input: 0, read: Some(0), output: 0, total: 0, compaction: 0 },
        Seed { session: "20260902_1", ts: 1_788_349_040, model: "agnes-2.5-flash", input: 32336, read: None, output: 549, total: 32885, compaction: 0 },
        Seed { session: "missing_session", ts: 1_788_349_044, model: "agnes-2.5-flash", input: 1000, read: Some(256), output: 40, total: 1040, compaction: 0 },
    ];

    fn read_all(path: &Path, cursor: ReadCursor) -> (Vec<UsageEvent>, Census) {
        let (outcome, census) = read_ledger(path, cursor, "db").unwrap();
        (outcome.events, census)
    }

    #[test]
    fn the_ledger_pages_in_batches_and_the_cursor_is_the_highest_consumed_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());
        // One batch of 500 would take all 8 rows at once; force the paging.
        let (first, census) = read_ledger_with(&path, ReadCursor(0), "db", 3).unwrap();
        assert_eq!(census.batches, 3, "3 + 3 + 2: {census:?}");
        assert_eq!(census.rows, 8, "every row was visited");
        assert_eq!(census.events, 7);
        assert_eq!(census.max_id, 8);
        let events = first.events;
        assert_eq!(events.iter().map(|e| e.source.clone()).collect::<Vec<_>>(), vec![
            "db#usage_ledger#1",
            "db#usage_ledger#2",
            "db#usage_ledger#3",
            "db#usage_ledger#4",
            "db#usage_ledger#5",
            "db#usage_ledger#7",
            "db#usage_ledger#8"
        ]);
        // Idempotence: resuming from the returned cursor consumes nothing new.
        let again = read_ledger_with(&path, events_cursor(&path), "db", 3).unwrap().0;
        assert!(again.events.is_empty(), "{:?}", again.events);
        assert_eq!(again.cursor, ReadCursor(8), "the cursor stays on the highest rowid");
        // A resume from the middle skips exactly what was already consumed.
        let (tail, census) = read_all(&path, ReadCursor(5));
        assert_eq!(census.rows, 3, "rows 6, 7, 8");
        assert_eq!(tail.len(), 2, "row 6 is the zero-token one");
        assert_eq!(tail[0].source, "db#usage_ledger#7");
        assert_eq!(census.max_id, 8);
        // A cursor past the row space is a prune or a restore: clamped, empty, no
        // error and nothing re-emitted.
        let (gone, census) = read_all(&path, ReadCursor(99));
        assert!(gone.is_empty() && census.rows == 0 && census.batches == 1);
    }

    /// The cursor the first pass hands back, read straight from the source.
    fn events_cursor(path: &Path) -> ReadCursor {
        read_ledger(path, ReadCursor(0), "db").unwrap().0.cursor
    }

    #[test]
    fn stages_are_netted_and_the_census_matches_a_direct_select() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());
        let (events, census) = read_all(&path, ReadCursor(0));
        assert_eq!(census.inclusive_violations, 0, "input_tokens >= cache_read on every row");
        assert_eq!(census.total_mismatches, 0, "the netted stages reproduce the vendor's total_tokens");
        assert_eq!(census.compaction_rows, 1, "the is_compaction row is visited and imported");
        assert_eq!(census.zero_rows, 1);
        assert_eq!(census.rows_with_cost, 0);
        assert_eq!(census.rows_with_cost_source, 0);
        // Direct SQL over the same columns, with the same netting spelled out.
        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let (input, read, output, total): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT SUM(input_tokens - COALESCE(cache_read_tokens, 0) - COALESCE(cache_write_tokens, 0)), \
                 SUM(COALESCE(cache_read_tokens, 0)), SUM(output_tokens), SUM(total_tokens) \
                 FROM usage_ledger WHERE input_tokens + output_tokens > 0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        drop(conn);
        assert_eq!(census.totals.input as i64, input);
        assert_eq!(census.totals.cache_read as i64, read);
        assert_eq!(census.totals.output as i64, output);
        assert_eq!(census.totals.total() as i64, total, "Σ total_tokens, and no stage counted twice");
        assert_eq!(events.len(), 7);
        assert_eq!(events[0].counts.input, 250.0, "17,402 − 17,152 cached");
        assert_eq!(events[2].model.as_deref(), Some("agnes-2.0-flash"));
        assert_eq!(events[6].project, None, "a row whose session is gone still bills, unlabelled");
        assert_eq!(events[3].project.as_deref(), Some("work"), "a throwaway session labels by the last component of its working_dir");
    }

    #[test]
    fn a_zero_token_row_visits_but_never_bills() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());
        let (events, census) = read_all(&path, ReadCursor(0));
        assert_eq!(census.rows, 8);
        assert_eq!(census.events, 7);
        assert_eq!(census.zero_rows, 1);
        assert!(!events.iter().any(|e| e.source.ends_with("#6")), "the zero row is not an event");
        assert!(events.iter().all(|e| !e.counts.is_zero()));
        // The zero row still consumes its id: a later pass never sees it.
        let (after, _) = read_all(&path, ReadCursor(6));
        assert_eq!(after.len(), 2, "rows 7 and 8 only");
    }

    #[test]
    fn a_null_cost_is_tolerated_and_a_real_one_is_still_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());
        let (events, census) = read_all(&path, ReadCursor(0));
        assert_eq!(census.rows_with_cost, 0, "cost is NULL in all 8 rows, exactly like the live 554");
        assert_eq!(census.events, 7, "a NULL money column bills nothing and drops nothing");
        assert!(events.iter().all(|e| e.counts.credits == 0.0));

        // Now with the vendor actually filling the columns in: still no money, and
        // still the same rows.
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE usage_ledger SET cost = 3.75, cost_source = 'vendor' WHERE id <= 3", [])
            .unwrap();
        drop(conn);
        let (events, census) = read_all(&path, ReadCursor(0));
        assert_eq!(census.rows_with_cost, 3);
        assert_eq!(census.rows_with_cost_source, 3);
        assert_eq!(census.events, 7, "a cost column is no reason to skip a row");
        assert!(events.iter().all(|e| e.counts.credits == 0.0), "tokenme prices centrally");
    }

    #[test]
    fn a_locked_or_foreign_database_degrades_and_freezes_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());
        // A writer mid-transaction must not turn into an error, and must not make us
        // lose the rows it has already committed.
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE; INSERT INTO usage_ledger (session_id, created_timestamp, model, input_tokens, output_tokens, total_tokens, cache_read_tokens, is_compaction) VALUES ('20260806_1', 1788349050, 'agnes-2.5-flash', 100, 10, 110, 0, 0);").unwrap();
        let (events, census) = read_all(&path, ReadCursor(0));
        assert_eq!(census.events, 7, "the uncommitted row is invisible, the pass is not an error: {census:?}");
        assert_eq!(events.len(), 7);
        writer.execute_batch("COMMIT;").unwrap();
        drop(writer);
        // The committed row is then picked up from the parked cursor.
        let (newest, census) = read_all(&path, ReadCursor(8));
        assert_eq!((census.rows, census.events, census.max_id), (1, 1, 9));
        assert_eq!(newest[0].source, "db#usage_ledger#9");
        // Gone or not ours: empty, cursor frozen, no error, no panic.
        let (outcome, census) = read_ledger(&dir.path().join("absent.db"), ReadCursor(7), "db").unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(7));
        assert_eq!(census, Census::default());
        let other = dir.path().join("other.db");
        let conn = Connection::open(&other).unwrap();
        conn.execute_batch("CREATE TABLE something_else (id INTEGER PRIMARY KEY); INSERT INTO something_else VALUES (1);")
            .unwrap();
        drop(conn);
        let (outcome, census) = read_ledger(&other, ReadCursor(3), "db").unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(3), "no usage_ledger: not our business");
        assert_eq!(census, Census::default());
        // A file that is not a database at all behaves the same way.
        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, b"not sqlite").unwrap();
        let (outcome, _) = read_ledger(&junk, ReadCursor(11), "db").unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(11));
    }

    #[test]
    fn a_ledger_without_the_sessions_table_still_bills() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bare.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        conn.execute(INSERT, rusqlite::params!["s", 1_788_349_044, "agnes-2.5-flash", 1000, 40, 1040, Some(256), 0])
            .unwrap();
        drop(conn);
        let (events, census) = {
            let (outcome, census) = read_ledger(&path, ReadCursor(0), "db").unwrap();
            (outcome.events, census)
        };
        assert_eq!((census.rows, census.events), (1, 1));
        assert_eq!(events[0].project, None, "no sessions table ⇒ no label, still a billable row");
        assert_eq!(events[0].counts.input, 744.0);
        assert_eq!(events[0].ts_ms, 1_788_349_044_000);
    }

    #[test]
    fn the_db_is_the_active_source_and_the_logs_are_the_fallback() {
        let _env = paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("state/logs/llm/20260902_1")).unwrap();
        std::fs::create_dir_all(root.join("data/sessions")).unwrap();
        std::env::set_var(paths::ENV_PATH_ROOT, root);

        let log = root.join("state/logs/llm/20260902_1/1788348486108-07179738-main.jsonl");
        let body = [
            "{\"meta\": {\"session_id\": \"20260902_1\", \"purpose\": \"main\", \"request_id\": \"07179738700a42b59c3bf3fcb46628df\", \"timestamp_ms\": 1788348486108}}",
            "{\"input\": {}, \"model_config\": {\"model_name\": \"agnes-2.5-flash\"}}",
            "{\"data\": {\"chunk\": 1}, \"usage\": null}",
            "{\"data\": null, \"usage\": {\"input_tokens\": 23607, \"output_tokens\": 844, \"total_tokens\": 24451, \"cache_read_input_tokens\": 11776, \"cache_write_input_tokens\": null}}",
        ]
        .join("\n");
        std::fs::write(&log, format!("{body}\n")).unwrap();
        std::fs::write(root.join("state/logs/llm/20260902_1/notes.txt"), b"x\n").unwrap();
        std::fs::write(root.join("state/logs/llm/loose-1-2-main.jsonl"), b"{}\n").unwrap();

        let found = discover(&DateFilter::default());
        assert_eq!(found.len(), 2, "the two jsonl files, nothing else: {found:?}");
        assert!(found.iter().all(|f| f.kind == FileKind::Jsonl));
        assert_eq!(found[0].path, log, "sorted by path");
        assert_eq!(found[0].size, body.len() as u64 + 1);
        let detected = probe().unwrap();
        assert_eq!((detected.id.as_str(), detected.display.as_str()), ("agnes", "AgnesCode"));
        assert_eq!(detected.roots, vec![root.to_path_buf()]);
        assert!(detected.hint.unwrap().contains("fallback"));
        assert!(discover(&DateFilter::new(None, Some(0))).is_empty(), "until_ms prunes by mtime");

        // The fallback bills once per file and stops on a line boundary.
        let out = read(&found[0], ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].counts.total(), 24451.0);
        assert_eq!(out.cursor, ReadCursor(body.len() as u64 + 1));
        let again = read(&found[0], out.cursor).unwrap();
        assert!(again.events.is_empty() && again.cursor == out.cursor);
        assert!(matches!(read(&found[0], ReadCursor(out.cursor.0 + 1)), Err(Error::Cursor { .. })));
        // A torn tail is not an error and is not consumed.
        std::fs::write(&log, format!("{}\n\"data\"", body)).unwrap();
        let grown = SourceFile { size: body.len() as u64 + 9, ..found[0].clone() };
        let out = read(&grown, ReadCursor(body.len() as u64 + 1)).unwrap();
        assert!(out.events.is_empty() && out.cursor == ReadCursor(body.len() as u64 + 1), "the partial line waits: {:?}", out.cursor);

        // Installing the ledger makes it the only source handed out: the overlap is
        // the same calls in another id space, so both would double-bill.
        let db = fixture_db(&root.join("data/sessions"));
        let found = discover(&DateFilter::default());
        assert_eq!(found.len(), 1, "the logs are dropped: {found:?}");
        assert_eq!(found[0].kind, FileKind::Sqlite);
        assert_eq!(found[0].path, db);
        let out = read(&found[0], ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 7);
        assert_eq!(out.cursor, ReadCursor(8), "the cursor is a rowid");
        assert_eq!(out.events[0].source, format!("{}#usage_ledger#1", db.display()));
        assert!(probe().unwrap().hint.unwrap().contains("usage_ledger"));
        std::env::remove_var(paths::ENV_PATH_ROOT);
    }
}

//! Primary reader: `~/.zcode/cli/db/db.sqlite`, table `model_usage`.
//!
//! One row per billed model call, written when the call finishes — 18,112 rows /
//! 151 sessions on this machine against the 151 records / 3 sessions that survive
//! in `rollout/` (see [`crate::paths`] for the source choice). The cursor is the
//! highest consumed `rowid`, which is what [`usage_core::FileKind::Sqlite`] means.
//!
//! ## Stage mapping — the opposite convention to the rollout log
//! ZCode normalises the provider payload into this table and **folds the cached
//! prefix into `input_tokens`**: for the very same call the provider reports
//! `input_tokens 702 + cache_read_input_tokens 381_888` while the row stores
//! `input_tokens 382_590`, and `input + output == computed_total_tokens`. So the
//! stages are netted here: `input = input_tokens − cache_read_input_tokens`.
//! Taking `input_tokens` verbatim would bill the cached prefix twice (4.61 B of
//! 4.74 B tokens across the real table).
//!
//! `reasoning_tokens` is a sub-breakdown of `output_tokens` (that identity is what
//! `computed_total_tokens` proves), so it is reported as [`TokenCounts::reasoning`]
//! and never added on top. `cache_creation_input_tokens` is 0 in all 18,112 rows.
//! `raw_usage_json` and `provider_total_tokens` are the same numbers again in
//! another dialect and are not read; `totalCost`-style money never is either — cost
//! is computed centrally from the shared price table.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use usage_core::{Error, ReadCursor, ReadOutcome, TokenCounts, UsageEvent};

use crate::parser::{build_event, Call};
use crate::paths::USAGE_TABLE;

/// Row census, so the smoke test can diff events against the table itself.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Census {
    pub rows: usize,
    pub events: usize,
    /// Rows skipped because every stage was 0 (`status = error | cancelled`).
    pub zero_rows: usize,
    /// Rows where `input_tokens < cache_read_input_tokens`, i.e. the inclusive
    /// convention this module assumes broke down. 0 across all 18,112 rows.
    pub inclusive_violations: usize,
    /// Highest `rowid` present when the table was opened.
    pub max_rowid: i64,
    pub totals: TokenCounts,
}

/// Read `model_usage` rows appended after `cursor`.
///
/// A database that cannot be opened, or that has no `model_usage` table, is
/// reported as an empty outcome with the **unchanged** cursor (nothing was
/// consumed); `Err` is reserved for a cursor past the end of the row space.
pub fn read(path: &Path, cursor: ReadCursor, source_key: &str) -> Result<(ReadOutcome, Census), Error> {
    let Ok(conn) = open(path) else {
        return Ok((ReadOutcome { events: Vec::new(), cursor }, Census::default()));
    };
    if !table_exists(&conn, USAGE_TABLE) {
        return Ok((ReadOutcome { events: Vec::new(), cursor }, Census::default()));
    }
    let max_rowid = scalar(&conn, &format!("SELECT COALESCE(MAX(rowid), 0) FROM {USAGE_TABLE}"))?;
    if i64::try_from(cursor.0).unwrap_or(i64::MAX) > max_rowid {
        return Err(Error::Cursor { path: path.to_path_buf(), cursor: cursor.0 });
    }
    // `session` holds the project label; if a build ever drops it the usage rows
    // are still billable, so that column is optional.
    let label = if table_exists(&conn, "session") {
        "(SELECT COALESCE(NULLIF(s.directory, ''), NULLIF(s.path, ''), s.title) FROM session s WHERE s.id = m.session_id)"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT m.rowid, m.session_id, m.logical_request_id, m.attempt_index, m.model_id, m.completed_at, m.started_at, \
         m.input_tokens, m.cache_read_input_tokens, m.cache_creation_input_tokens, m.output_tokens, m.reasoning_tokens, \
         {label} AS project \
         FROM {USAGE_TABLE} m WHERE m.rowid > ?1 ORDER BY m.rowid"
    );
    let (events, census) = {
        let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
        let key = source_key.to_string();
        let rows = stmt
            .query_map([max_rowid_of(cursor)], |row| row_to_event(row, &key))
            .map_err(sqlite_err)?;
        let mut events = Vec::new();
        let mut census = Census { max_rowid, ..Census::default() };
        for row in rows {
            let (event, flags) = row.map_err(sqlite_err)?;
            census.rows += 1;
            census.zero_rows += usize::from(flags.zero);
            census.inclusive_violations += usize::from(flags.inclusive_violation);
            if let Some(event) = event {
                census.events += 1;
                census.totals += &event.counts;
                events.push(event);
            }
        }
        (events, census)
    };
    Ok((
        ReadOutcome {
            events,
            cursor: ReadCursor(u64::try_from(max_rowid).unwrap_or(u64::MAX)),
        },
        census,
    ))
}

/// Everything one row means, plus whether it was billable.
fn row_to_event(row: &rusqlite::Row<'_>, source_key: &str) -> rusqlite::Result<(Option<UsageEvent>, RowFlags)> {
    let rowid: i64 = row.get::<_, i64>("rowid")?;
    let session: Option<String> = row.get::<_, Option<String>>("session_id")?;
    let session = session.filter(|s| !s.is_empty()).unwrap_or_else(|| format!("zcode-row{rowid}"));
    let request_id: Option<String> = row.get::<_, Option<String>>("logical_request_id")?;
    let attempt: i64 = row.get::<_, Option<i64>>("attempt_index")?.unwrap_or(0);
    let model: Option<String> = row.get::<_, Option<String>>("model_id")?.filter(|m| !m.is_empty());
    let completed_at: Option<i64> = row.get::<_, Option<i64>>("completed_at")?;
    let started_at: Option<i64> = row.get::<_, Option<i64>>("started_at")?;
    let stored_input: i64 = row.get::<_, Option<i64>>("input_tokens")?.unwrap_or(0);
    let cache_read: i64 = row.get::<_, Option<i64>>("cache_read_input_tokens")?.unwrap_or(0);
    let cache_creation: i64 = row.get::<_, Option<i64>>("cache_creation_input_tokens")?.unwrap_or(0);
    let output: i64 = row.get::<_, Option<i64>>("output_tokens")?.unwrap_or(0);
    let reasoning: i64 = row.get::<_, Option<i64>>("reasoning_tokens")?.unwrap_or(0);
    let project = basename(&row.get::<_, Option<String>>("project")?);

    let flags = RowFlags {
        zero: stored_input == 0 && cache_read == 0 && cache_creation == 0 && output == 0,
        inclusive_violation: stored_input < cache_read,
    };
    let counts = TokenCounts {
        input: stored_input.saturating_sub(cache_read).max(0) as f64,
        cache_creation: cache_creation.max(0) as f64,
        cache_read: cache_read.max(0) as f64,
        output: output.max(0) as f64,
        // A sub-breakdown of `output`, clamped so it can never inflate a total.
        reasoning: (reasoning.max(0) as f64).min(output.max(0) as f64),
        credits: 0.0,
    };
    let event = millis(completed_at.or(started_at)).and_then(|ts_ms| {
        build_event(Call {
            session: &session,
            request_id: request_id.as_deref(),
            attempt,
            ts_ms,
            model: model.as_deref(),
            project,
            counts,
            source_key: &source_for(source_key, rowid),
        })
    });
    Ok((event, flags))
}

#[derive(Debug, Default, Clone, Copy)]
struct RowFlags {
    zero: bool,
    inclusive_violation: bool,
}

/// Session rows store an absolute `directory`; the label is its last component,
/// which is what every other adapter reports.
fn basename(path: &Option<String>) -> Option<String> {
    Some(Path::new(path.as_deref()?).file_name()?.to_str()?.to_string())
}

/// `event.source` is the manifest key of the origin, and `rowid` lets a truncated
/// table purge exactly its own stale events.
fn source_for(source_key: &str, rowid: i64) -> String {
    format!("{source_key}#{USAGE_TABLE}#{rowid}")
}

/// ZCode writes 13-digit epoch ms; the <1e10 ⇒ seconds cutoff is the same one
/// [`usage_core::parse_ts_ms`] applies.
fn millis(value: Option<i64>) -> Option<i64> {
    let v = value.filter(|v| *v > 0)?;
    Some(if v < 10_000_000_000 { v * 1000 } else { v })
}

fn max_rowid_of(cursor: ReadCursor) -> i64 {
    i64::try_from(cursor.0).unwrap_or(i64::MAX)
}

fn open(path: &Path) -> Result<Connection, Error> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| Error::io(path, std::io::Error::other(e)))?;
    // ZCode keeps its own writer open; a lock means "busy", not "gone".
    let _ = conn.busy_timeout(std::time::Duration::from_millis(1500));
    Ok(conn)
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .and_then(|mut s| s.query_row([name], |_| Ok(())).optional())
        .unwrap_or(None)
        .is_some()
}

fn scalar(conn: &Connection, sql: &str) -> Result<i64, Error> {
    let mut stmt = conn.prepare(sql).map_err(sqlite_err)?;
    let mut rows = stmt.query([]).map_err(sqlite_err)?;
    match rows.next().map_err(sqlite_err)? {
        Some(row) => row.get::<_, i64>(0).map_err(sqlite_err),
        None => Ok(0),
    }
}

fn sqlite_err(e: rusqlite::Error) -> Error {
    Error::Sqlite(e.to_string())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use usage_core::ReadCursor;

    use super::*;

    fn fixture() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/model_usage.sqlite")
    }

    fn read_all(cursor: ReadCursor) -> (Vec<UsageEvent>, Census) {
        let (outcome, census) = read(&fixture(), cursor, "src").unwrap();
        (outcome.events, census)
    }

    #[test]
    fn maps_the_real_rows_with_the_cached_prefix_netted_out() {
        let (events, census) = read_all(ReadCursor(0));
        assert_eq!(census.rows, 9, "every row of the fixture was visited");
        assert_eq!(census.events, 8, "the zero-usage error row is not billable");
        assert_eq!(census.zero_rows, 1);
        assert_eq!(census.inclusive_violations, 0);
        assert_eq!(census.max_rowid, 9);
        let e = &events[0];
        // Row 1: stored input_tokens 253, of which 192 came out of the cache.
        assert_eq!(e.counts, TokenCounts { input: 61.0, cache_creation: 0.0, cache_read: 192.0, output: 14.0, reasoning: 0.0, credits: 0.0 });
        assert_eq!(e.counts.total(), 267.0, "matches the row's computed_total_tokens");
        assert_eq!(e.tool, "zcode");
        assert_eq!(e.session, "sess_37312a25-fba3-4f1b-ace0-5aeecc32565b");
        assert_eq!(e.model.as_deref(), Some("GLM-5.3"), "verbatim, exactly what the rollout path reports (a title call, hence the cheaper model)");
        assert_eq!(e.project.as_deref(), Some("WorkFile"), "basename of session.directory");
        // Note the id: a `session_title` row keeps a bare (truncated) uuid where a
        // `main_turn` row uses `msg_<stamp>_<uuid>`; both are unique per row.
        assert_eq!(e.dedupe_key.as_deref(), Some("sess_37312a25-fba3-4f1b-ace0-5aeecc32565b#e60d9f2b-442a-47#0"));
        assert_eq!(e.ts_ms, 1_787_623_268_943, "completed_at, 13-digit ms untouched");
        assert_eq!(e.source, "src#model_usage#1", "manifest key + rowid");
        assert_eq!(e.meter, usage_core::Meter::Tokens);
        // The newest session's row is also the last record of the rollout fixture:
        // the db stores 385 800 = 392 + 385 408 where the provider split it.
        let db = events.iter().find(|e| e.project.as_deref() == Some("bug-hunter")).unwrap();
        assert_eq!(db.counts, TokenCounts { input: 392.0, cache_creation: 0.0, cache_read: 385_408.0, output: 54.0, reasoning: 0.0, credits: 0.0 });
        assert_eq!(census.totals, TokenCounts { input: 18_998.0, cache_creation: 0.0, cache_read: 477_888.0, output: 1586.0, reasoning: 0.0, credits: 0.0 });
        assert_eq!(census.totals.input + census.totals.cache_read, 496_886.0, "Σ stored input_tokens, billed once");
    }

    #[test]
    fn a_retry_attempt_is_a_separate_event() {
        // Real rows carry attempt_index 0 only, so the fixture appends a second
        // attempt of the first row; it must not merge with it.
        let (events, _) = read_all(ReadCursor(0));
        let keys: Vec<_> = events.iter().filter_map(|e| e.dedupe_key.clone()).filter(|k| k.contains("e60d9f2b-442a-47")).collect();
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(keys[0].ends_with("#0") && keys[1].ends_with("#1"), "{keys:?}");
        let set: std::collections::HashSet<_> = events.iter().map(|e| e.dedupe_key.clone()).collect();
        assert_eq!(set.len(), events.len(), "no two rows of one db share a key");
    }

    #[test]
    fn cursor_is_the_rowid_and_drives_no_double_count() {
        let (first, census) = read_all(ReadCursor(0));
        assert_eq!(first.len(), 8);
        assert_eq!(census.max_rowid, 9);
        // Resuming from the returned cursor learns nothing new.
        let (outcome, _) = read(&fixture(), ReadCursor(9), "src").unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(9));
        let (partial, _) = read_all(ReadCursor(6));
        assert_eq!(partial.len(), 2, "rows 8 and 9 — row 7 is the zero-usage error");
        assert_eq!(partial[0].source, "src#model_usage#8");
        // Past the end of the row space is the error case.
        assert!(matches!(read(&fixture(), ReadCursor(10), "src"), Err(Error::Cursor { .. })));
    }

    #[test]
    fn unreadable_or_foreign_database_yields_no_events_and_keeps_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.sqlite");
        let (outcome, census) = read(&missing, ReadCursor(7), "src").unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(7));
        assert_eq!(census, Census::default());
        let foreign = dir.path().join("other.sqlite");
        let conn = Connection::open(&foreign).unwrap();
        conn.execute_batch("create table something_else (id integer primary key, x integer); insert into something_else values (1, 2);").unwrap();
        drop(conn);
        let (outcome, census) = read(&foreign, ReadCursor(3), "src").unwrap();
        assert!(outcome.events.is_empty() && outcome.cursor == ReadCursor(3), "no model_usage table: not our business");
        assert_eq!(census, Census::default());
    }

    #[test]
    fn null_and_garbage_columns_degrade_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model_usage.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("create table model_usage (id text primary key, session_id text, logical_request_id text, attempt_index integer, model_id text, completed_at integer, started_at integer, input_tokens integer, cache_read_input_tokens integer, cache_creation_input_tokens integer, output_tokens integer, reasoning_tokens integer);").unwrap();
        conn.execute("insert into model_usage values ('a', null, null, null, null, null, 2, 100, 30, null, null, 900)", []).unwrap();
        conn.execute("insert into model_usage values ('b', 'sess_b', 'req_b', 3, '', 1, 0, 10, 4, 0, 5, 2)", []).unwrap();
        drop(conn);
        let (events, census) = {
            let (outcome, census) = read(&path, ReadCursor(0), "src").unwrap();
            (outcome.events, census)
        };
        assert_eq!(census.rows, 2);
        assert_eq!(events.len(), 2);
        assert_eq!(census.inclusive_violations, 0, "row 1 has 100 >= 30");
        assert_eq!(census.zero_rows, 0);
        let a = &events[0];
        assert_eq!(a.session, "zcode-row1", "a null session_id still has to be attributable");
        assert_eq!(a.dedupe_key, None, "no request id ⇒ no dedupe key");
        assert_eq!(a.counts.input, 70.0);
        assert_eq!(a.counts.cache_read, 30.0);
        assert_eq!(a.counts.output, 0.0);
        assert_eq!(a.counts.reasoning, 0.0, "reasoning beyond a zero output is clamped, never added");
        assert_eq!(a.ts_ms, 2000, "started_at fallback, and seconds are promoted to ms");
        assert_eq!(a.model, None, "an empty model_id stays unknown rather than inventing one");
        assert_eq!(a.project, None, "no session table ⇒ no label");
        assert_eq!(events[1].dedupe_key.as_deref(), Some("sess_b#req_b#3"));
        assert_eq!(events[1].counts.reasoning, 2.0, "a real sub-breakdown of output is kept");
        assert_eq!(events[1].ts_ms, 1000);
    }
}

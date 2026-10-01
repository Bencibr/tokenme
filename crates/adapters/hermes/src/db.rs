//! The billable rows of Hermes's `state.db`.
//!
//! One cumulative row per `(session, model, billing_provider, base_url, mode,
//! task)` in `session_model_usage`: an upsert keeps raising the token counters
//! while a session runs, so a row is the whole-to-date consumption of that
//! slice, not a delta. The events handed up are those cumulative totals with a
//! stable dedupe key — the indexer replaces on the key, so re-reading a row is
//! idempotent and the cursor may replay it freely.
//!
//! The cursor is the highest `last_seen` consumed (milliseconds). An SQLite
//! `UPDATE` keeps the rowid, so the rowid cursor the append-only sources use
//! would never re-see a live row's growth; `last_seen` bumps on every call and
//! is the column the tool itself orders recency by. A zero-token row (session
//! opened, nothing answered) is skipped but still advances the cursor — when
//! its tokens land, `last_seen` moves past the mark and the row is caught then.
//!
//! `reasoning_tokens` is a sub-breakdown of `output_tokens` — the same identity
//! the zcode adapter asserts for this vendor family — so it maps onto
//! `TokenCounts.reasoning` and never enters `total()` twice.
//!
//! Read-only against a database the gateway owns and writes while it runs:
//! open read-only and treat a lock as "no rows this pass", never an error.

use rusqlite::{Connection, OpenFlags};

use usage_core::{Error, Meter, ReadCursor, ReadOutcome, SourceFile, UsageEvent};

/// Rows touched in one pass; per-session slices stay small next to a
/// transcript row count.
const BATCH: i64 = 512;

const QUERY: &str = "\
SELECT u.session_id, u.model, u.billing_provider, u.task, u.api_call_count, \
       u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens, \
       u.reasoning_tokens, u.last_seen, s.cwd \
FROM session_model_usage u LEFT JOIN sessions s ON s.id = u.session_id \
WHERE u.last_seen > ?1 \
ORDER BY u.last_seen LIMIT ?2";

/// Cheap probe census: one aggregate query, run at startup only.
pub fn count_usage(path: &std::path::Path) -> Option<i64> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    conn.query_row("SELECT count(*) FROM session_model_usage", [], |r| r.get(0)).ok()
}

pub fn read(file: &SourceFile, cursor: ReadCursor) -> Result<(ReadOutcome, u64), Error> {
    let conn = match Connection::open_with_flags(
        &file.path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        // A locked or mid-write database is a skip, not a failure: the next
        // pass sees the file as changed and retries.
        Err(_) => return Ok((ReadOutcome { events: Vec::new(), cursor }, cursor.0)),
    };
    let mut stmt = match conn.prepare(QUERY) {
        Ok(stmt) => stmt,
        Err(_) => return Ok((ReadOutcome { events: Vec::new(), cursor }, cursor.0)),
    };
    // The cursor counts milliseconds; `last_seen` is REAL seconds.
    let since_s = cursor.0 as f64 / 1000.0;
    let rows = stmt
        .query_map(rusqlite::params![since_s, BATCH], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, f64>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })
        .map_err(|source| Error::Io { path: file.path.clone(), source: std::io::Error::other(source) })?;

    let mut events = Vec::new();
    let mut high = cursor.0;
    for row in rows {
        let (session_id, model, billing_provider, task, _api_calls, input, output, cache_read, cache_write, reasoning, last_seen, cwd) =
            row.map_err(|source| Error::Io { path: file.path.clone(), source: std::io::Error::other(source) })?;
        let ts_ms = (last_seen.max(0.0) * 1000.0) as u64;
        high = high.max(ts_ms);
        // A session that opened but never answered: nothing to bill yet, and
        // when that changes `last_seen` moves past the cursor. The token
        // columns are the billable truth — a call count without tokens is
        // still an empty slice.
        if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
            continue;
        }
        let mut event = UsageEvent::new(crate::TOOL_ID, ts_ms as i64, &session_id);
        event.counts.input = input as f64;
        event.counts.output = output as f64;
        event.counts.cache_read = cache_read as f64;
        event.counts.cache_creation = cache_write as f64;
        event.counts.reasoning = reasoning as f64;
        event.meter = Meter::Tokens;
        event.model = Some(model.clone());
        event.project = cwd.as_deref().filter(|c| !c.is_empty()).map(project_name);
        event.dedupe_key = Some(format!(
            "{}{session_id}#{model}#{billing_provider}#{task}",
            crate::DEDUPE_PREFIX
        ));
        event.source = file.key();
        events.push(event);
    }
    Ok((ReadOutcome { events, cursor: ReadCursor(high) }, high))
}

/// `C:\Users\sp\workspace\tokenme` → `tokenme`; the panel shows basenames, and
/// Hermes runs on both separators.
fn project_name(cwd: &str) -> String {
    cwd.rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    /// The two tables the adapter reads, with the tool's own column set.
    pub const DDL: &str = "\
CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT); \
CREATE TABLE session_model_usage (session_id TEXT NOT NULL, model TEXT NOT NULL, \
 billing_provider TEXT NOT NULL DEFAULT '', billing_base_url TEXT NOT NULL DEFAULT '', \
 billing_mode TEXT NOT NULL DEFAULT '', task TEXT NOT NULL DEFAULT '', \
 api_call_count INTEGER NOT NULL DEFAULT 0, input_tokens INTEGER NOT NULL DEFAULT 0, \
 output_tokens INTEGER NOT NULL DEFAULT 0, cache_read_tokens INTEGER NOT NULL DEFAULT 0, \
 cache_write_tokens INTEGER NOT NULL DEFAULT 0, reasoning_tokens INTEGER NOT NULL DEFAULT 0, \
 estimated_cost_usd REAL NOT NULL DEFAULT 0, actual_cost_usd REAL NOT NULL DEFAULT 0, \
 cost_status TEXT, cost_source TEXT, first_seen REAL, last_seen REAL, \
 PRIMARY KEY (session_id, model, billing_provider, billing_base_url, billing_mode, task));";

    pub fn fixture(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("state.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        path
    }

    pub fn insert_usage(db: &Path, session: &str, model: &str, task: &str, input: i64, output: i64, last_seen: f64) {
        let conn = Connection::open(db).unwrap();
        // INSERT OR REPLACE: the tool upserts the slice as a session runs, and
        // the composite PK is what a second call collides on.
        conn.execute(
            "INSERT OR REPLACE INTO session_model_usage (session_id, model, task, api_call_count, input_tokens, output_tokens, last_seen) \
             VALUES (?1, ?2, ?3, 3, ?4, ?5, ?6)",
            rusqlite::params![session, model, task, input, output, last_seen],
        )
        .unwrap();
    }
}

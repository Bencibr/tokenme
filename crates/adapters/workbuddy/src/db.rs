//! The billable rows of `workbuddy.db`.
//!
//! The desktop app meters in **credits**, not tokens: `session_usage` carries
//! one row per session — `used`/`size` are context-window fullness (not
//! consumption), and the only money signal is `credit_json`, a
//! `{model-hash: credits}` map the app writes when a session closes (measured
//! here: on 2026-09-19/20 both credited rows landed with `updated_at` equal to
//! the session's own close).
//!
//! One event per credited session, keyed `workbuddy#<session_id>`, credits =
//! the map's sum. The cursor is the highest `updated_at` consumed — an SQLite
//! `UPDATE` keeps the rowid, so the rowid cursor the other SQLite sources use
//! would miss the credit landing on an already-seen row.
//!
//! Read-only against a database the app owns and writes while it runs: open
//! with an immutable-ish read-only connection and treat a lock as "no rows
//! this pass", never an error.

use rusqlite::{Connection, OpenFlags};

use usage_core::{Error, Meter, ReadCursor, ReadOutcome, SourceFile, UsageEvent};

use crate::DEDUPE_PREFIX;

/// Cheap probe census: one aggregate query, run at startup only.
pub fn count_sessions(path: &std::path::Path) -> Option<i64> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    conn.query_row("SELECT count(*) FROM session_usage", [], |r| r.get(0)).ok()
}

/// Rows touched in one pass; the table is per-session, so this is the number
/// of sessions, and it stays small next to a transcript row count.
const BATCH: i64 = 512;

// The WHERE clause filters on time only: the credit/status predicates run in
// Rust so a skipped row (free-model session, still-running session) still
// advances the cursor and is never scanned again. A live session that later
// closes bumps `updated_at` past the high-water mark and is caught then.
const QUERY: &str = "\
SELECT u.session_id, u.credit_json, u.updated_at, s.model, s.cwd, s.status \
FROM session_usage u JOIN sessions s ON s.id = u.session_id \
WHERE u.updated_at > ?1 \
ORDER BY u.updated_at LIMIT ?2";

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
    let rows = stmt
        .query_map(rusqlite::params![cursor.0, BATCH], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|source| Error::Io { path: file.path.clone(), source: std::io::Error::other(source) })?;

    let mut events = Vec::new();
    let mut high = cursor.0;
    for row in rows {
        let (session_id, credit_json, updated_at, model, cwd, status) =
            row.map_err(|source| Error::Io { path: file.path.clone(), source: std::io::Error::other(source) })?;
        let updated_at = updated_at.max(0) as u64;
        let credits = match (status == "completed", credit_json) {
            (true, Some(json)) => credit_sum(&json),
            _ => 0.0,
        };
        if credits <= 0.0 {
            high = high.max(updated_at);
            continue;
        }
        let mut event = UsageEvent::new(crate::TOOL_ID, updated_at as i64, &session_id);
        event.counts.credits = credits;
        event.meter = Meter::Credits;
        event.model = model.filter(|m| !m.is_empty());
        event.project = Some(project_name(&cwd));
        event.dedupe_key = Some(format!("{DEDUPE_PREFIX}{session_id}"));
        event.source = file.key();
        events.push(event);
        high = high.max(updated_at);
    }
    Ok((ReadOutcome { events, cursor: ReadCursor(high) }, high))
}

/// `{"<model hash>": 220.55, …}` → the session's total credits. A map entry
/// that will not parse is dropped, not fatal — the vendor owns this shape.
fn credit_sum(credit_json: &str) -> f64 {
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(credit_json)
        .map(|m| m.values().filter_map(serde_json::Value::as_f64).sum())
        .unwrap_or(0.0)
}

/// `/Users/me/workspace/bug-hunter` → `bug-hunter`; the panel shows basenames.
fn project_name(cwd: &str) -> String {
    cwd.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(cwd).to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    const DDL: &str = "\
CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT NOT NULL, user_id TEXT NOT NULL DEFAULT '', \
 status TEXT NOT NULL DEFAULT 'Pending', created_at INTEGER NOT NULL DEFAULT 0, \
 updated_at INTEGER NOT NULL DEFAULT 0, model TEXT); \
CREATE TABLE session_usage (session_id TEXT PRIMARY KEY, used INTEGER NOT NULL, \
 size INTEGER NOT NULL, updated_at INTEGER NOT NULL, credit_json TEXT);";

    pub(crate) fn fixture(dir: &Path) -> SourceFile {
        let path = dir.join("workbuddy.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        // One completed, credited session; one completed free-model session
        // (no credits); one still-running session with credits mid-flight —
        // only the first may leave the adapter.
        conn.execute_batch(
            "INSERT INTO sessions(id, cwd, status, model) VALUES \
             ('s-done', '/Users/me/workspace/bug-hunter', 'completed', 'deepseek-v4.1-flash'), \
             ('s-free', '/Users/me/workspace/tokenme', 'completed', 'deepseek-v4.1-flash'), \
             ('s-live', '/Users/me/workspace/x', 'Pending', 'hy4-preview-f'); \
             INSERT INTO session_usage VALUES \
             ('s-done', 12345, 300000, 1000, '{\"a\": 9.27, \"b\": 17}'), \
             ('s-free', 999, 300000, 2000, NULL), \
             ('s-live', 50, 300000, 3000, '{\"c\": 8.0}');",
        )
        .unwrap();
        drop(conn);
        let (size, mtime_ms) = crate::paths::stat_file(&path).unwrap();
        SourceFile { path, kind: usage_core::FileKind::Sqlite, size, mtime_ms }
    }

    #[test]
    fn only_completed_sessions_with_credits_become_events() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let (outcome, high) = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(outcome.events.len(), 1, "{:?}", outcome.events);
        let ev = &outcome.events[0];
        assert_eq!(ev.counts.credits, 26.27);
        assert_eq!(ev.meter, Meter::Credits);
        assert_eq!(ev.model.as_deref(), Some("deepseek-v4.1-flash"));
        assert_eq!(ev.project.as_deref(), Some("bug-hunter"));
        assert_eq!(ev.dedupe_key.as_deref(), Some("workbuddy#s-done"));
        // The free and live rows still advance the cursor past themselves:
        // neither may be re-read next pass.
        assert_eq!(high, 3000);
    }

    #[test]
    fn the_cursor_skips_already_consumed_rows_but_catches_late_credits() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let (_, high) = read(&file, ReadCursor(0)).unwrap();
        let (again, _) = read(&file, ReadCursor(high)).unwrap();
        assert!(again.events.is_empty());
        // The running session closes: its row is updated in place (same
        // rowid, newer updated_at) — exactly what a rowid cursor would miss.
        let conn = Connection::open(&file.path).unwrap();
        conn.execute_batch(
            "UPDATE sessions SET status = 'completed' WHERE id = 's-live';
             UPDATE session_usage SET updated_at = 4000, credit_json = '{\"c\": 8.0}' WHERE session_id = 's-live'",
        )
        .unwrap();
        drop(conn);
        let (after, _) = read(&file, ReadCursor(high)).unwrap();
        assert_eq!(after.events.len(), 1);
        assert_eq!(after.events[0].dedupe_key.as_deref(), Some("workbuddy#s-live"));
        assert_eq!(after.events[0].counts.credits, 8.0);
    }

    #[test]
    fn a_missing_or_locked_database_is_an_empty_read_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = fixture(dir.path());
        file.path = dir.path().join("absent.db");
        let (outcome, _) = read(&file, ReadCursor(0)).unwrap();
        assert!(outcome.events.is_empty());
    }
}

//! The billable rows of `workbuddy.db`.
//!
//! WorkBuddy's desktop app meters in **credits**, not tokens: `session_usage`
//! carries one row per session — `used`/`size` are context-window fullness
//! (not consumption), `credit_json` is a `{model-hash: credits}` map written
//! when a session closes, and the traces carry `totalTokens: 0` (re-measured
//! 2026-09-29), so there is no token signal anywhere in the store.
//!
//! One event per **completed** session, keyed `workbuddy#<session_id>`,
//! `credits` = the map's sum or 0. The session itself is usage regardless of
//! what the vendor charged for it — free-model work must still count as a
//! 回话 — while credits stay the only money signal. Events fire at close only,
//! and that is load-bearing: the index dedupe is insert-once (a re-emitted key
//! is swallowed), so a mid-flight 0-credit event would permanently hide the
//! credited re-emission when the session closes.
//!
//! The read is a full scan of the per-session table on every pass — its row
//! count is human-bounded, and rescanning is what backfills sessions the old
//! credited-only cursor had already walked past without any index rebuild.
//! Idempotence comes from the dedupe keys, and the cursor is bookkeeping only.
//!
//! Read-only against a database the app owns and writes while it runs: open
//! with an immutable-ish read-only connection and treat a lock as "no rows
//! this pass", never an error.

use rusqlite::{Connection, OpenFlags};

use usage_core::{Error, Meter, ReadCursor, ReadOutcome, SourceFile, UsageEvent};

use crate::DEDUPE_PREFIX;

/// The rows that count as usage: closed sessions the user has not deleted.
/// The app's own cloud indexes treat `deleted_at = -1` as not deleted, and
/// local rows keep it NULL.
const USAGE_PREDICATE: &str =
    "s.status = 'completed' AND (s.deleted_at IS NULL OR s.deleted_at = -1)";

/// Cheap probe census: one aggregate query, run at startup only. Matches what
/// [`read`] would emit, so "N sessions" is the count a scan produces.
pub fn count_sessions(path: &std::path::Path) -> Option<i64> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    conn.query_row(
        &format!(
            "SELECT count(*) FROM session_usage u JOIN sessions s ON s.id = u.session_id \
             WHERE {USAGE_PREDICATE}"
        ),
        [],
        |r| r.get(0),
    )
    .ok()
}

// No LIMIT: the table is one row per session, so a full scan is a few hundred
// small rows even after years of daily use.
fn query() -> String {
    format!("\
SELECT u.session_id, u.credit_json, u.updated_at, s.model, s.cwd \
FROM session_usage u JOIN sessions s ON s.id = u.session_id \
WHERE {USAGE_PREDICATE} \
ORDER BY u.updated_at")
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
    let sql = query();
    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(_) => return Ok((ReadOutcome { events: Vec::new(), cursor }, cursor.0)),
    };
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|source| Error::Io { path: file.path.clone(), source: std::io::Error::other(source) })?;

    let mut events = Vec::new();
    let mut high = cursor.0;
    for row in rows {
        let (session_id, credit_json, updated_at, model, cwd) =
            row.map_err(|source| Error::Io { path: file.path.clone(), source: std::io::Error::other(source) })?;
        let updated_at = updated_at.max(0) as u64;
        high = high.max(updated_at);
        let credits = credit_json.map(|json| credit_sum(&json)).unwrap_or(0.0);
        let mut event = UsageEvent::new(crate::TOOL_ID, updated_at as i64, &session_id);
        event.counts.credits = credits;
        event.meter = Meter::Credits;
        event.model = model.filter(|m| !m.is_empty());
        event.project = Some(project_name(&cwd));
        event.dedupe_key = Some(format!("{DEDUPE_PREFIX}{session_id}"));
        event.source = file.key();
        events.push(event);
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

/// `/Users/sp/workspace/bug-hunter` → `bug-hunter`; the panel shows basenames.
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
 updated_at INTEGER NOT NULL DEFAULT 0, deleted_at INTEGER, model TEXT); \
CREATE TABLE session_usage (session_id TEXT PRIMARY KEY, used INTEGER NOT NULL, \
 size INTEGER NOT NULL, updated_at INTEGER NOT NULL, credit_json TEXT);";

    pub(crate) fn fixture(dir: &Path) -> SourceFile {
        let path = dir.join("workbuddy.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        // One completed, credited session; one completed free-model session
        // (no credits — still usage, zero credits); one deleted session; one
        // cloud session whose deleted_at is the app's own not-deleted mark;
        // one still-running session with credits mid-flight. The first three
        // plus the cloud one leave the adapter; the running one waits for its
        // close, the deleted one never comes back.
        conn.execute_batch(
            "INSERT INTO sessions(id, cwd, status, deleted_at, model) VALUES \
             ('s-done', '/Users/sp/workspace/bug-hunter', 'completed', NULL, 'deepseek-v4.1-flash'), \
             ('s-free', '/Users/sp/workspace/tokenme', 'completed', NULL, 'deepseek-v4.1-flash'), \
             ('s-gone', '/Users/sp/workspace/old', 'completed', 1790000000000, 'deepseek-v4.1-flash'), \
             ('s-cloud', '/Users/sp/workspace/cloud', 'completed', -1, 'hy4-preview-f'), \
             ('s-live', '/Users/sp/workspace/x', 'Pending', NULL, 'hy4-preview-f'); \
             INSERT INTO session_usage VALUES \
             ('s-done', 12345, 300000, 1000, '{\"a\": 9.27, \"b\": 17}'), \
             ('s-free', 999, 300000, 2000, NULL), \
             ('s-gone', 10, 300000, 1200, '{\"d\": 5.0}'), \
             ('s-cloud', 20, 300000, 1500, '{\"e\": 3.5}'), \
             ('s-live', 50, 300000, 3000, '{\"c\": 8.0}');",
        )
        .unwrap();
        drop(conn);
        let (size, mtime_ms) = crate::paths::source_stat(&path).unwrap();
        SourceFile { path, kind: usage_core::FileKind::Sqlite, size, mtime_ms }
    }

    #[test]
    fn every_completed_session_is_an_event_whether_or_not_it_was_metered() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let (outcome, high) = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(outcome.events.len(), 3, "{:?}", outcome.events);
        let by_key = |key: &str| {
            outcome
                .events
                .iter()
                .find(|e| e.dedupe_key.as_deref() == Some(key))
                .unwrap_or_else(|| panic!("{key} missing"))
        };
        let done = by_key("workbuddy#s-done");
        assert_eq!(done.counts.credits, 26.27);
        assert_eq!(done.meter, Meter::Credits);
        assert_eq!(done.model.as_deref(), Some("deepseek-v4.1-flash"));
        assert_eq!(done.project.as_deref(), Some("bug-hunter"));
        // The free session is real usage the vendor charged nothing for: it
        // counts as a session and carries no money.
        let free = by_key("workbuddy#s-free");
        assert_eq!(free.counts.credits, 0.0);
        assert_eq!(free.project.as_deref(), Some("tokenme"));
        assert_eq!(by_key("workbuddy#s-cloud").counts.credits, 3.5);
        assert!(!outcome.events.iter().any(|e| e.session == "s-gone"), "deleted rows stay out");
        assert!(!outcome.events.iter().any(|e| e.session == "s-live"), "a live session waits for its close");
        assert_eq!(high, 2000);
    }

    #[test]
    fn a_session_that_closes_later_is_emitted_with_its_credits() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let (first, _) = read(&file, ReadCursor(0)).unwrap();
        assert_eq!(first.events.len(), 3);
        let conn = Connection::open(&file.path).unwrap();
        conn.execute_batch(
            "UPDATE sessions SET status = 'completed' WHERE id = 's-live';
             UPDATE session_usage SET updated_at = 4000 WHERE session_id = 's-live'",
        )
        .unwrap();
        drop(conn);
        // The rescan re-emits everything — including rows already indexed, which
        // the ingest-side dedupe keys swallow — and the freshly closed session
        // lands complete with the credits written at its close.
        let (second, high) = read(&file, ReadCursor(2000)).unwrap();
        assert_eq!(second.events.len(), 4);
        let live = second
            .events
            .iter()
            .find(|e| e.dedupe_key.as_deref() == Some("workbuddy#s-live"))
            .unwrap();
        assert_eq!(live.counts.credits, 8.0);
        assert_eq!(high, 4000);
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

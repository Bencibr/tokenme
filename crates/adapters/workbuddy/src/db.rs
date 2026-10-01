//! The `sessions` census of `workbuddy.db` — deliberately *not* the usage source.
//!
//! WorkBuddy's desktop app meters in **credits** and keeps two accounts of the
//! same money: `session_usage.credit_json` is a `{model-hash: credits}` map
//! written when a session closes, and the transcripts in `projects/` carry
//! `providerData.rawUsage.credit` per call. They reconcile exactly — this
//! machine: 314.54 both ways over 10 sessions (measured 2026-09-29) — so reading
//! both would bill the account twice. [`crate::transcript`] wins because the
//! per-call side also carries the token stages, the model and the time, none of
//! which survive in the closed-session rollup: 8 of 10 sessions here have
//! `credit_json IS NULL` (a free-model session simply never gets a map), which is
//! the whole reason an earlier revision of this adapter reported WorkBuddy with
//! ten sessions, ten requests and zero tokens.
//!
//! `used`/`size` on the same table are context-window fullness (`size` is the
//! window: 300,000 or 1,000,000), not consumption — never import them as tokens.
//!
//! What the database stays good for is the vendor's own head count: `sessions` is
//! the app's sidebar, so `probe` can say how many conversations the user would
//! see when they open it.

use rusqlite::{Connection, OpenFlags};

use std::path::Path;

/// The rows that count as usage: closed sessions the user has not deleted.
/// The app's own cloud indexes treat `deleted_at = -1` as not deleted, and
/// local rows keep it NULL.
const USAGE_PREDICATE: &str =
    "s.status = 'completed' AND (s.deleted_at IS NULL OR s.deleted_at = -1)";

/// How many sessions the app lists. `None` when the database is absent or busy —
/// a census is never worth an error, and a locked file is the app's normal state.
pub fn count_sessions(path: &Path) -> Option<i64> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.query_row(
        &format!("SELECT count(*) FROM session_usage u JOIN sessions s ON s.id = u.session_id WHERE {USAGE_PREDICATE}"),
        [],
        |row| row.get(0),
    )
    .ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const DDL: &str = "\
CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT NOT NULL, status TEXT NOT NULL, \
 deleted_at INTEGER, model TEXT); \
CREATE TABLE session_usage (session_id TEXT PRIMARY KEY, used INTEGER NOT NULL, \
 size INTEGER NOT NULL, updated_at INTEGER NOT NULL, credit_json TEXT);";

    pub(crate) fn write_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(DDL).unwrap();
        // Deleted rows and still-running rows stay out of the census; a cloud row
        // marked with the app's own not-deleted sentinel (-1) counts.
        conn.execute_batch(
            "INSERT INTO sessions(id, cwd, status, deleted_at, model) VALUES \
             ('s-done', '/Users/sp/workspace/bug-hunter', 'completed', NULL, 'deepseek-v4.1-flash'), \
             ('s-gone', '/Users/sp/workspace/old', 'completed', 1790000000000, 'deepseek-v4.1-flash'), \
             ('s-cloud', '/Users/sp/workspace/cloud', 'completed', -1, 'hy4-preview-f'), \
             ('s-live', '/Users/sp/workspace/x', 'Pending', NULL, 'hy4-preview-f'); \
             INSERT INTO session_usage(session_id, used, size, updated_at, credit_json) VALUES \
             ('s-done', 12345, 300000, 1000, '{\"a\": 9.27, \"b\": 17}'), \
             ('s-gone', 10, 300000, 1200, '{\"d\": 5.0}'), \
             ('s-cloud', 20, 1000000, 1500, NULL), \
             ('s-live', 50, 300000, 3000, '{\"c\": 8.0}');",
        )
        .unwrap();
    }

    #[test]
    fn the_census_matches_the_sessions_the_app_lists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workbuddy.db");
        write_db(&path);
        assert_eq!(count_sessions(&path), Some(2), "one deleted, one still running");
    }

    #[test]
    fn an_absent_or_locked_database_has_no_census_rather_than_a_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_sessions(&dir.path().join("absent.db")), None);
    }
}

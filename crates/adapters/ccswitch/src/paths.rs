//! Locating CC Switch's own accounting database and opening it without ever
//! taking a lock the running app needs.
//!
//! CC Switch is a local proxy in front of Claude Code / Codex / Gemini CLI /
//! OpenCode / Pi / Grok Build / Mcode and keeps its **own** usage ledger. Reading
//! it is the only way to see gateway-side facts no tool log carries — which
//! provider actually answered, what the retry that never surfaced cost, and what
//! the traffic looked like on the wire.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// The table this adapter ingests. `usage_daily_rollups` is deliberately **not**
/// read: it is the `Cumulative` 30-day roll-up of the same rows
/// (`database/dao/usage_rollup.rs:116-140`), so taking both would bill every call
/// twice inside this one source.
pub(crate) const TABLE: &str = "proxy_request_logs";

/// Proves the table we ingest is queryable without scanning it. `EXISTS` always
/// returns exactly one row, so an *empty but valid* ledger still counts as usable
/// instead of falling through the ladder like a missing one.
pub(crate) const USABLE_SQL: &str = "SELECT EXISTS(SELECT 1 FROM proxy_request_logs LIMIT 1)";

fn encode_uri_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 8);
    for b in raw.as_bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'/' | b'-' | b'_' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn open_with_flags(path: &Path, query: &str, extra: OpenFlags) -> rusqlite::Result<Connection> {
    let uri = format!("file:{}?{query}", encode_uri_path(path));
    Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_URI | extra)
}

/// A connection that cannot block the app and, as far as the data goes, cannot
/// write.
///
/// Same ladder as the opencode/zcode adapters. The live file is in rollback-journal
/// mode (`PRAGMA journal_mode` answers `delete` on this machine), where plain
/// `mode=ro` already suffices; the ladder is there for a build or a restored copy
/// that turns WAL on, which breaks `nolock` (the wal-index lives in `-shm`) and
/// then breaks a read-only handle once the app exits and deletes `-shm`/`-wal`.
pub(crate) fn open_readonly(path: &Path, usable_sql: &str) -> rusqlite::Result<Connection> {
    let attempts = [
        ("mode=ro&nolock=1", OpenFlags::SQLITE_OPEN_READ_ONLY, false),
        ("mode=ro", OpenFlags::SQLITE_OPEN_READ_ONLY, false),
        ("mode=rw", OpenFlags::SQLITE_OPEN_READ_WRITE, true),
    ];
    let mut last = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(14),
        Some("no usable open mode".into()),
    );
    for (query, flags, fence) in attempts {
        match try_open(path, query, flags, fence, usable_sql) {
            Ok(conn) => return Ok(conn),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn try_open(path: &Path, query: &str, flags: OpenFlags, fence: bool, usable_sql: &str) -> rusqlite::Result<Connection> {
    let conn = open_with_flags(path, query, flags)?;
    if fence {
        // Only this mode needs fencing; the other two cannot write by definition.
        conn.execute_batch("PRAGMA query_only = 1;")?;
    }
    // A busy writer surfaces here, not at open time.
    let _ = conn.busy_timeout(std::time::Duration::from_millis(1500));
    conn.query_row(usable_sql, [], |r| r.get::<_, i64>(0)).map(|_| conn)
}

/// Where the running app keeps its database, or `None` when it is not there.
///
/// Only the two overrides CC Switch itself implements are honoured (verified in
/// `farion1231/cc-switch` @ main):
///
/// * `CC_SWITCH_TEST_HOME` replaces the *home* directory — `src-tauri/src/config.rs:22-29`
///   (`get_home_dir`, its documented test/debug isolation hook). There is no
///   `CC_SWITCH_DATA_DIR`: `config.rs:16` explicitly refuses to honour `HOME`,
///   because a third-party-injected `HOME` once made users' providers vanish.
/// * the in-app data-directory setting, persisted by the Tauri store in
///   `app_paths.json` under `app_config_dir_override`
///   (`src-tauri/src/app_store.rs:9,30-56`), which lives in Tauri's `app_data_dir`
///   = `dirs::config_dir()/com.ccswitch.desktop` (`tauri.conf.json:5`).
///
/// Default: `~/.cc-switch/cc-switch.db` (`src-tauri/src/database/mod.rs:99-101`).
pub(crate) fn db_path() -> Option<PathBuf> {
    let in_dir = |dir: PathBuf| {
        let p = dir.join("cc-switch.db");
        p.is_file().then_some(p)
    };
    if let Some(home) = env_home() {
        if let Some(db) = in_dir(home.join(".cc-switch")) {
            return Some(db);
        }
    }
    if let Some(dir) = store_override() {
        if let Some(db) = in_dir(dir) {
            return Some(db);
        }
    }
    dirs::home_dir().and_then(|h| in_dir(h.join(".cc-switch")))
}

fn env_home() -> Option<PathBuf> {
    std::env::var("CC_SWITCH_TEST_HOME").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Reads the one key of the app's own path store. A store we cannot parse, or that
/// names a directory which no longer exists, simply means "no override" — which is
/// also how the app itself treats it (`app_store.rs:46-53`).
fn store_override() -> Option<PathBuf> {
    let file = dirs::config_dir()?.join("com.ccswitch.desktop").join("app_paths.json");
    let raw = std::fs::read_to_string(file).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let dir = value.get("app_config_dir_override")?.as_str()?.trim();
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir))
}

/// Size + mtime for `discover`, with no `strftime` round-trip.
pub(crate) fn stat_file(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((meta.len(), mtime_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encodes_the_characters_that_would_break_a_uri() {
        let p = Path::new("/tmp/a b?c#d/cc-switch.db");
        let enc = encode_uri_path(p);
        assert!(!enc.contains(' ') && !enc.contains('?') && !enc.contains('#'));
        assert!(enc.ends_with("/cc-switch.db"));
        assert!(enc.contains("%20") && enc.contains("%3F") && enc.contains("%23"));
    }

    #[test]
    fn an_empty_or_blank_store_is_not_an_override() {
        // `{}` is exactly what this machine's store holds when the data directory
        // was never moved.
        let v: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert!(v.get("app_config_dir_override").and_then(|x| x.as_str()).is_none());
        for raw in [r#"{"app_config_dir_override":"  "}"#, "not json", r#"{"app_config_dir_override":42}"#] {
            let v: Option<serde_json::Value> = serde_json::from_str(raw).ok();
            let dir = v.and_then(|v| v.get("app_config_dir_override").and_then(|x| x.as_str()).map(str::to_string));
            assert!(dir.is_none() || dir.is_some_and(|d| d.trim().is_empty()), "{raw} must not become a path");
        }
    }

    /// The live database is in `delete` journal mode, but WAL is what the ladder
    /// exists for, so both are proven here.
    #[test]
    fn read_only_open_survives_both_journal_modes() {
        let dir = tempfile::tempdir().unwrap();
        for mode in ["delete", "wal"] {
            let path = dir.path().join(format!("{mode}.db"));
            let writer = Connection::open(&path).unwrap();
            writer.execute_batch(&format!("PRAGMA journal_mode={mode};")).unwrap();
            seed(&writer);
            assert_eq!(journal_mode(&writer), mode, "the fixture really is in {mode}");
            let conn = open_readonly(&path, USABLE_SQL).unwrap_or_else(|e| panic!("{mode}: {e}"));
            assert_eq!(count(&conn), 2);
            assert!(conn.execute("DELETE FROM proxy_request_logs", []).is_err(), "a read-only handle can never write");
            drop(conn);
            // A live writer must not stop us, and must not see us as a contender.
            assert_eq!(count(&open_readonly(&path, USABLE_SQL).unwrap()), 2, "{mode}: readable while the app holds it");
            writer.execute("INSERT INTO proxy_request_logs (request_id) VALUES ('c')", []).unwrap();
            assert_eq!(count(&open_readonly(&path, USABLE_SQL).unwrap()), 3, "{mode}: a fresh commit is not lost");
            drop(writer);
            // Journal helpers are gone now (the WAL case) — the ladder must adapt.
            assert_eq!(count(&open_readonly(&path, USABLE_SQL).unwrap()), 3, "{mode}: still readable after the app exited");
        }
    }

    #[test]
    fn an_empty_table_still_counts_as_our_own_database() {
        // `USABLE_SQL` is `EXISTS`, so `query_row` returns a row for 0 rows of
        // data: an app that has recorded nothing is detected, not skipped.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE proxy_request_logs (request_id TEXT PRIMARY KEY);").unwrap();
        drop(conn);
        assert!(open_readonly(&path, USABLE_SQL).is_ok());
    }

    #[test]
    fn a_database_without_our_table_is_not_ours() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foreign.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE providers (id TEXT PRIMARY KEY);").unwrap();
        drop(conn);
        assert!(open_readonly(&path, USABLE_SQL).is_err());
        assert!(open_readonly(&path.join("nope"), USABLE_SQL).is_err());
    }

    fn seed(conn: &Connection) {
        conn.execute_batch("CREATE TABLE proxy_request_logs (request_id TEXT PRIMARY KEY, model TEXT);")
            .unwrap();
        conn.execute("INSERT INTO proxy_request_logs VALUES ('a','m'), ('b','m')", []).unwrap();
    }

    fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM proxy_request_logs", [], |r| r.get(0)).unwrap()
    }

    fn journal_mode(conn: &Connection) -> String {
        conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap()
    }
}

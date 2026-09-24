//! Where AgnesCode keeps its data, which of its two trees is authoritative, and
//! how to open a WAL-live ledger without lying about what it holds.
//!
//! Measured on this machine (2026-09-24), all under `~/.agnes/`:
//!
//! | tree | holds | volume |
//! |---|---|---|
//! | `data/sessions/sessions.db` → `usage_ledger` | one row per billed model call | **554 rows / 4 sessions** (18 MiB, WAL) |
//! | `state/logs/llm/<session>/<ms>-<id>-<purpose>.jsonl` | the full request/response stream of individual calls | 125 files / 79 MB / 23,880 records, of which only **118** carry a `usage` object |
//! | `sessions.db` → `sessions.accumulated_*` | per-session roll-up of the same ledger | 4 rows |
//! | `sessions.db` → `messages` | chat history; its `tokens` column is NULL in all 2,392 rows | 2,392 rows |
//!
//! ## Authoritative source: `usage_ledger`
//! The ledger is the vendor's own billing table and the only complete one: 86 of
//! the 86 `usage` records that survive in `llm/` as `purpose:"main"` are inside it
//! (85 within 120 s of the ledger stamp, the other with the byte-identical
//! `(input, output)` pair 191 s later — the ledger timestamps the commit, not the
//! request). The ledger holds 554 calls against those 86, because the JSONL tree
//! is a per-call debug buffer that only 4 sessions' worth of it survives in.
//! Reading the JSONL *as well* would double-bill the overlap: the two id spaces do
//! not intersect (`<session>#<ledger id>` vs `<session>#<request_id>`), so the
//! index's `event_dedupe` cannot fold it. `discover` therefore hands out the db
//! when it exists and the JSONL only as a fallback (see [`crate::loader`]).
//!
//! What the fallback loses when the db is gone is documented in
//! [`crate::loader`]: the 30 `tool_pair_summary` and 2 `session_naming` usage
//! records in the JSONL are calls the vendor never entered in its own ledger.
//!
//! ## Env override
//! AgnesCode is a Goose fork (`agnesd --version` answers `goose-server 1.44.0`),
//! and the shipped `/Applications/AgnesCode.app/Contents/Resources/bin/agnesd`
//! carries exactly one root-dir override: the string `AGNES_PATH_ROOT` sits in its
//! `paths` module right next to `Agnes requires a home dir.` and `.agnes`, i.e.
//! `env("AGNES_PATH_ROOT") || home_dir().join(".agnes")` — Goose's
//! `GOOSE_PATH_ROOT` renamed. There is no `AGNES_HOME`/`AGNES_DATA_DIR`, and
//! `GOOSE_PATH_ROOT` is not in the binary at all, so only `AGNES_PATH_ROOT` is
//! honoured here; everything else (`AGNES_MODEL`, `AGNES_WORKING_DIR`,
//! `AGNES_TEMPORARY_WORKING_DIR`, …) configures a session, not this location.
//!
//! ## The db is WAL-live: never `immutable=1`
//! `sessions.db` is in `journal_mode=wal` and `sessions.db-wal`/`-shm` sit beside
//! it, so a handle must see the WAL. Measured on a scratch WAL db whose newest
//! frames are still uncheckpointed:
//!
//! | handle | rows seen |
//! |---|---|
//! | `file:t.db?mode=ro` | 5 |
//! | copy of `t.db` + `t.db-wal` + `t.db-shm` into a temp dir | 5 |
//! | `file:t.db?immutable=1` | `no such table: u` (0 rows, silently) |
//! | copy of `t.db` alone | `no such table: u` |
//!
//! `immutable` tells SQLite the file can never change, which switches the WAL
//! machinery off; the uncheckpointed frames (which on a fresh install include the
//! schema itself) are then simply not there. So [`open_ledger`] opens read-only
//! first and, only if that fails, copies all three files to a private temp dir and
//! reads the copy, whose lifetime is bound to the returned [`Ledger`].

use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// Read by AgnesCode itself: the only root override the shipped `agnesd` knows.
pub const ENV_PATH_ROOT: &str = "AGNES_PATH_ROOT";

/// The append-only per-call ledger, and the only table this adapter ingests.
/// `sessions.accumulated_*` is the roll-up of the same rows and is read for the
/// real-machine cross-check only — never as an event source.
pub const USAGE_TABLE: &str = "usage_ledger";

/// Holds `working_dir`, the project label. Optional: the ledger is billable
/// without it (see [`crate::loader::select_sql`]).
pub const SESSIONS_TABLE: &str = "sessions";

/// Relative to [`root`].
const DB_RELATIVE: &str = "data/sessions/sessions.db";

/// Relative to [`root`]: the fallback request/response buffer.
const LLM_LOGS_RELATIVE: &str = "state/logs/llm";

/// SQLite's WAL sidecars, spelled with a dash because that is what the vendor's
/// file names actually use (`sessions.db-wal`, not `sessions.wal`).
pub const WAL_SUFFIX: &str = "-wal";

pub const SHM_SUFFIX: &str = "-shm";

/// Suffix of every file under [`LLM_LOGS_RELATIVE`].
pub const LLM_LOG_SUFFIX: &str = ".jsonl";

/// `~/.agnes`, honouring `AGNES_PATH_ROOT` exactly the way `agnesd` does: a
/// non-empty value replaces the whole root, an empty one is ignored.
pub fn root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_PATH_ROOT) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|h| h.join(".agnes"))
}

/// `~/.agnes/data/sessions/sessions.db`, when it is there.
pub fn db_path() -> Option<PathBuf> {
    let p = root()?.join(DB_RELATIVE);
    p.is_file().then_some(p)
}

/// `~/.agnes/state/logs/llm`, when it is there.
pub fn llm_log_dir() -> Option<PathBuf> {
    let p = root()?.join(LLM_LOGS_RELATIVE);
    p.is_dir().then_some(p)
}

/// `<ms>-<request id prefix>-<purpose>.jsonl`, one file per logged request.
pub fn is_llm_log_file(path: &Path) -> bool {
    path.is_file() && path.file_name().and_then(|s| s.to_str()).is_some_and(|n| n.ends_with(LLM_LOG_SUFFIX) && n.len() > LLM_LOG_SUFFIX.len())
}

/// What the name of a fallback log file says about the call it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogIdentity {
    /// `meta.session_id`, which is also the directory the file lives in
    /// (`20260902_1`).
    pub session: String,
    /// Request start in epoch ms — the first name field is `timestamp_ms`
    /// verbatim, so this needs no unit promotion.
    pub ts_ms: i64,
    /// First 8 hex chars of `meta.request_id`.
    pub request_prefix: String,
    /// `main` / `tool_pair_summary` / `session_naming`.
    pub purpose: String,
}

/// `1788348486108-07179738-main.jsonl` inside `llm/20260902_1/` →
/// `Some({20260902_1, 1788348486108, "07179738", "main"})`.
///
/// The name is the fallback for a log whose `meta` line was already read in an
/// earlier pass, so a resumed tail read still attributes to the right session.
pub fn log_identity(path: &Path) -> Option<LogIdentity> {
    let name = path.file_name()?.to_str()?.strip_suffix(LLM_LOG_SUFFIX)?;
    let mut fields = name.splitn(3, '-');
    let (ms, prefix, purpose) = (fields.next()?, fields.next()?, fields.next()?);
    Some(LogIdentity {
        session: path.parent()?.file_name()?.to_str()?.to_string(),
        ts_ms: ms.parse().ok()?,
        request_prefix: prefix.to_string(),
        purpose: purpose.to_string(),
    })
}

/// A ledger handle plus whatever keeps it valid.
pub(crate) struct Ledger {
    conn: Connection,
    /// Only set when the original could not be opened: the copy is deleted when
    /// the handle drops, so a pass that reads a copy never leaves 18 MB behind.
    _copy: Option<TempDb>,
}

impl Ledger {
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// The temp directory holding the emergency copy, removed on drop.
struct TempDb {
    dir: PathBuf,
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A read-only handle on `path`, or `None` when AgnesCode has it locked in a way
/// this pass cannot work around.
///
/// Two rungs only, in this order: a plain `mode=ro` URI (which sees the WAL), then
/// the temp-dir copy. `immutable=1` is never tried — see the module docs.
pub(crate) fn open_ledger(path: &Path) -> Option<Ledger> {
    if let Ok(conn) = open_uri(path, "mode=ro", OpenFlags::SQLITE_OPEN_READ_ONLY) {
        match usable(&conn) {
            Ok(true) => return Some(Ledger { conn, _copy: None }),
            // It opened and answered: this is not our database, and copying 18 MB of
            // somebody else's file would not change that.
            Ok(false) => return None,
            // A busy or half-migrated handle is not a foreign db either: fall through
            // to the copy.
            Err(_) => {}
        }
    }
    let (dir, conn) = copy_and_open(path).ok()?;
    usable(&conn).ok().filter(|v| *v).map(|_| Ledger { conn, _copy: Some(TempDb { dir }) })
}

/// `mode=ro` on a path that is not ours (no `usage_ledger`) or cannot be opened,
/// and the copy rung then fails the same way, so the caller sees `None`.
fn copy_and_open(path: &Path) -> io::Result<(PathBuf, Connection)> {
    let dir = unique_temp_dir()?;
    let stem = path.file_name().and_then(|s| s.to_str()).ok_or_else(|| io::Error::other("no file name"))?;
    let mut copied = false;
    for suffix in ["", WAL_SUFFIX, SHM_SUFFIX] {
        let from = pathbuf_with_suffix(path, suffix);
        if from.is_file() {
            std::fs::copy(&from, dir.join(format!("{stem}{suffix}")))
                .map(|_| copied = true)
                .map_err(|e| io::Error::other(format!("{suffix} sidecar: {e}")))?;
        }
    }
    if !copied {
        return Err(io::Error::other("nothing to copy"));
    }
    let target = dir.join(stem);
    // The copy is opened read-write on purpose: recovering a WAL whose `-shm` was
    // copied stale needs write access to the *copy*. `query_only` fences it, and
    // a read-only handle is tried first anyway.
    let conn = open_uri(&target, "mode=rw", OpenFlags::SQLITE_OPEN_READ_WRITE)
        .or_else(|_| Connection::open(&target))
        .map_err(|e| io::Error::other(e.to_string()))?;
    let _ = conn.execute_batch("PRAGMA query_only = 1;");
    let _ = conn.busy_timeout(std::time::Duration::from_millis(1500));
    Ok((dir, conn))
}

/// A connection that cannot block the app and, on the write side, cannot.
fn open_uri(path: &Path, query: &str, extra: OpenFlags) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(format!("file:{}?{query}", encode_uri_path(path)), OpenFlags::SQLITE_OPEN_URI | extra)?;
    // A live writer surfaces as SQLITE_BUSY here, not at open time.
    let _ = conn.busy_timeout(std::time::Duration::from_millis(1500));
    Ok(conn)
}

/// The table we ingest is queryable, which is also what distinguishes this db from
/// any other SQLite file under the home dir. `EXISTS` answers one row either way,
/// so an *empty* ledger of our own shape still counts as ours. An `Err` means "could
/// not be read", not "not ours" — a busy writer, for instance.
fn usable(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)", [USAGE_TABLE], |r| r.get::<_, i64>(0))
        .map(|v| v == 1)
}

fn pathbuf_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", path.display()))
}

fn unique_temp_dir() -> io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("tokenme-agnes-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Percent-encodes everything a SQLite URI parser would otherwise reinterpret
/// (`?`, `#`, `%`), so any path round-trips into a `file:` URI.
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

/// Env vars are process-global, so every test that points the adapter at a temp
/// home holds this lock (shared across the crate's test modules).
#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_honours_only_agnes_path_root() {
        let _env = lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_PATH_ROOT, dir.path());
        assert_eq!(root().as_deref(), Some(dir.path()));
        assert!(db_path().is_none(), "nothing installed under the temp root");
        assert!(llm_log_dir().is_none());
        std::fs::create_dir_all(dir.path().join("data/sessions")).unwrap();
        std::fs::create_dir_all(dir.path().join("state/logs/llm")).unwrap();
        std::fs::write(dir.path().join(DB_RELATIVE), b"SQLite format 3\0").unwrap();
        assert_eq!(db_path().as_deref(), Some(dir.path().join(DB_RELATIVE).as_path()));
        assert_eq!(llm_log_dir().as_deref(), Some(dir.path().join(LLM_LOGS_RELATIVE).as_path()));
        // An empty value is ignored, exactly like `agnesd`'s own check.
        std::env::set_var(ENV_PATH_ROOT, "");
        assert_eq!(root().as_deref(), dirs::home_dir().map(|h| h.join(".agnes")).as_deref());
        std::env::remove_var(ENV_PATH_ROOT);
        assert_eq!(root().as_deref(), dirs::home_dir().map(|h| h.join(".agnes")).as_deref());
        assert!(db_path().is_some(), "the real ~/.agnes has a ledger on this machine");
    }

    #[test]
    fn log_file_names_carry_session_time_and_purpose() {
        let p = Path::new("/x/state/logs/llm/20260902_1/1788348486108-07179738-main.jsonl");
        let id = log_identity(p).unwrap();
        assert_eq!(id.session, "20260902_1");
        assert_eq!(id.ts_ms, 1_788_348_486_108);
        assert_eq!((id.request_prefix.as_str(), id.purpose.as_str()), ("07179738", "main"));
        let p = Path::new("/x/llm/20260806_1/1786108348416-58780ff8-tool_pair_summary.jsonl");
        let id = log_identity(p).unwrap();
        assert_eq!((id.session.as_str(), id.purpose.as_str()), ("20260806_1", "tool_pair_summary"));
        assert!(log_identity(Path::new("/x/llm/sess/plain.txt")).is_none(), "not a jsonl");
        assert!(log_identity(Path::new("/x/llm/nope.jsonl")).is_none(), "not the vendor's name shape");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("1788348486108-07179738-main.jsonl");
        assert!(!is_llm_log_file(&file), "not on disk yet");
        std::fs::write(&file, b"{}\n").unwrap();
        assert!(is_llm_log_file(&file));
        assert_eq!(log_identity(&file).unwrap().session, dir.path().file_name().unwrap().to_str().unwrap());
    }

    /// The hazard the module doc promises never to hit: an uncheckpointed WAL is
    /// invisible to `immutable=1` and to a copy that skipped the sidecars, while
    /// both a `mode=ro` handle and the three-file copy see every row.
    #[test]
    fn an_uncheckpointed_wal_is_only_readable_with_the_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode = wal; PRAGMA wal_autocheckpoint = 0;").unwrap();
        writer.execute("CREATE TABLE usage_ledger (id INTEGER PRIMARY KEY AUTOINCREMENT, input_tokens INTEGER)", []).unwrap();
        writer.execute("INSERT INTO usage_ledger (input_tokens) VALUES (7), (8), (9)", []).unwrap();
        assert!(dir.path().join("sessions.db-wal").is_file(), "the frames are still in the WAL");

        let handle = open_ledger(&path).expect("a ledger opened by this machine");
        assert_eq!(count(&handle), 3, "mode=ro sees the WAL");
        drop(handle);

        let (copy_dir, conn) = copy_and_open(&path).unwrap();
        let ledger = Ledger { conn, _copy: Some(TempDb { dir: copy_dir }) };
        assert_eq!(count(&ledger), 3, "db + -wal + -shm copied together: every row survives");
        drop(ledger);

        // What the forbidden shortcuts answer on the very same file: either an
        // error or a short count, never the three rows.
        let immutable = Connection::open_with_flags(format!("file:{}?immutable=1", path.display()), OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| count_of(&c))
            .ok();
        assert_ne!(immutable, Some(3), "immutable=1 lost the WAL, as promised (measured: {immutable:?})");
        let alone = dir.path().join("main-file-only.db");
        std::fs::copy(&path, &alone).unwrap();
        let copied = Connection::open(&alone).and_then(|c| count_of(&c)).ok();
        assert_ne!(copied, Some(3), "a copy without -wal loses the frames the same way (measured: {copied:?})");
    }

    #[test]
    fn a_foreign_or_absent_database_is_not_openable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open_ledger(&dir.path().join("absent.db")).is_none());
        let other = dir.path().join("other.db");
        let conn = Connection::open(&other).unwrap();
        conn.execute_batch("CREATE TABLE something_else (id INTEGER PRIMARY KEY); INSERT INTO something_else VALUES (1);")
            .unwrap();
        drop(conn);
        assert!(open_ledger(&other).is_none(), "no usage_ledger, not our business");
        // An empty ledger of our own shape still is ours.
        let empty = dir.path().join("empty.db");
        let conn = Connection::open(&empty).unwrap();
        conn.execute("CREATE TABLE usage_ledger (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
            .unwrap();
        drop(conn);
        assert!(open_ledger(&empty).is_some());
    }

    fn count(ledger: &Ledger) -> i64 {
        count_of(ledger.conn()).unwrap()
    }

    fn count_of(conn: &Connection) -> rusqlite::Result<i64> {
        conn.query_row("SELECT COUNT(*) FROM usage_ledger", [], |r| r.get(0))
    }
}

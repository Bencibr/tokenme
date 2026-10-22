//! Finding Antigravity's conversation databases and opening them without
//! disturbing a CLI or IDE that may be writing one right now.
//!
//! The IDE app premise "it stores nothing locally" was wrong: `~/.gemini/antigravity`
//! carries the same `conversations/*.db` schema and holds the bulk of the data
//! (measured across both roots: 35 databases, 52,058 decodable `gen_metadata`
//! rows, `#4.3 == #4.9 + #4.10` on every one of them). Every install tree is
//! read, but each by its own tool now: the desktop adapter takes
//! `antigravity-cli` / `antigravity` / `antigravity-backup`, the IDE adapter
//! takes `antigravity-ide`. `ANTIGRAVITY_DATA_DIR` and
//! `ANTIGRAVITY_IDE_DATA_DIR` are comma-separated lists and override that.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

use crate::Edition;

/// The desktop install's data-dir override.
pub(crate) const DATA_DIR_ENV: &str = "ANTIGRAVITY_DATA_DIR";
/// The IDE install's override. One shared list would bill the same databases
/// to both tools, so the editions each carry their own.
pub(crate) const IDE_DATA_DIR_ENV: &str = "ANTIGRAVITY_IDE_DATA_DIR";
pub(crate) const CONVERSATIONS: &str = "conversations";
/// Proves a connection can read the one table this adapter ingests. `max(rowid)`
/// seeks the table btree's rightmost page instead of scanning it.
pub(crate) const USABLE_SQL: &str = "SELECT max(rowid) FROM gen_metadata";
/// `probe`'s hint only. Run once, against one already-open database.
pub(crate) const COUNT_SQL: &str = "SELECT count(*) FROM gen_metadata";
/// The CLI's own summary index: 21 columns of titles, step counts and workspace
/// URIs, and no token data in any of them, so it is never a source file even
/// when a user points `ANTIGRAVITY_DATA_DIR` at the directory holding it.
pub(crate) const SUMMARIES_DB: &str = "conversation_summaries.db";
/// The CLI commits between our open and our query when a turn lands; a short
/// wait beats silently reporting nothing for that pass.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

impl crate::Edition {
    /// Data roots, in scan order: the edition's env override when set, else
    /// every install dir that shares this schema. The desktop keeps the
    /// original env name; `antigravity-ide` moves out of it to its own tool.
    pub(crate) fn roots(self, env: Option<&OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
        if let Some(list) = env.filter(|v| !v.is_empty()) {
            return list
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .collect();
        }
        let Some(home) = home else { return Vec::new() };
        let gemini = home.join(".gemini");
        let names: &[&str] = match self {
            Edition::Desktop => &["antigravity-cli", "antigravity", "antigravity-backup"],
            Edition::Ide => &["antigravity-ide"],
        };
        names.iter().map(|name| gemini.join(name)).collect()
    }
}

pub(crate) fn roots(edition: Edition) -> Vec<PathBuf> {
    let env = match edition {
        Edition::Desktop => std::env::var_os(DATA_DIR_ENV),
        Edition::Ide => std::env::var_os(IDE_DATA_DIR_ENV),
    };
    edition.roots(env.as_deref(), home().as_deref())
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// `conversations/` inside a root, or the root itself when it *is* a directory of
/// databases — the shape ccusage accepts so a user can point the env var at a
/// bare directory.
pub(crate) fn conversation_dir(root: &Path) -> PathBuf {
    let nested = root.join(CONVERSATIONS);
    if nested.is_dir() {
        nested
    } else {
        root.to_path_buf()
    }
}

/// `<size, mtime_ms>` for one regular file, `None` when it is absent.
pub(crate) fn stat_file(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    meta.is_file().then_some((meta.len(), mtime_ms))
}

/// A read-only handle plus whatever it had to do to get one.
pub(crate) struct Db {
    conn: Connection,
    /// Held so the sidecar copies are removed with the connection, and never
    /// before it: dropping the copy first would leave the handle pointing at a
    /// deleted inode.
    _copy: Option<TempCopy>,
}

impl Db {
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// Opens a conversation database read-only, falling back to a private copy.
///
/// The CLI owns these files and keeps them in WAL mode, which is why an
/// `immutable=1` open is wrong here rather than merely aggressive: uncheckpointed
/// rows live in the `-wal` sidecar, so an immutable read answers questions about
/// a database that is a checkpoint behind and silently loses the turns the user is
/// watching. `mode=ro` reads the sidecar properly, and needs the `-shm` wal-index
/// to exist and be creatable. When it cannot — a database copied from another
/// machine, a directory without write permission, a `-shm` left behind by a
/// crashed CLI — the last resort is to copy the database *and* its sidecars to a
/// private temp directory and read that, where SQLite may rebuild the wal-index.
/// Neither path writes to the source directory.
pub(crate) fn open(path: &Path) -> Option<Db> {
    match try_open(path) {
        Ok(conn) => Some(Db { conn, _copy: None }),
        // A database we can read but that has no `gen_metadata` is another tool's
        // file, or the summaries database; copying it cannot change the answer.
        Err(Open::NotOurDatabase) => None,
        Err(Open::Unavailable) => open_via_copy(path),
    }
}

fn open_via_copy(path: &Path) -> Option<Db> {
    let copy = TempCopy::new(path)?;
    let conn = open_readonly(&copy.db_path).or_else(|| {
        // The copy sits in a directory we own, so rebuilding the wal-index is the
        // one thing that can still succeed here. `query_only` fences it: the copy
        // is ours, but nothing in this adapter should be able to write anywhere.
        Connection::open(&copy.db_path)
            .and_then(|c| {
                c.busy_timeout(BUSY_TIMEOUT)?;
                c.execute_batch("PRAGMA query_only = 1;")?;
                Ok(c)
            })
            .ok()
    })?;
    conn.query_row(USABLE_SQL, [], |_| Ok(())).ok()?;
    Some(Db { conn, _copy: Some(copy) })
}

/// Why a plain read-only open did not produce a usable handle.
#[derive(Debug)]
enum Open {
    /// Opened and read, but the table this adapter ingests is not there.
    NotOurDatabase,
    /// Locked, unreadable, or a WAL database whose wal-index cannot be created.
    Unavailable,
}

fn try_open(path: &Path) -> Result<Connection, Open> {
    let conn = open_readonly(path).ok_or(Open::Unavailable)?;
    match conn.query_row(USABLE_SQL, [], |row| row.get::<_, Option<i64>>(0)) {
        Ok(_) => Ok(conn),
        Err(err) if missing_table(&err) => Err(Open::NotOurDatabase),
        Err(_) => Err(Open::Unavailable),
    }
}

/// SQLite's "no such table" answer, which is the only error that means the file
/// is fine and simply not ours.
fn missing_table(err: &rusqlite::Error) -> bool {
    match err {
        rusqlite::Error::SqliteFailure(_, Some(message)) => message.starts_with("no such table"),
        _ => false,
    }
}

fn open_readonly(path: &Path) -> Option<Connection> {
    let uri = format!(
        "file:{}?mode=ro&busy_timeout={}",
        encode_uri_path(path),
        BUSY_TIMEOUT.as_millis()
    );
    let conn =
        Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.busy_timeout(BUSY_TIMEOUT).ok()?;
    Some(conn)
}

/// Percent-encodes the bytes a `file:` URI cannot carry verbatim.
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

/// A private directory holding a copy of one database and its WAL sidecars.
struct TempCopy {
    dir: PathBuf,
    db_path: PathBuf,
}

impl TempCopy {
    fn new(source: &Path) -> Option<Self> {
        let name = source.file_name().and_then(OsStr::to_str).unwrap_or("conversation.db");
        let stamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("tokenme-antigravity-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let db_path = dir.join(name);
        std::fs::copy(source, &db_path).ok()?;
        // `-wal` carries the rows that make the read current, so a copy without
        // it is a copy of the past. `-shm` comes along for the same reason:
        // SQLite reconciles it against the copied `-wal` header instead of
        // trusting a stale one.
        for suffix in ["-wal", "-shm"] {
            let from = PathBuf::from(format!("{}{suffix}", source.display()));
            if from.is_file() {
                let _ = std::fs::copy(&from, dir.join(format!("{name}{suffix}")));
            }
        }
        Some(Self { dir, db_path })
    }
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_list_and_default_root_both_split_correctly() {
        assert_eq!(
            Edition::Desktop.roots(Some(OsStr::new(" /tmp/a , /tmp/b ,, ")), None),
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
        let desktop = PathBuf::from("/home/u/.gemini");
        assert_eq!(
            Edition::Desktop.roots(None, Some(Path::new("/home/u"))),
            vec![desktop.join("antigravity-cli"), desktop.join("antigravity"), desktop.join("antigravity-backup")]
        );
        assert_eq!(
            Edition::Ide.roots(None, Some(Path::new("/home/u"))),
            vec![desktop.join("antigravity-ide")],
            "the IDE tree is its own tool's only root, never the desktop's"
        );
        assert_eq!(
            Edition::Ide.roots(Some(OsStr::new("")), Some(Path::new("/home/u"))),
            Edition::Ide.roots(None, Some(Path::new("/home/u"))),
            "an empty override means the defaults"
        );
        assert!(Edition::Desktop.roots(None, None).is_empty(), "no home and no override means no roots");
    }

    #[test]
    fn a_conversations_child_dir_wins_over_the_bare_root() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("root");
        std::fs::create_dir_all(bare.join(CONVERSATIONS)).unwrap();
        assert_eq!(conversation_dir(&bare), bare.join(CONVERSATIONS));
        // Pointing the env var straight at a directory of databases works too.
        let no_children = dir.path().join("plain");
        std::fs::create_dir_all(&no_children).unwrap();
        assert_eq!(conversation_dir(&no_children), no_children);
    }

    #[test]
    fn percent_encoding_keeps_a_uri_parseable() {
        let enc = encode_uri_path(Path::new("/tmp/a b?c#d/x.db"));
        assert!(!enc.contains(' ') && !enc.contains('?') && !enc.contains('#'));
        assert!(enc.ends_with("/x.db") && enc.contains("%20") && enc.contains("%3F"));
    }

    #[test]
    fn stat_reports_size_and_mtime_and_refuses_directories() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        std::fs::write(&db, b"hello").unwrap();
        let (size, mtime) = stat_file(&db).expect("a file stats");
        assert_eq!(size, 5);
        assert!(mtime > 1_700_000_000_000, "mtime is milliseconds, got {mtime}");
        assert!(stat_file(dir.path()).is_none(), "a directory is not a source file");
        assert!(stat_file(&dir.path().join("absent.db")).is_none());
    }

    /// The whole reason the ladder exists: a read-only handle must see rows that
    /// are still sitting in the `-wal`, and an `immutable`-style read would not.
    #[test]
    fn read_only_open_sees_uncheckpointed_wal_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("c.db");
        let writer = Connection::open(&db).unwrap();
        writer.execute_batch("PRAGMA journal_mode=wal; CREATE TABLE gen_metadata (idx integer PRIMARY KEY, data blob);").unwrap();
        writer.execute("INSERT INTO gen_metadata VALUES (1, x'0801')", []).unwrap();
        let conn = try_open(&db).expect("a live WAL db opens read-only");
        assert_eq!(count(&conn), 1);
        assert!(conn.execute("DELETE FROM gen_metadata", []).is_err(), "the handle cannot write");
        writer.execute("INSERT INTO gen_metadata VALUES (2, x'0802')", []).unwrap();
        let conn = try_open(&db).expect("the second row is not lost to the wal");
        assert_eq!(count(&conn), 2, "committed-but-uncheckpointed rows are visible");
    }

    /// The fallback exists for a read-only handle that cannot get a wal-index:
    /// the copy carries the `-wal` rows, leaves the source untouched, and its
    /// directory goes away with the handle. Driven through `open` so the whole
    /// ladder runs, but with the writer locked so `try_open` must fail — left to
    /// timing, SQLite can join this process's live wal-index and skip the copy.
    #[cfg(unix)]
    #[test]
    fn a_private_copy_serves_the_wal_and_is_removed_with_its_handle() {
        use std::collections::BTreeSet;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let conversations = dir.path().join("conversations");
        std::fs::create_dir_all(&conversations).unwrap();
        let db = conversations.join("c.db");
        {
            let first = Connection::open(&db).unwrap();
            first
                .execute_batch("CREATE TABLE gen_metadata (idx integer PRIMARY KEY, data blob); INSERT INTO gen_metadata VALUES (1, x'0801');")
                .unwrap();
        }
        let writer = Connection::open(&db).unwrap();
        writer.execute_batch("PRAGMA journal_mode=wal; PRAGMA wal_autocheckpoint=0;").unwrap();
        writer.execute("INSERT INTO gen_metadata VALUES (2, x'0802')", []).unwrap();
        writer.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;").unwrap();
        std::fs::remove_file(conversations.join("c.db-shm")).unwrap();
        let listing = || -> BTreeSet<std::ffi::OsString> {
            std::fs::read_dir(&conversations).unwrap().flatten().map(|e| e.file_name()).collect()
        };
        let before = listing();
        let bytes = std::fs::read(&db).unwrap();
        std::fs::set_permissions(&conversations, std::fs::Permissions::from_mode(0o555)).unwrap();

        let handle = open(&db).expect("the copy provides a handle");
        let copy_dir = handle._copy.as_ref().expect("the handle is the private copy").dir.clone();
        assert_eq!(count(handle.conn()), 2, "the -wal row is served");
        assert_eq!(listing(), before, "the source directory gained nothing");
        assert_eq!(std::fs::read(&db).unwrap(), bytes, "and the database was never rewritten");
        drop(handle);
        assert!(!copy_dir.exists(), "the copy leaves with the handle");

        std::fs::set_permissions(&conversations, std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(writer);
    }

    fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM gen_metadata", [], |r| r.get(0)).unwrap()
    }
}

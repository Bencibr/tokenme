//! Locating an OpenCode-dialect SQLite store and opening it without ever
//! touching a lock the running app holds.
//!
//! The coordinates below are the *only* thing a sibling product changes: a fork
//! that ships OpenCode's `message` table and `message.data` JSON needs the
//! parser, the SQL and the WAL ladder in this crate unchanged, and a different
//! directory, file name and tool id.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

/// One product that speaks the OpenCode dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Product {
    /// Tool id stamped onto every event this product's rows produce.
    pub id: &'static str,
    pub display: &'static str,
    /// Names the data directory. This is tokenme's own override, following
    /// `OPENCODE_DATA_DIR`: the siblings publish no documented variable of
    /// their own, so a test (or a relocated install) is the only thing that sets it.
    pub env_dir: &'static str,
    /// Default data directory, relative to `$HOME`. OpenCode-family apps keep
    /// `~/.local/share/<product>` on every platform, including macOS, so the
    /// platform `data_dir()` helper is deliberately not used.
    pub default_rel: &'static [&'static str],
    /// The database file name when it is stable.
    pub db_file: &'static str,
    /// True when the product's directory holds **several** readable stores
    /// instead of one named file: Crow5 keeps `crow5.db` (its history) beside
    /// `opencode-powerformer-v<version>.db` (the live one, renamed on every
    /// upgrade). Every `*.db` that carries our table is then a source — picking
    /// one by mtime would silently drop 99 % of the history the moment either
    /// file is touched, and the two hold disjoint message ids.
    pub multi_db: bool,
    /// The desktop app's Electron userData subdirectory (`data_dir()`-relative),
    /// when the product ALSO ships as a desktop app writing there. Both roots
    /// are read: measured 2026-09-29 on macOS, `~/.local/share/crow5` went quiet
    /// on Sep 23-27 while the Crow5 Desktop app wrote every new session into
    /// `Application Support/com.crow5.desktop/crow5/opencode.db` — disjoint
    /// message ids, so the union bills each call exactly once.
    pub desktop_rel: &'static [&'static str],
}

pub(crate) static OPENCODE: Product = Product {
    id: "opencode",
    display: "OpenCode",
    env_dir: "OPENCODE_DATA_DIR",
    default_rel: &[".local", "share", "opencode"],
    db_file: "opencode.db",
    multi_db: false,
    desktop_rel: &[],
};

/// Crow5, an OpenCode fork. Same `message` table, same `data` JSON.
pub(crate) static CROW5: Product = Product {
    id: "crow5",
    display: "Crow5",
    env_dir: "CROW5_DATA_DIR",
    default_rel: &[".local", "share", "crow5"],
    // The name a single-store product would use; for a `multi_db` product this is
    // only the fallback when the scan below finds nothing readable.
    db_file: "crow5.db",
    multi_db: true,
    desktop_rel: &["com.crow5.desktop", "crow5"],
};

/// Mimocode, an OpenCode fork. Same `message` table, same `data` JSON, and its
/// `message.data` does carry a usage payload (45 of the 51 rows here are
/// billable assistant records).
pub(crate) static MIMOCODE: Product = Product {
    id: "mimocode",
    display: "Mimocode",
    env_dir: "MIMOCODE_DATA_DIR",
    default_rel: &[".local", "share", "mimocode"],
    db_file: "mimocode.db",
    multi_db: false,
    desktop_rel: &[],
};

/// OpenCode's data directory. The adapters resolve through [`data_dir_for`] with
/// [`OPENCODE`]; this stays as the frozen OpenCode entry point, pinned to it by
/// `the_opencode_helpers_and_the_product_table_agree`.
#[allow(dead_code)] // frozen surface: behaviour must not change for OpenCode
pub(crate) fn data_dir() -> Option<PathBuf> {
    data_dir_for(&OPENCODE)
}

pub(crate) fn data_dir_for(product: &Product) -> Option<PathBuf> {
    // The frozen single-dir surface: join WITHOUT the existence filter — a
    // missing directory is the caller's business (db_path_for falls back to
    // the single-store name). Discovery goes through data_dirs_for, which
    // filters on existence; this must keep answering the plain join or the
    // frozen contract above breaks on machines without the tool installed.
    if let Ok(v) = std::env::var(product.env_dir) {
        if let Some(first) = split_paths(&v).first() {
            return Some(first.clone());
        }
    }
    let mut dir = dirs::home_dir()?;
    for part in product.default_rel {
        dir = dir.join(part);
    }
    Some(dir)
}

/// A comma-separated list, because a relocated or containerised install has to
/// be able to name more than one root — the same convention `CLAUDE_CONFIG_DIR`
/// already uses in this codebase.
fn split_paths(value: &str) -> Vec<PathBuf> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Every existing data directory for the product, priority order. The env
/// override pins exactly the roots it names; otherwise the `XDG_DATA_HOME` root
/// (which the upstream product honours, and a Nix or container install points
/// somewhere else with it), the `~/.local/share` root, and the desktop app's
/// Electron userData root all count when they exist — the desktop product writes
/// there while the legacy root keeps the older history, and the stores hold
/// disjoint message ids. Duplicates are dropped: two roots naming the same
/// directory would put one file in the manifest twice.
pub(crate) fn data_dirs_for(product: &Product) -> Vec<PathBuf> {
    if let Ok(v) = std::env::var(product.env_dir) {
        return dedupe(split_paths(&v)).into_iter().filter(|p| p.is_dir()).collect();
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let xdg = PathBuf::from(xdg);
        // A relative XDG root is undefined by the spec, so it is ignored rather
        // than resolved against an arbitrary working directory.
        if xdg.is_absolute() {
            let mut dir = xdg;
            // The product's own directory name is the last segment of the
            // home-relative default; that is what hangs off XDG_DATA_HOME.
            if let Some(name) = product.default_rel.last() {
                dir = dir.join(name);
            }
            dirs.push(dir);
        }
    }
    let mut legacy = match dirs::home_dir() {
        Some(h) => h,
        None => PathBuf::new(),
    };
    if !legacy.as_os_str().is_empty() {
        for part in product.default_rel {
            legacy = legacy.join(part);
        }
        dirs.push(legacy);
    }
    if !product.desktop_rel.is_empty() {
        if let Some(data) = dirs::data_dir() {
            let mut desktop = data;
            for part in product.desktop_rel {
                desktop = desktop.join(part);
            }
            dirs.push(desktop);
        }
    }
    dedupe(dirs).into_iter().filter(|p| p.is_dir()).collect()
}

fn dedupe(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths.into_iter().filter(|p| seen.insert(p.clone())).collect()
}

/// OpenCode's database file, i.e. its name joined on, never globbed. See
/// [`data_dir`] for why this stays alongside [`db_path_for`].
#[allow(dead_code)] // frozen surface: behaviour must not change for OpenCode
pub(crate) fn db_path(dir: &Path) -> PathBuf {
    db_path_for(dir, &OPENCODE)
}

pub(crate) fn db_path_for(dir: &Path, product: &Product) -> PathBuf {
    db_list_for(dir, product)
        .into_iter()
        .next()
        .unwrap_or_else(|| dir.join(product.db_file))
}

/// Every database of this product that we can actually read, newest first.
///
/// A single-store product (OpenCode, Mimocode) has exactly one name, so it is
/// joined, never globbed. A [`Product::multi_db`] product is scanned: candidates
/// SQLite will not open, or that carry no `message` table, are skipped — but the
/// rest are *all* sources, because the app keeps its history and its live store
/// in separate files with disjoint row ids.
pub(crate) fn db_list_for(dir: &Path, product: &Product) -> Vec<PathBuf> {
    if !product.multi_db {
        return vec![dir.join(product.db_file)];
    }
    let found = usable_dbs(dir);
    if found.is_empty() {
        return vec![dir.join(product.db_file)];
    }
    found.into_iter().map(|(path, _)| path).collect()
}

/// `*.db` files in `dir` that this dialect can actually read, newest first.
fn usable_dbs(dir: &Path) -> Vec<(PathBuf, i64)> {
    let mut found: Vec<(PathBuf, i64)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension() != Some(OsStr::new("db")) {
                return None;
            }
            let (size, mtime_ms) = stat_file(&path)?;
            if size == 0 || open_readonly(&path, USABLE_SQL).is_err() {
                return None;
            }
            Some((path, mtime_ms))
        })
        .collect();
    // Newest first, and on a tie the name, so the choice never depends on the
    // order the directory happens to be stored in.
    found.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    found
}

/// The label `probe` shows for a resolved file: only a multi-store product has a
/// choice to report.
pub(crate) fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

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

/// Proves a connection can read a store of this dialect, and does it without
/// touching a data page: the old `count(*) FROM message` proof walked the whole
/// table on every cold open, which on a multi-gigabyte live store is the cost
/// [`RUNG_TTL`] exists to avoid.
///
/// Either table name qualifies, because the dialect has two generations of record
/// table ([`DIALECT_TABLES`]) and a product mid-migration carries both.
pub(crate) const USABLE_SQL: &str =
    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name IN ('message', 'session_message') LIMIT 1";

/// The record tables this dialect has used, in the order they were introduced.
/// The second is OpenCode's v2 shape: the role moves out of `data` into a
/// `type` column and a `seq` orders the session, which is why a row read from it
/// must not have its role re-derived from JSON the column already settled.
pub(crate) const DIALECT_TABLES: [(&str, bool); 2] = [("message", false), ("session_message", true)];

/// The columns a table must carry to be this dialect's record table at all.
const REQUIRED_COLUMNS: [&str; 4] = ["id", "session_id", "data", "time_created"];

/// The record table one database bills from, and what was already consumed.
pub(crate) struct Dialect {
    pub table: &'static str,
    /// True when the role lives in a `type` column rather than in `data`.
    pub role_from_column: bool,
    /// The table's own rowid high-water mark — an O(1) b-tree probe.
    pub max_rowid: i64,
    /// The sibling table's high-water mark, when it exists: the cursor sitting
    /// exactly there belongs to the sibling, not to the table we are reading.
    pub sibling_max: Option<i64>,
}

/// Which dialect table this store writes, if any.
///
/// The one with the most rows decides, and a tie goes to `message` because that
/// is the table the dialect has always had. A product that flips its write target
/// mid-life is the reason [`Dialect::sibling_max`] exists: see the cursor rule in
/// [`crate::loader`].
pub(crate) fn dialect(conn: &Connection) -> Option<Dialect> {
    let mut best: Option<Dialect> = None;
    let mut other: Option<i64> = None;
    for (table, role_from_column) in DIALECT_TABLES {
        if !has_columns(conn, table) {
            continue;
        }
        let Ok(max_rowid) = conn.query_row(
            &format!("SELECT COALESCE(MAX(rowid), 0) FROM {table}"),
            [],
            |row| row.get::<_, i64>(0),
        ) else {
            continue;
        };
        match &best {
            Some(winner) if winner.max_rowid >= max_rowid => other = other.max(Some(max_rowid)),
            _ => {
                other = best.map(|w| w.max_rowid);
                best = Some(Dialect { table, role_from_column, max_rowid, sibling_max: None });
            }
        }
    }
    let mut chosen = best?;
    chosen.sibling_max = other;
    Some(chosen)
}

/// True when `table` exists and carries every [`REQUIRED_COLUMNS`] entry. One
/// `LIMIT 0` prepare answers both at once: SQLite gives no column list for a
/// table that is not there.
fn has_columns(conn: &Connection, table: &str) -> bool {
    let Ok(stmt) = conn.prepare(&format!("SELECT * FROM {table} LIMIT 0")) else {
        return false;
    };
    let names: Vec<String> = (0..stmt.column_count())
        .filter_map(|i| stmt.column_name(i).ok().map(str::to_string))
        .collect();
    REQUIRED_COLUMNS.iter().all(|c| names.iter().any(|n| n == c))
}

/// How long a proven ladder rung is reused without re-running its proof. The
/// proof walks the whole table — seconds on a multi-gigabyte live store — and
/// discover + read repeat it on every ingest pass. A rung only stops being the
/// right one when the writer flips between running and exited; the first bad
/// open under it forgets it (`forget_rung`), and this TTL is the backstop.
const RUNG_TTL: Duration = Duration::from_secs(30);

const LADDER: [(&str, OpenFlags, bool); 3] = [
    ("mode=ro&nolock=1", OpenFlags::SQLITE_OPEN_READ_ONLY, false),
    ("mode=ro", OpenFlags::SQLITE_OPEN_READ_ONLY, false),
    ("mode=rw", OpenFlags::SQLITE_OPEN_READ_WRITE, true),
];

/// The ladder rung last proven to open `path`, and when it was proven.
static RUNG_CACHE: Mutex<Option<std::collections::HashMap<PathBuf, (usize, Instant)>>> = Mutex::new(None);

fn cached_rung(path: &Path) -> Option<usize> {
    let map = RUNG_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.as_ref()?;
    let (rung, at) = map.get(path)?;
    (at.elapsed() < RUNG_TTL).then_some(*rung)
}

fn remember_rung(path: &Path, rung: usize) {
    let mut guard = RUNG_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert_with(Default::default).insert(path.to_path_buf(), (rung, Instant::now()));
}

/// Drops the cached rung after a failed open or read, so the next attempt
/// re-runs the full ladder instead of trusting a rung that stopped working.
pub(crate) fn forget_rung(path: &Path) {
    let mut guard = RUNG_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = guard.as_mut() {
        map.remove(path);
    }
}

fn open_rung(path: &Path, rung: usize) -> rusqlite::Result<Connection> {
    let (query, flags, fence) = LADDER[rung];
    let conn = open_with_flags(path, query, flags)?;
    if fence {
        conn.execute_batch("PRAGMA query_only = 1;")?;
    }
    Ok(conn)
}

/// Cache-hit proof: schema presence only. The real proof of a rung is the read
/// it serves; when that read fails, the caller forgets the rung.
fn cheap_prove(conn: &Connection) -> rusqlite::Result<()> {
    conn.query_row(USABLE_SQL, [], |_| Ok(()))?;
    Ok(())
}

fn open_with_flags(path: &Path, query: &str, extra: OpenFlags) -> rusqlite::Result<Connection> {
    let uri = format!("file:{}?{query}", encode_uri_path(path));
    Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_URI | extra)
}

/// A connection that cannot block the app and, as far as the data goes, cannot
/// write.
///
/// The OpenCode app owns this database and keeps it in WAL mode, which leaves two
/// failure modes that SQLite reports as an *open that succeeds and a read that
/// fails*, hence the ladder and the usability check:
///
/// 1. `mode=ro&nolock=1` never takes a file lock, but a WAL database needs its
///    wal-index in `-shm`, so the read of `-wal` data fails.
/// 2. `mode=ro` reads a live database fine, but once the app has exited cleanly it
///    deletes `-shm`/`-wal` while the header still says WAL, and a read-only handle
///    may not recreate them.
/// 3. `mode=rw` exists only so SQLite may rebuild `-shm`; it is immediately fenced
///    with `PRAGMA query_only`, so this adapter still cannot write.
///
/// `usable_sql` counts the table the caller is about to read, which is what makes
/// a half-broken open fall through instead of surfacing as a read error.
pub(crate) fn open_readonly(path: &Path, usable_sql: &str) -> rusqlite::Result<Connection> {
    // A rung proven recently is opened directly, proved by schema presence
    // only: re-running the full-table proof on every ingest pass is what made
    // a 2.5 GB live store cost a b-tree walk per pass.
    if let Some(rung) = cached_rung(path) {
        match open_rung(path, rung).and_then(|conn| {
            cheap_prove(&conn)?;
            Ok(conn)
        }) {
            Ok(conn) => return Ok(conn),
            Err(_) => forget_rung(path),
        }
    }
    let mut last = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(14), Some("no usable open mode".into()));
    for (rung, (query, flags, fence)) in LADDER.iter().enumerate() {
        match try_open(path, query, *flags, *fence, usable_sql) {
            Ok(c) => {
                remember_rung(path, rung);
                return Ok(c);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn try_open(path: &Path, query: &str, flags: OpenFlags, fence: bool, usable_sql: &str) -> rusqlite::Result<Connection> {
    let conn = open_with_flags(path, query, flags)?;
    if fence {
        conn.execute_batch("PRAGMA query_only = 1;")?;
    }
    conn.query_row(usable_sql, [], |_| Ok(()))?;
    Ok(conn)
}

/// `%W`-free file size probe used by `discover`.
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

    /// `set_var` is process-global, so the tests that touch the roots below share
    /// one lock instead of racing each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A relocated or containerised install has to be able to name more than one
    /// root, and a root that is not there must not become a phantom source.
    #[test]
    fn the_env_override_is_a_list_of_existing_directories() {
        let _g = ENV_LOCK.lock().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let missing = a.path().join("absent");
        std::env::set_var("MIMOCODE_DATA_DIR", format!("{},{}", a.path().display(), missing.display()));
        assert_eq!(data_dirs_for(&MIMOCODE), vec![a.path().to_path_buf()], "the missing root drops out");
        std::env::set_var("MIMOCODE_DATA_DIR", format!("{},{}", missing.display(), b.path().display()));
        assert_eq!(data_dirs_for(&MIMOCODE), vec![b.path().to_path_buf()], "order follows the list");
        // The frozen single-root surface answers with the first entry it was given.
        std::env::set_var("MIMOCODE_DATA_DIR", format!("{},{}", b.path().display(), a.path().display()));
        assert_eq!(data_dir_for(&MIMOCODE), Some(b.path().to_path_buf()), "first named, first used");
        std::env::remove_var("MIMOCODE_DATA_DIR");
    }

    /// `XDG_DATA_HOME` is the root the upstream product itself honours, so a store
    /// relocated that way used to be invisible here.
    #[test]
    fn xdg_data_home_is_another_root_and_never_the_same_one_twice() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("OPENCODE_DATA_DIR");
        let xdg = tempfile::tempdir().unwrap();
        let relocated = xdg.path().join("opencode");
        std::fs::create_dir_all(&relocated).unwrap();
        let legacy = dirs::home_dir().unwrap().join(".local").join("share").join("opencode");

        std::env::set_var("XDG_DATA_HOME", xdg.path());
        let got = data_dirs_for(&OPENCODE);
        assert_eq!(got.first(), Some(&relocated), "the XDG root is read, and first");
        if legacy.is_dir() {
            assert!(got.contains(&legacy), "the home-relative root keeps counting beside it: {got:?}");
            // XDG resolving to the very directory the legacy root already names
            // must not put one store into the manifest twice.
            std::env::set_var("XDG_DATA_HOME", legacy.parent().unwrap());
            let once = data_dirs_for(&OPENCODE);
            assert_eq!(once.iter().filter(|p| *p == &legacy).count(), 1, "duplicates collapse: {once:?}");
        }

        // A relative XDG root is undefined by the spec, so it is ignored rather
        // than resolved against whatever the process happens to be doing.
        std::env::set_var("XDG_DATA_HOME", "not/absolute");
        assert!(!data_dirs_for(&OPENCODE).iter().any(|p| p.to_string_lossy().starts_with("not/")));
        std::env::remove_var("XDG_DATA_HOME");
    }

    #[test]
    fn percent_encodes_the_characters_that_would_break_a_uri() {
        let p = Path::new("/tmp/a b?c#d/opencode.db");
        let enc = encode_uri_path(p);
        assert!(!enc.contains(' ') && !enc.contains('?') && !enc.contains('#'));
        assert!(enc.ends_with("/opencode.db"));
        assert!(enc.contains("%20") && enc.contains("%3F") && enc.contains("%23"));
    }

    /// The live OpenCode database is WAL, which is why the ladder exists:
    /// `nolock` cannot serve committed-but-uncheckpointed rows, and a purely
    /// read-only handle cannot reopen a database whose `-shm` was cleaned when the
    /// app exited.
    #[test]
    fn read_only_open_survives_both_wal_states() {
        let dir = tempfile::tempdir().unwrap();

        // (a) WAL with the app still holding it.
        let live = dir.path().join("live.db");
        let writer = Connection::open(&live).unwrap();
        writer.execute_batch("PRAGMA journal_mode=wal;").unwrap();
        seed(&writer);
        let conn = open_readonly(&live, USABLE_SQL).expect("a live WAL db is readable");
        assert_eq!(mode_of(&conn), "wal", "we are really testing a WAL database");
        assert_eq!(count(&conn), 2, "rows still sitting in the -wal are visible");
        assert!(conn.execute("DELETE FROM message", []).is_err(), "a read-only handle can never write");
        drop(conn);
        writer.execute("INSERT INTO message VALUES ('c','{}')", []).unwrap();
        let conn = open_readonly(&live, USABLE_SQL).unwrap();
        assert_eq!(count(&conn), 3, "a row the app just committed is not lost to us");
        drop(conn);
        drop(writer);

        // (b) WAL after the app exited cleanly: the helpers are gone.
        let closed = dir.path().join("closed.db");
        let w = Connection::open(&closed).unwrap();
        w.execute_batch("PRAGMA journal_mode=wal;").unwrap();
        seed(&w);
        drop(w);
        assert_eq!(count(&open_readonly(&closed, USABLE_SQL).expect("a closed WAL db is still readable")), 2);
    }

    fn seed(conn: &Connection) {
        conn.execute_batch("CREATE TABLE message (id text PRIMARY KEY, data text NOT NULL);").unwrap();
        conn.execute("INSERT INTO message VALUES ('a','{}'), ('b','{}')", []).unwrap();
    }

    fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM message", [], |r| r.get(0)).unwrap()
    }

    fn mode_of(conn: &Connection) -> String {
        conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap()
    }

    /// A versioned product cannot join its file name, so this is the whole
    /// resolution rule: newest `*.db` that carries the table, sidecars and
    /// foreign stores excluded.
    #[test]
    fn a_versioned_product_resolves_to_the_newest_readable_db() {
        let dir = tempfile::tempdir().unwrap();
        let older = seeded(dir.path(), "opencode-powerformer-v1.17.0.db");
        let newer = seeded(dir.path(), "opencode-powerformer-v1.18.1.db");
        set_mtime(&older, 1_700_000_000);
        set_mtime(&newer, 1_800_000_000);
        // Two decoys a real product directory actually contains: WAL sidecars and
        // a database from another component of the app, which has no `message`.
        std::fs::write(dir.path().join("opencode.db-wal"), b"not a database").unwrap();
        let telemetry = dir.path().join("telemetry.db");
        Connection::open(&telemetry)
            .unwrap()
            .execute_batch("CREATE TABLE event (id text PRIMARY KEY);")
            .unwrap();
        set_mtime(&telemetry, 1_900_000_000);

        assert_eq!(db_path_for(dir.path(), &CROW5), newer);
        // A stable-named product never globs: it is its own file, present or not.
        assert_eq!(db_path(dir.path()), dir.path().join("opencode.db"));
        assert_eq!(db_path_for(dir.path(), &MIMOCODE), dir.path().join("mimocode.db"));
    }

    /// With nothing readable to pick, the resolution falls back to the nominal
    /// name, so `probe`/`discover` report "absent" through their existing stat.
    #[test]
    fn a_versioned_product_without_any_readable_db_falls_back_to_its_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("opencode-powerformer-v1.18.1.db"), b"junk").unwrap();
        assert_eq!(db_path_for(dir.path(), &CROW5), dir.path().join("crow5.db"));
        assert!(stat_file(&db_path_for(dir.path(), &CROW5)).is_none());
    }

    fn seeded(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let conn = Connection::open(&path).unwrap();
        seed(&conn);
        drop(conn);
        path
    }

    fn set_mtime(path: &Path, secs: u64) {
        let handle = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        handle.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)).unwrap();
    }

    /// `data_dir`/`db_path` are the frozen OpenCode entry points: they must keep
    /// answering exactly what the product table answers for OpenCode, and OpenCode
    /// must keep joining its own file name instead of globbing.
    #[test]
    fn the_opencode_helpers_and_the_product_table_agree() {
        assert_eq!(data_dir(), data_dir_for(&OPENCODE));
        let dir = data_dir().expect("HOME is set for the test run");
        assert_eq!(db_path(&dir), db_path_for(&dir, &OPENCODE));
        assert_eq!(db_path(&dir), dir.join("opencode.db"));
    }
}

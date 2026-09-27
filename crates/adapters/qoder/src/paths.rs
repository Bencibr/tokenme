//! Where Qoder keeps its logs on this machine.
//!
//! Two stores, two roots:
//!
//! * the **transcript tree** `<root>/projects/**` in the Claude-Code layout, under
//!   `$QODER_CONFIG_DIR` (the CN build spells it `QODERCN_CONFIG_DIR`), both
//!   pointing at `~/.qoder`. Qoder exports its own root to every subprocess, which
//!   is why the env var beats the home directory here.
//! * the **IDE cache db** `<App Support>/Qoder{,CN}/SharedClientCache/cache/db/local.db`,
//!   which is where the IDE's chat/Quest path writes real per-call tokens. The CN
//!   edition's copy sits beside the international one under `QoderCN/`, so both are
//!   candidates on every machine; the two never overlap, they are separate accounts.
//!
//! `<root>/logs/sessions/<mangled-cwd>/<session>/segments/*.jsonl` is *not* a
//! usage root: it is phase telemetry. Across 160 segment files and 176 065 lines
//! on this machine no record carries a `credits` key at all, and every one of the
//! 24 104 `*_tokens` fields in it (including `model.response.completed` and
//! `turn.finished`) is 0 — so watching it would add nothing to meter, only a
//! second file per turn to reason about.
//!
//! And `~/Library/Application Support/com.qoder.app.stable/main.sqlite` (the
//! desktop app, a third store) is deliberately **not** read: measured here, its
//! `chat_session_context_usage` has 6 rows and all 6 snapshots begin
//! `{"totalTokens":0,"maxTokens":0,…,"tokenCountsAvailable":false}` — the vendor
//! says so itself that it has no counts. The only token-named column in that whole
//! database is `byok_model_profiles.max_input_tokens`, which is a BYOK config
//! ceiling and not usage.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

pub(crate) const CONFIG_DIR_ENV: &str = "QODER_CONFIG_DIR";
pub(crate) const CN_CONFIG_DIR_ENV: &str = "QODERCN_CONFIG_DIR";
pub(crate) const PROJECTS_DIR: &str = "projects";
/// The IDE's own home override, honoured by both path ladders we cross-checked
/// (`vibe-cafe/vibe-usage:src/qoder-roots.js:60-62` and
/// `xiufengsun/TokenTracker:src/lib/rollout.js:5763-5767`).
pub(crate) const IDE_HOME_ENV: &str = "QODER_HOME";
pub(crate) const CN_IDE_HOME_ENV: &str = "QODER_CN_HOME";
/// The application-support directory each edition writes into.
pub(crate) const IDE_APP_DIRS: [&str; 2] = ["Qoder", "QoderCN"];
/// …and the db lives this deep below it.
const CACHE_DB_REL: [&str; 4] = ["SharedClientCache", "cache", "db", "local.db"];
/// …and one of the two ladders resolves `QODER_HOME` to the *parent* of
/// `SharedClientCache` instead, so both spellings are candidates.
const CACHE_DB_SHORT_REL: [&str; 3] = ["cache", "db", "local.db"];

/// `$QODER_CONFIG_DIR`, else `$QODERCN_CONFIG_DIR`, else `~/.qoder`. `None` when
/// nothing resolves lets every entry point bail without touching the filesystem.
pub(crate) fn config_root() -> Option<PathBuf> {
    root_from(
        std::env::var_os(CONFIG_DIR_ENV).as_deref(),
        std::env::var_os(CN_CONFIG_DIR_ENV).as_deref(),
        dirs::home_dir().as_deref(),
    )
}

pub(crate) fn root_from(
    qoder: Option<&OsStr>,
    qodercn: Option<&OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = qoder.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = qodercn.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.map(|h| h.join(".qoder"))
}

pub(crate) fn projects_dir(root: &Path) -> PathBuf {
    root.join(PROJECTS_DIR)
}

/// The IDE cache db: `$QODER_HOME`, else `$QODER_CN_HOME`, else
/// `<App Support>/Qoder{,CN}/SharedClientCache/cache/db/local.db`. `None` when the
/// chosen rung holds no database.
pub(crate) fn cache_db_path() -> Option<PathBuf> {
    cache_db_from(
        std::env::var_os(IDE_HOME_ENV).as_deref(),
        std::env::var_os(CN_IDE_HOME_ENV).as_deref(),
        dirs::home_dir().as_deref(),
        dirs::config_dir().as_deref(),
    )
}

/// The pure half of [`cache_db_path`], so the ladder is testable without touching
/// the real application-support directory.
///
/// An env var that is set wins outright and does **not** fall through to the
/// default directory: that is how both ladders we cross-checked behave
/// (`vibe-cafe/vibe-usage:src/qoder-roots.js:60-62` returns from it directly,
/// `xiufengsun/TokenTracker:src/lib/rollout.js:5763-5767` uses it as *the* root),
/// it matches how `config_root` treats `QODER_CONFIG_DIR`, and it is what makes the
/// var usable as a test isolation hook.
///
/// The two ladders disagree on what `QODER_HOME` points *at* — one joins
/// `cache/db/local.db` on it, the other `SharedClientCache/cache/db/local.db` — so
/// both spellings are tried under each env value instead of one of them being
/// guessed.
pub(crate) fn cache_db_from(
    ide_home: Option<&OsStr>,
    cn_ide_home: Option<&OsStr>,
    home: Option<&Path>,
    app_support: Option<&Path>,
) -> Option<PathBuf> {
    let under = |dir: &Path, rel: &[&str]| {
        let candidate = rel.iter().fold(dir.to_path_buf(), |acc, seg| acc.join(seg));
        candidate.is_file().then_some(candidate)
    };
    let in_dir = |dir: &Path| {
        under(dir, CACHE_DB_REL.as_slice()).or_else(|| under(dir, CACHE_DB_SHORT_REL.as_slice()))
    };
    let named = [ide_home, cn_ide_home]
        .into_iter()
        .flatten()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if !named.is_empty() {
        return named.iter().find_map(|dir| in_dir(dir));
    }
    let linux_fallback = home.map(|h| h.join(".config"));
    let support = app_support.or(linux_fallback.as_deref())?;
    IDE_APP_DIRS
        .iter()
        .find_map(|dir| in_dir(&support.join(dir)))
}

/// Size + mtime for `discover`, so an unreadable or vanished file simply is not a
/// source rather than an error halfway through the pass.
pub(crate) fn stat_file(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some((meta.len(), mtime_ms(&meta)?))
}

/// A connection that cannot block the IDE and, as far as the data goes, cannot
/// write. Same ladder as the workbuddy adapter, and for the same reason.
///
/// Measured on this machine's cache db, which is in **WAL**: rung 1
/// (`mode=ro&nolock=1`) answers `unable to open database file (14)` because the
/// wal-index lives in the `-shm` sidecar, and rung 2 (`mode=ro`) is the one that
/// reads it. Rung 3 exists for the case the first two cannot — the IDE has exited
/// and deleted `-shm`/`-wal` behind a database still marked WAL.
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
    let uri = format!("file:{}?{}", encode_uri_path(path), query);
    let conn = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_URI | flags)?;
    if fence {
        conn.execute_batch("PRAGMA query_only = 1;")?;
    }
    // A busy writer surfaces here, not at open time.
    let _ = conn.busy_timeout(std::time::Duration::from_millis(1500));
    conn.query_row(usable_sql, [], |r| r.get::<_, i64>(0)).map(|_| conn)
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

pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let since = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())?;
    Some(since.as_millis() as i64)
}

/// `<root>/projects/<mangled-workspace>/…` carries the workspace path even when
/// a record omits `cwd`.
pub(crate) fn project_segment(path: &Path) -> Option<&OsStr> {
    let mut comps = path.components().peekable();
    while let Some(c) = comps.next() {
        if c.as_os_str() == PROJECTS_DIR {
            // Subagent and transcript dirs still hang off the mangled workspace dir.
            return match comps.next()? {
                Component::Normal(seg) => Some(seg),
                _ => None,
            };
        }
    }
    None
}

/// Install hint for the sources list, read from the log we already opened rather
/// than a second file: Qoder stamps every record with the `version` of the CLI
/// that wrote it. Scans a small prefix because a leading record can be a
/// header line or a torn write.
pub(crate) fn version_hint(path: &Path) -> Option<String> {
    let bytes = read_prefix(path, 64 * 1024)?;
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(version) = record.get("version").and_then(Value::as_str) {
            if !version.is_empty() {
                return Some(version.to_string());
            }
        }
    }
    None
}

fn read_prefix(path: &Path, limit: u64) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut handle = std::fs::File::open(path).ok()?;
    let len = handle.metadata().ok()?.len().min(limit);
    let mut buf = Vec::with_capacity(len as usize);
    handle.by_ref().take(len).read_to_end(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fallback(home: Option<&Path>) -> Option<PathBuf> {
        cache_db_from(None, None, home, None)
    }

    #[test]
    fn the_cache_db_ladder_falls_through_both_editions_and_both_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let support = home.join("Library").join("Application Support");
        let none = || cache_db_from(None, None, Some(&home), Some(&support));

        // Nothing on disk is not an install, for either store.
        assert_eq!(none(), None);
        // A directory that exists but has no database beside it still resolves nothing.
        std::fs::create_dir_all(support.join("Qoder").join("SharedClientCache").join("cache").join("db")).unwrap();
        assert_eq!(none(), None);

        // The international edition's store, at the depth the vendor writes it.
        let intl = support.join("Qoder").join("SharedClientCache").join("cache").join("db").join("local.db");
        std::fs::create_dir_all(intl.parent().unwrap()).unwrap();
        std::fs::write(&intl, b"SQLite format 3\x00").unwrap();
        assert_eq!(none(), Some(intl.clone()));
        // QoderCN is a separate account and never wins over an existing `Qoder`.
        let cn = support.join("QoderCN").join("SharedClientCache").join("cache").join("db").join("local.db");
        std::fs::create_dir_all(cn.parent().unwrap()).unwrap();
        std::fs::write(&cn, b"SQLite format 3\x00").unwrap();
        assert_eq!(none(), Some(intl.clone()));
        std::fs::remove_file(&intl).unwrap();
        assert_eq!(none(), Some(cn.clone()), "the CN edition is a source too");

        // $QODER_HOME beats both, whichever of the two spellings it points at.
        let ide = dir.path().join("idehome");
        let under = ide.join("SharedClientCache").join("cache").join("db").join("local.db");
        std::fs::create_dir_all(under.parent().unwrap()).unwrap();
        std::fs::write(&under, b"SQLite format 3\x00").unwrap();
        assert_eq!(
            cache_db_from(Some(OsStr::new(&ide)), None, Some(&home), Some(&support)),
            Some(under.clone())
        );
        std::fs::remove_file(&under).unwrap();
        let shallow = ide.join("cache").join("db").join("local.db");
        std::fs::create_dir_all(shallow.parent().unwrap()).unwrap();
        std::fs::write(&shallow, b"SQLite format 3\x00").unwrap();
        assert_eq!(
            cache_db_from(Some(OsStr::new(&ide)), None, Some(&home), Some(&support)),
            Some(shallow),
            "the other reference resolves QODER_HOME one level shorter"
        );
        // An env var that is set is the whole answer: it pins the search, which is
        // what makes it usable as an isolation hook and what both ladders do.
        let nope = dir.path().join("nope");
        assert_eq!(
            cache_db_from(Some(nope.as_os_str()), None, Some(&home), Some(&support)),
            None,
            "a QODER_HOME with no database under it is not a source"
        );
        assert_eq!(
            cache_db_from(Some(OsStr::new("")), None, Some(&home), Some(&support)),
            Some(cn.clone()),
            "an empty override is unset, not a redirect"
        );
        // No home at all means no store.
        assert_eq!(fallback(None), None);
    }

    #[test]
    fn a_read_only_handle_survives_both_journal_modes() {
        let dir = tempfile::tempdir().unwrap();
        for mode in ["delete", "wal"] {
            let path = dir.path().join(format!("{mode}.db"));
            let writer = rusqlite::Connection::open(&path).unwrap();
            writer.execute_batch(&format!("PRAGMA journal_mode={mode};")).unwrap();
            writer
                .execute_batch("CREATE TABLE chat_message (id TEXT PRIMARY KEY, role TEXT);")
                .unwrap();
            writer.execute("INSERT INTO chat_message VALUES ('a','assistant')", []).unwrap();
            let conn = open_readonly(&path, "SELECT EXISTS(SELECT 1 FROM chat_message LIMIT 1)")
                .unwrap_or_else(|e| panic!("{mode}: {e}"));
            assert_eq!(
                conn.query_row("SELECT count(*) FROM chat_message", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert!(
                conn.execute("DELETE FROM chat_message", []).is_err(),
                "a read-only handle can never write"
            );
            drop(conn);
            // A foreign table is not ours, in either mode.
            assert!(open_readonly(&path, "SELECT EXISTS(SELECT 1 FROM nope LIMIT 1)").is_err());
        }
    }

    #[test]
    fn stat_file_never_invents_a_source() {
        let dir = tempfile::tempdir().unwrap();
        assert!(stat_file(dir.path()).is_none(), "a directory is not a source");
        assert_eq!(stat_file(&dir.path().join("gone.db")), None);
        std::fs::write(dir.path().join("x.db"), "hi").unwrap();
        let (size, mtime) = stat_file(&dir.path().join("x.db")).unwrap();
        assert_eq!(size, 2);
        assert!(mtime > 0);
    }

    #[test]
    fn percent_encodes_the_characters_that_would_break_a_uri() {
        let enc = encode_uri_path(Path::new("/tmp/a b?c#d/local.db"));
        assert!(!enc.contains(' ') && !enc.contains('?') && !enc.contains('#'));
        assert!(enc.ends_with("/local.db"));
        assert!(enc.contains("%20") && enc.contains("%3F") && enc.contains("%23"));
    }

    #[test]
    fn qoder_env_beats_home_and_cn_is_the_fallback() {
        let home = Some(Path::new("/home/x"));
        assert_eq!(
            root_from(Some(OsStr::new("/tmp/a")), Some(OsStr::new("/tmp/b")), home),
            Some(PathBuf::from("/tmp/a"))
        );
        assert_eq!(
            root_from(None, Some(OsStr::new("/tmp/b")), home),
            Some(PathBuf::from("/tmp/b"))
        );
        // An empty override is not a redirect: treat it as unset.
        assert_eq!(
            root_from(Some(OsStr::new("")), Some(OsStr::new("")), home),
            Some(PathBuf::from("/home/x/.qoder"))
        );
        assert_eq!(
            root_from(None, None, home),
            Some(PathBuf::from("/home/x/.qoder"))
        );
        assert_eq!(root_from(None, None, None), None);
    }

    #[test]
    fn project_segment_is_the_mangled_workspace() {
        let p = Path::new("/h/.qoder/projects/-Users-sp-proj/sess/subagents/agent-a.jsonl");
        assert_eq!(project_segment(p), Some(OsStr::new("-Users-sp-proj")));
        assert_eq!(project_segment(Path::new("/elsewhere/a.jsonl")), None);
    }

    #[test]
    fn version_hint_comes_from_the_first_stamped_record() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("a.jsonl");
        // A torn first line must not hide the version on line two.
        std::fs::write(
            &log,
            b"{ broken\n{\"type\":\"runtime-config\",\"version\":\"1.1.57\"}\n",
        )
        .unwrap();
        assert_eq!(version_hint(&log).as_deref(), Some("1.1.57"));
        std::fs::write(&log, b"{ broken\n").unwrap();
        assert_eq!(version_hint(&log), None);
        assert_eq!(version_hint(&dir.path().join("missing.jsonl")), None);
    }
}

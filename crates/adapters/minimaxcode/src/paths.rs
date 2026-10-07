//! Locating a MiniMax Code install and the session transcripts inside it.
//!
//! The vendor resolves its own data directory in
//! `packages/tui/src/runtime/data-dir.ts`: `MINIMAX_DATA_DIR`, else the
//! pre-rename `MAVIS_DATA_DIR`, else `~/.minimax` — and `~/.minimax-<profile>`
//! when `MAVIS_PROFILE` names a profile (`packages/config/src/data-dir.ts`
//! `basenameForProfile`). The legacy basename `~/.mavis` is not deleted by that
//! migration: it is replaced by a **symlink** to the new directory, so both
//! basenames are read and every root is canonicalised — otherwise one session
//! file enters the manifest under two keys and is billed twice.
//!
//! Inside a home, one transcript per session:
//!
//! ```text
//! <home>/v2/sessions/<YYYY>/<MM>/<DD>/<HH-mm-ss-mmm>-session_<encoded id>/messages.jsonl
//! ```
//!
//! which is `packages/local-runtime-v2/…/history/session-history-paths.ts`
//! (`resolveSessionHistoryPathsFromRelativeDir` plus `buildSessionRelativeDir`,
//! UTC-bucketed by default). `manifest.json` beside it carries the plain session
//! id, and the sqlite store beside that carries the workspace each session ran in.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// The vendor's data-dir override, naming the directory exactly.
pub(crate) const ENV_DATA_DIR: &str = "MINIMAX_DATA_DIR";
/// The same knob under the product's pre-rename name, consulted second.
pub(crate) const ENV_LEGACY_DATA_DIR: &str = "MAVIS_DATA_DIR";
/// The vendor's profile selector, which suffixes the default directory name.
pub(crate) const ENV_PROFILE: &str = "MAVIS_PROFILE";
/// Keeps the environment-touching tests off each other's variables. Nothing in
/// the runtime path reads an environment variable under a lock.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const SESSIONS_REL: &str = "v2/sessions";
const MESSAGES_FILE: &str = "messages.jsonl";
/// The vendor's own projection of every billed call, used for the project label.
const RUNTIME_DB_REL: &str = "v2/sqlite/runtime-state.sqlite";
/// Below this many path segments a transcript cannot live, and a deeper tree is
/// somebody else's data (a `node_modules` inside a workspace, say).
const MAX_DEPTH: usize = 8;

/// Every existing home, in the vendor's priority order: an env override pins
/// exactly one, and otherwise the new and legacy defaults both count.
pub(crate) fn homes() -> Vec<PathBuf> {
    for name in [ENV_DATA_DIR, ENV_LEGACY_DATA_DIR] {
        let Ok(value) = std::env::var(name) else { continue };
        let single = value.trim();
        if !single.is_empty() {
            return existing(vec![PathBuf::from(single)]);
        }
    }
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let profile = std::env::var(ENV_PROFILE).ok().map(|p| p.trim().to_string()).unwrap_or_default();
    let suffixed = |base: &str| -> PathBuf {
        if profile.is_empty() {
            home.join(base)
        } else {
            home.join(format!("{base}-{profile}"))
        }
    };
    existing(vec![suffixed(".minimax"), suffixed(".mavis")])
}

/// Existing directories only, in order, with one canonical path counted once.
fn existing(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = Vec::new();
    paths
        .into_iter()
        .filter(|p| p.is_dir())
        .filter(|p| {
            let key = p.canonicalize().unwrap_or_else(|_| p.clone());
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        })
        .collect()
}

/// Every `messages.jsonl` under every home, sorted so a pass is reproducible.
pub(crate) fn history_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for home in homes() {
        collect(&home.join(SESSIONS_REL), 0, &mut files);
    }
    files.sort();
    files.dedup();
    files
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, depth + 1, out);
        } else if path.file_name().is_some_and(|n| n == MESSAGES_FILE) {
            out.push(path);
        }
    }
}

/// The plain session id a transcript belongs to.
///
/// The directory encodes it (`session_<base64 of id>`), but `manifest.json` is
/// what the vendor writes it into un-encoded, and the file is 1 KB, so decoding
/// the directory name is never needed. `history-catalog.json` carries the same
/// field for a session whose manifest has not been written yet; a transcript
/// with neither is labelled by its directory, which is stable for its lifetime.
pub(crate) fn session_id(path: &Path) -> String {
    let dir = match path.parent() {
        Some(d) => d,
        None => return String::new(),
    };
    for name in ["manifest.json", "history-catalog.json"] {
        if let Some(id) = field_at(&dir.join(name), "sessionId") {
            return id;
        }
    }
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn field_at(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The first instant of the UTC day the session was created on, from its path.
///
/// The dated directory is derived from `createdAtMs`, so it is a *lower bound* on
/// every call in the file — which is what makes pruning a future window sound and
/// pruning a past one not (a session opened last month still bills today).
pub(crate) fn created_day_ms(path: &Path) -> Option<i64> {
    let rel = path.components().collect::<Vec<_>>();
    let at = rel.iter().rposition(|c| *c == Component::Normal("sessions".as_ref()))?;
    // …/v2/sessions/YYYY/MM/DD/<leaf>/messages.jsonl
    let segment = |offset: usize| -> Option<i64> {
        rel.get(at + offset)?
            .as_os_str()
            .to_str()?
            .parse::<i64>()
            .ok()
    };
    let (year, month, day) = (segment(1)?, segment(2)?, segment(3)?);
    let date = chrono::NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

/// `session id → workspace dir`, the label the app itself shows for a session.
///
/// Read once per transcript the pass re-reads, so its cost rides along with the
/// whole-file read it labels rather than adding a pass step; a database that cannot
/// be opened costs the label, never an event.
pub(crate) fn workspaces() -> HashMap<String, String> {
    let mut out = HashMap::new();
    for home in homes() {
        let Some(conn) = open_readonly(&home.join(RUNTIME_DB_REL)) else { continue };
        let Ok(mut stmt) = conn.prepare(
            "SELECT session_id, workspace_dir FROM local_runtime_sessions WHERE workspace_dir IS NOT NULL AND workspace_dir <> ''",
        ) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
            ))
        }) else {
            continue;
        };
        for row in rows.flatten() {
            out.insert(row.0, row.1);
        }
    }
    out
}

/// The first of the store's databases that exists, for `probe`'s hint.
pub(crate) fn runtime_db() -> Option<PathBuf> {
    homes()
        .into_iter()
        .map(|h| h.join(RUNTIME_DB_REL))
        .find(|p| p.is_file())
}

/// A connection that cannot block the app and, as far as the data goes, cannot
/// write. The MiniMax Code runtime owns this database and keeps it in WAL mode,
/// so a read-only handle has three possible failures to step over — the same
/// ladder `usage-adapter-opencode` documents:
///
/// 1. `mode=ro&nolock=1` takes no lock but cannot build the wal-index in `-shm`.
/// 2. `mode=ro` reads a live store, but not once the app has exited and deleted
///    `-shm`/`-wal` while the header still says WAL.
/// 3. `mode=rw` exists only so SQLite may rebuild `-shm`; it is fenced with
///    `PRAGMA query_only` at once.
///
/// The usability proof is a read of the table the caller is about to query, so a
/// half-broken open falls through to the next rung instead of surfacing as an error.
fn open_readonly(path: &Path) -> Option<Connection> {
    if !path.is_file() {
        return None;
    }
    const PROOF: &str = "SELECT 1 FROM local_runtime_sessions LIMIT 1";
    let rungs = [
        ("mode=ro&nolock=1", OpenFlags::SQLITE_OPEN_READ_ONLY, false),
        ("mode=ro", OpenFlags::SQLITE_OPEN_READ_ONLY, false),
        ("mode=rw", OpenFlags::SQLITE_OPEN_READ_WRITE, true),
    ];
    let uri_base = format!("file:{}", encode_uri_path(path));
    for (query, flags, fence) in rungs {
        let opened = Connection::open_with_flags(
            format!("{uri_base}?{query}"),
            OpenFlags::SQLITE_OPEN_URI | flags,
        );
        let Ok(conn) = opened else { continue };
        if fence && conn.execute_batch("PRAGMA query_only = 1;").is_err() {
            continue;
        }
        if conn.execute_batch(PROOF).is_err() {
            continue;
        }
        return Some(conn);
    }
    None
}

fn encode_uri_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            // A Windows path reaches the URI with backslashes and a drive letter;
            // SQLite reads both as escapes unless they are percent-encoded.
            '\\' => out.push_str("%5C"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_data_dir_variable_pins_exactly_one_root() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_DATA_DIR, dir.path());
        assert_eq!(homes(), vec![dir.path().to_path_buf()]);
        // Blank is not a root: the ladder falls through to the defaults.
        std::env::set_var(ENV_DATA_DIR, "  ");
        std::env::remove_var(ENV_DATA_DIR);
        assert!(!homes().contains(&PathBuf::from("  ")));
    }

    #[test]
    fn the_legacy_variable_is_consulted_only_when_the_new_one_is_absent() {
        let _g = ENV_LOCK.lock().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_DATA_DIR, a.path());
        std::env::set_var(ENV_LEGACY_DATA_DIR, b.path());
        assert_eq!(homes(), vec![a.path().to_path_buf()], "MINIMAX_DATA_DIR wins");
        std::env::remove_var(ENV_DATA_DIR);
        assert_eq!(homes(), vec![b.path().to_path_buf()]);
        std::env::remove_var(ENV_LEGACY_DATA_DIR);
    }

    #[test]
    fn a_missing_root_yields_no_history_and_no_database() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_DATA_DIR, dir.path().join("absent"));
        assert!(homes().is_empty());
        assert!(history_files().is_empty());
        assert!(runtime_db().is_none());
        std::env::remove_var(ENV_DATA_DIR);
    }

    #[test]
    fn the_dated_layout_is_found_and_named_by_session_and_day() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_DATA_DIR, dir.path());
        let leaf = dir.path().join("v2").join("sessions").join("2026").join("10").join("05");
        let session = leaf.join("00-07-16-076-session_bXZz");
        std::fs::create_dir_all(&session).unwrap();
        let transcript = session.join("messages.jsonl");
        std::fs::write(&transcript, "").unwrap();
        // A manifest the vendor wrote: the id is read from it, not decoded.
        std::fs::write(
            session.join("manifest.json"),
            r#"{"schemaVersion":1,"sessionId":"mvs_abc"}"#,
        )
        .unwrap();
        // Anything that is not the transcript is not a source.
        std::fs::write(session.join("history-catalog.json"), r#"{"sessionId":"ignored"}"#)
            .unwrap();

        assert_eq!(history_files(), vec![transcript.clone()]);
        assert_eq!(session_id(&transcript), "mvs_abc");
        // 2026-10-05T00:00:00Z, the lower bound every call in the file obeys.
        assert_eq!(created_day_ms(&transcript), Some(1_791_158_400_000));
        std::env::remove_var(ENV_DATA_DIR);
    }

    #[test]
    fn a_session_without_a_manifest_is_still_labelled_stably() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_DATA_DIR, dir.path());
        let session = dir
            .path()
            .join("v2/sessions/2026/10/05/01-02-03-004-session_ZZZ");
        std::fs::create_dir_all(&session).unwrap();
        let transcript = session.join("messages.jsonl");
        std::fs::write(&transcript, "").unwrap();
        assert_eq!(session_id(&transcript), "01-02-03-004-session_ZZZ");
        std::env::remove_var(ENV_DATA_DIR);
    }

    #[test]
    fn a_directory_that_is_not_dated_has_no_lower_bound() {
        // Never prune on a path this adapter cannot read a date out of.
        assert_eq!(
            created_day_ms(Path::new("/h/v2/sessions/scratch/messages.jsonl")),
            None
        );
    }
}

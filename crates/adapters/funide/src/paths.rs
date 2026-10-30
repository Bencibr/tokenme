//! Where FunIDE keeps its agent sessions.
//!
//! The data dir is Electron's: `%APPDATA%\FunIDE` on Windows,
//! `~/Library/Application Support/FunIDE` on macOS, and everything below lives
//! under `<data_dir>/FunIDE/User/funide/sessions/<workspace-hash>/`. No env
//! override is documented; `FUNIDE_DATA_DIR` is tokenme's, following the
//! adapters that honour their vendors' overrides.
//!
//! ## The layout changed under us in 0.0.4.4
//!
//! Until 0.0.4.3 one conversation was one file, `<sid>.json`, whose header
//! (`id`, `activeModelId`, `workspaceKey`, `updatedAt`) and per-turn `items`
//! sat together. Measured on a live 0.0.4.5 install, that file is gone and the
//! same conversation is now four siblings:
//!
//! | file                | holds                                                |
//! |---------------------|------------------------------------------------------|
//! | `<sid>.meta.json`   | the session header + the UI's lifetime `cacheUsage`  |
//! | `<sid>.main.json`   | `items` — the per-turn ledger, and nothing else      |
//! | `<sid>.ckpt-N.json` | a rollback snapshot: turns `main` already carries    |
//! | `<sid>.draft.txt`   | the composer's unsent text                           |
//!
//! Only `.main.json` is a session body, and the old `<sid>.json` is deleted as
//! the split lands (no workspace here has both). Reading `.meta.json` alone
//! bills nothing — it has no `items`; reading a `.ckpt-N.json` would bill turns
//! twice or land on the wrong day. So discovery returns exactly one path per
//! session, and the body's header is picked up from its sibling in [`header_for`].
//!
//! `index.json` per workspace is UI bookkeeping (titles, counts) and is not a
//! session either.

use std::path::PathBuf;

pub(crate) const HOME_ENV: &str = "FUNIDE_DATA_DIR";

/// `<data>/FunIDE/User/funide/sessions`, when it exists.
pub fn sessions_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(HOME_ENV)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        let p = dir.join("sessions");
        return p.is_dir().then_some(p);
    }
    let base = dirs::data_dir()?;
    let p = base.join("FunIDE").join("User").join("funide").join("sessions");
    p.is_dir().then_some(p)
}

/// One path per session under the root, `index.json` excluded — it is the
/// workspace list UI state and carries no usage.
///
/// A session is `<sid>.main.json` in the 0.0.4.4+ layout and `<sid>.json` before
/// it. The header (`.meta.json`) and the rollback snapshots (`.ckpt-N.json`) are
/// deliberately not returned: the header has no turn to bill, and a checkpoint
/// replays turns the body already holds. When a split is half-written and both
/// bodies exist, `main` wins as the newer, authoritative one — the turn ids are
/// shared, so the indexer's dedupe absorbs the overlap either way.
pub fn session_files() -> Vec<PathBuf> {
    let Some(root) = sessions_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&root) else { return Vec::new() };
    let mut by_session: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for ws in entries.flatten() {
        let ws_dir = ws.path();
        if !ws_dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&ws_dir) else { continue };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if name == "index.json" {
                continue;
            }
            if let Some(sid) = name.strip_suffix(".main.json") {
                by_session.insert(sid.to_string(), path);
                continue;
            }
            // `.meta.json` is the header, `.ckpt-N.json` a snapshot: neither is a body.
            if name.ends_with(".meta.json") || name.contains(".ckpt-") {
                continue;
            }
            let Some(sid) = name.strip_suffix(".json") else { continue };
            by_session.entry(sid.to_string()).or_insert(path);
        }
    }
    let mut out: Vec<PathBuf> = by_session.into_values().collect();
    out.sort();
    out
}

/// The session header that belongs to a body, in the layout 0.0.4.4 introduced:
/// `<sid>.main.json` → `<sid>.meta.json`. `None` for the older single-file
/// layout, whose header fields sit in the body itself.
pub fn header_for(body: &std::path::Path) -> Option<PathBuf> {
    let name = body.file_name()?.to_str()?;
    let sid = name.strip_suffix(".main.json")?;
    let header = body.with_file_name(format!("{sid}.meta.json"));
    header.is_file().then_some(header)
}

pub fn stat_file(path: &std::path::Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let mtime_ms = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((meta.len(), mtime_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_files_skip_the_index_and_walk_workspaces() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(HOME_ENV, dir.path());
        let root = dir.path().join("sessions").join("ws1");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.json"), b"{}").unwrap();
        std::fs::write(root.join("abc.json"), b"{}").unwrap();
        let files = session_files();
        assert_eq!(files.len(), 1, "index.json is not a session");
        assert!(files[0].ends_with("abc.json"));
        std::env::remove_var(HOME_ENV);
    }

    #[test]
    fn the_split_layout_yields_one_body_per_session_and_nothing_else() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(HOME_ENV, dir.path());
        let root = dir.path().join("sessions").join("ws1");
        std::fs::create_dir_all(&root).unwrap();
        for name in [
            "index.json",
            "s9.main.json",
            "s9.meta.json",
            "s9.ckpt-0.json",
            "s9.ckpt-1.json",
            "s9.draft.txt",
        ] {
            std::fs::write(root.join(name), b"{}").unwrap();
        }
        let files = session_files();
        assert_eq!(files.len(), 1, "meta, checkpoints, draft and index are not session bodies");
        assert!(files[0].ends_with("s9.main.json"));
        std::env::remove_var(HOME_ENV);
    }

    #[test]
    fn both_layouts_side_by_side_stay_two_sessions_and_a_half_written_split_stays_one() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(HOME_ENV, dir.path());
        let root = dir.path().join("sessions").join("ws1");
        std::fs::create_dir_all(&root).unwrap();
        // A pre-split session, a post-split one, and a split caught mid-write.
        std::fs::write(root.join("old.json"), b"{}").unwrap();
        std::fs::write(root.join("new.main.json"), b"{}").unwrap();
        std::fs::write(root.join("new.meta.json"), b"{}").unwrap();
        std::fs::write(root.join("mid.json"), b"{}").unwrap();
        std::fs::write(root.join("mid.main.json"), b"{}").unwrap();
        let files = session_files();
        assert_eq!(files.len(), 3, "old, new and mid — the half-written one is listed once");
        assert!(files.iter().any(|f| f.ends_with("old.json")));
        assert!(files.iter().any(|f| f.ends_with("new.main.json")));
        assert!(
            files.iter().any(|f| f.ends_with("mid.main.json")),
            "the split body is the newer, authoritative one"
        );
        std::env::remove_var(HOME_ENV);
    }

    #[test]
    fn a_body_finds_its_header_only_in_the_split_layout() {
        let dir = tempfile::tempdir().unwrap();
        let body = dir.path().join("s9.main.json");
        std::fs::write(&body, b"{}").unwrap();
        assert!(header_for(&body).is_none(), "no sibling yet");
        let header = dir.path().join("s9.meta.json");
        std::fs::write(&header, b"{}").unwrap();
        assert_eq!(header_for(&body), Some(header));
        // A legacy body is its own header: there is nothing to look up.
        let legacy = dir.path().join("s1.json");
        std::fs::write(&legacy, b"{}").unwrap();
        assert!(header_for(&legacy).is_none());
    }
}

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

//! Where FunIDE keeps its agent sessions.
//!
//! The writer's layout, straight from a live install (2026-09-29):
//! `<data_dir>/FunIDE/User/funide/sessions/<workspace-hash>/<session>.json`
//! plus an `index.json` per workspace that is UI bookkeeping, not usage. The
//! data dir is Electron's: `%APPDATA%\FunIDE` on Windows, 
//! `~/Library/Application Support/FunIDE` on macOS. No env override is
//! documented; `FUNIDE_DATA_DIR` is tokenme's, following the adapters that
//! honour their vendors' overrides.

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

/// Every session JSON under the root (any depth), `index.json` excluded —
/// it is the workspace list UI state and carries no usage.
pub fn session_files() -> Vec<PathBuf> {
    let Some(root) = sessions_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&root) else { return Vec::new() };
    let mut out = Vec::new();
    for ws in entries.flatten() {
        let ws_dir = ws.path();
        if !ws_dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&ws_dir) else { continue };
        for f in files.flatten() {
            let path = f.path();
            let is_json = path.extension().and_then(|e| e.to_str()) == Some("json");
            let is_index = path.file_name().and_then(|n| n.to_str()) == Some("index.json");
            if is_json && !is_index {
                out.push(path);
            }
        }
    }
    out.sort();
    out
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
}

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

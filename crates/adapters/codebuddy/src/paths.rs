//! Where CodeBuddy CN keeps its local store.
//!
//! The desktop app (a VS Code fork, `CodeBuddy CN.exe`) lands its agent
//! transcripts outside the app-data dir, in a shared runtime dir named after
//! the product family: `%LOCALAPPDATA%\CodeBuddyExtension\Data\<uid>\CodeBuddyIDE\<uid>\history\`.
//! Under that, one directory per workspace hash, one per conversation, and a
//! `messages/` directory holding one JSON file per message. `CODEBUDDY_DATA_DIR`
//! overrides the root, mirroring the other adapters' env escapes and giving the
//! tests a fixture handle.

use std::path::PathBuf;

/// The vendor-shared runtime root; an empty value means unset.
pub const ENV_DATA_DIR: &str = "CODEBUDDY_DATA_DIR";

/// Every history root that exists here, env override first.
pub fn config_roots() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_DATA_DIR).filter(|dir| !dir.is_empty()) {
        let dir = PathBuf::from(dir);
        return if dir.is_dir() { vec![dir] } else { Vec::new() };
    }
    let Some(local) = dirs::data_local_dir() else { return Vec::new() };
    let root = local.join("CodeBuddyExtension").join("Data");
    if root.is_dir() { vec![root] } else { Vec::new() }
}

/// Env vars are process-global, so every test that points the adapter at a
/// temp dir holds this lock (shared across the crate's test modules).
#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

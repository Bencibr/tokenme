//! Where WorkBuddy AI keeps its local store.
//!
//! The desktop app (com.workbuddy.workbuddy-ai) uses `~/.workbuddy-ai` as its
//! user-data dir (`customUserDataDir` in the shipped product config); the
//! `WORKBUDDY_CONFIG_DIR` env var overrides it, mirroring the vendor's own
//! convention. `~/.workbuddy` is the sibling the CLI-shaped install uses, and
//! third-party WorkBuddy readers default to it, so both roots get walked — one
//! machine can hold either, both, or neither. CN/CodeBuddy editions are separate
//! products with their own stores — not this adapter's business.

use std::path::PathBuf;
#[cfg(test)]
use std::path::Path;

pub const DB_NAME: &str = "workbuddy.db";

/// The vendor's own override for the data dir; an empty value means unset.
pub const ENV_CONFIG_DIR: &str = "WORKBUDDY_CONFIG_DIR";

/// Every data root that exists here, env override first, in walk order.
pub fn config_roots() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_CONFIG_DIR).filter(|dir| !dir.is_empty()) {
        return vec![PathBuf::from(dir)];
    }
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    [".workbuddy-ai", ".workbuddy"]
        .iter()
        .map(|name| home.join(name))
        .filter(|root| root.is_dir())
        .collect()
}

/// The database path, preferring the first root that has one.
pub fn db_path() -> Option<PathBuf> {
    config_roots()
        .into_iter()
        .map(|root| root.join(DB_NAME))
        .find(|path| path.is_file())
}

/// `(size, mtime_ms)` — production stats its files inline; this survives for
/// the transcript tests, which build `SourceFile`s from fixture paths.
#[cfg(test)]
pub fn stat_file(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((meta.len(), mtime_ms))
}

/// Env vars are process-global, so every test that points the adapter at a temp
/// home holds this lock (shared across the crate's test modules).
#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

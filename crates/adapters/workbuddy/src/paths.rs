//! Where WorkBuddy AI keeps its local database.
//!
//! The desktop app (com.workbuddy.workbuddy-ai) uses `~/.workbuddy-ai` as its
//! user-data dir (`customUserDataDir` in the shipped product config); the
//! `WORKBUDDY_CONFIG_DIR` env var overrides it, mirroring the vendor's own
//! convention. CN/CodeBuddy editions are separate products with their own
//! stores — not this adapter's business.

use std::path::{Path, PathBuf};

pub const DB_NAME: &str = "workbuddy.db";

/// The directory the database lives in, env override first.
pub fn config_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("WORKBUDDY_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|home| home.join(".workbuddy-ai"))
}

pub fn db_path() -> Option<PathBuf> {
    let path = config_root()?.join(DB_NAME);
    path.is_file().then_some(path)
}

/// `(size, mtime_ms)` for the discover/manifest bookkeeping.
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

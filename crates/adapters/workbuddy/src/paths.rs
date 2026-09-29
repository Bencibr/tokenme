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

/// The stat the ingest change-detection keys on. The app runs the database in
/// WAL mode — session writes land in `workbuddy.db-wal` and the main file's
/// own stat stays frozen between checkpoints — so the WAL's mtime folds in as
/// the activity signal. The size stays the main file's: the WAL shrinks on
/// every checkpoint, and a shrinking stat is what the ingest reads as "this
/// log was rewritten, purge and start over".
pub fn source_stat(path: &Path) -> Option<(u64, i64)> {
    let (size, mtime_ms) = stat_file(path)?;
    let mut wal_name = path.file_name()?.to_os_string();
    wal_name.push("-wal");
    let wal = path.with_file_name(wal_name);
    match stat_file(&wal) {
        Some((_, wal_mtime)) => Some((size, mtime_ms.max(wal_mtime))),
        None => Some((size, mtime_ms)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wal_write_is_activity_even_while_the_main_file_sleeps() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(DB_NAME);
        std::fs::write(&db, b"main").unwrap();
        assert_eq!(source_stat(&db), stat_file(&db), "no wal: the plain stat stands");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join(format!("{DB_NAME}-wal")), b"wal").unwrap();
        let (size, mtime) = source_stat(&db).unwrap();
        let (_, db_mtime) = stat_file(&db).unwrap();
        assert_eq!(size, stat_file(&db).unwrap().0, "the size stays the main file's");
        assert!(mtime > db_mtime, "the newer wal mtime is the change signal");
    }
}

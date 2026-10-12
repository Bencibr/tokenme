//! Where 妙手 (the IDE formerly branded CatPaw) keeps its usage, and what this
//! machine's copy taught us about it.
//!
//! The 2026-09 builds renamed the product to 妙手 (`product.json`:
//! `applicationName: "catpaw-moon"`, `dataFolderName: ".meituan-catpaw"`) and
//! moved the chat store with it. Both generations are read:
//!
//! | store | holds |
//! |---|---|
//! | `~/Library/Application Support/catpaw-moon` (Windows `%APPDATA%`) → `catpaw-memory-<accountId>.db`, table `ui_sdk_messages` | the renamed IDE's chat rows; every assistant row whose payload carries `extra.contextInfo.usage` is one billed model call. `catpaw-memory-anon.db` is the logged-out scope of the same schema |
//! | `~/.sankuai/CatPawAI/sqliteDB/globalCache.sqlite` → table `t_ui_messages` | the pre-rename IDE; one row per chat block, `tokenUsage` objects mark billed calls |
//! | `~/.catpaw/projects/…` | human-readable transcripts (jsonl since the rename) — no usage numbers at all; kept for the hint text |
//!
//! ## The two token conventions
//! The old store used snake_case `tokenUsage`, the new one camelCase
//! `usage` — and both are **exclusive**: `totalTokens` = prompt + completion +
//! cacheRead, with the cached prefix *not* folded into the prompt (measured on
//! the old store: 366 + 156_416 + 163 with total 529 = 366 + 163; on the new
//! one: 9135 + 16896 + 73 = 26104). Stages map 1:1 onto
//! [`usage_core::TokenCounts`] with no netting, either generation.
//!
//! ## Corruption is a supported state
//! Reads are best-effort exactly like the qoder cache db: an unopenable file,
//! a missing table or a corrupt read yields an empty outcome with the
//! **unchanged** cursor — never a fabricated number, and never a cursor
//! advance that would hide rows once the store heals.

use std::path::{Path, PathBuf};

/// Read by the pre-rename IDE; overridable so a test can point the adapter at
/// a fixture tree instead of a user's home.
pub const ENV_CATPAW_DATA: &str = "CATPAW_DATA_DIR";

/// Read by the renamed IDE (妙手); same contract as [`ENV_CATPAW_DATA`].
pub const ENV_CATPAW_MOON_DATA: &str = "CATPAW_MOON_DATA_DIR";

/// `~/.sankuai/CatPawAI` — the pre-rename IDE's cross-workspace data root.
pub fn data_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_CATPAW_DATA) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|h| h.join(".sankuai").join("CatPawAI"))
}

/// The renamed IDE's Electron userData dir — `product.json`'s
/// `applicationName`, so it follows the app name, not the product's mood.
pub fn moon_data_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_CATPAW_MOON_DATA) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::data_dir().map(|d| d.join("catpaw-moon"))
}

/// The one sqlite file of the pre-rename IDE. Cursor = max `rowid` of
/// `t_ui_messages`.
pub fn db_path() -> Option<PathBuf> {
    let p = data_root()?.join("sqliteDB").join("globalCache.sqlite");
    p.is_file().then_some(p)
}

/// Every account-scoped memory db of the renamed IDE, sorted for stable
/// ordering. One db per logged-in account (`catpaw-memory-<accountId>.db`)
/// plus the logged-out `catpaw-memory-anon.db`; all of them count.
pub fn memory_dbs() -> Vec<PathBuf> {
    let Some(root) = moon_data_root() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&root) else { return Vec::new() };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            name.starts_with("catpaw-memory-") && name.ends_with(".db")
        })
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

/// Agent transcripts hold no usage numbers (txt before the rename, jsonl
/// after); they exist so the panel's "sources" row can point a user somewhere
/// human-readable. Kept for the hint text.
pub fn transcripts_dir() -> Option<PathBuf> {
    let p = dirs::home_dir()?.join(".catpaw").join("projects");
    p.is_dir().then_some(p)
}

#[allow(dead_code)]
pub fn project_dir_exists(path: &Path) -> bool {
    path.is_dir()
}

#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_root_honours_the_env_override() {
        let _env = lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_CATPAW_DATA, dir.path());
        assert_eq!(data_root().as_deref(), Some(dir.path()));
        assert!(db_path().is_none(), "no db under the temp root");
        std::fs::create_dir_all(dir.path().join("sqliteDB")).unwrap();
        std::fs::write(dir.path().join("sqliteDB/globalCache.sqlite"), b"SQLite format 3").unwrap();
        assert_eq!(
            db_path().as_deref(),
            Some(dir.path().join("sqliteDB/globalCache.sqlite").as_path())
        );
        std::env::remove_var(ENV_CATPAW_DATA);
        assert_eq!(
            data_root().as_deref(),
            dirs::home_dir().map(|h| h.join(".sankuai").join("CatPawAI")).as_deref()
        );
    }

    #[test]
    fn memory_dbs_glob_the_moon_root() {
        let _env = lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_CATPAW_MOON_DATA, dir.path());
        assert!(memory_dbs().is_empty(), "no dbs under the temp root");
        for name in ["catpaw-memory-61041752.db", "catpaw-memory-anon.db"] {
            std::fs::write(dir.path().join(name), b"SQLite format 3").unwrap();
        }
        // Sidecar and foreign files never match.
        std::fs::write(dir.path().join("catpaw-memory-61041752.db-wal"), b"x").unwrap();
        std::fs::write(dir.path().join("unrelated.db"), b"x").unwrap();
        let dbs = memory_dbs();
        assert_eq!(dbs.len(), 2, "the two memory dbs, not their sidecars");
        assert!(dbs[0].ends_with("catpaw-memory-61041752.db"), "sorted, anon last");
        assert!(dbs[1].ends_with("catpaw-memory-anon.db"));
        std::env::remove_var(ENV_CATPAW_MOON_DATA);
    }
}

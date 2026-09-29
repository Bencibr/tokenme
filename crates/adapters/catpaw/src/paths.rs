//! Where CatPaw (美团 CatPawAI, a VSCode-family agent IDE) keeps its usage, and
//! what this machine's copy taught us about it.
//!
//! | store | holds |
//! |---|---|
//! | `~/.sankuai/CatPawAI/sqliteDB/globalCache.sqlite` → table `t_ui_messages` | one row per chat block; every row that carries a `tokenUsage` object is one billed model call |
//! | `~/.catpaw/projects/ide-<workspace>/<session>/agent-transcripts/transcript.txt` | human-readable transcript — no usage numbers at all |
//!
//! ## The two token conventions on one row
//! The content JSON carries two spellings of the same call, and they disagree
//! with each other by design: `usage` and `tokenUsage` are equal, and inside
//! them `total_tokens = prompt_tokens + completion_tokens` — `cacheReadTokens`
//! is *not* part of `prompt_tokens` (measured: prompt 366 + cacheRead 156_416 +
//! completion 163, total 529 = 366 + 163). That is the **exclusive** convention,
//! the opposite of ZCode's folded one: stages map 1:1 onto
//! [`usage_core::TokenCounts`] with no netting.
//!
//! ## Which rows bill
//! A model call's usage lands on the row that closes it — `message_type` `tool`
//! for tool-call turns, `text` for plain replies — and a 6.3k-row survey of the
//! recovered store found every `(conversation, prompt, completion, cacheRead)`
//! tuple exactly once, so neither row class duplicates the other. Rows without a
//! `tokenUsage` (thinking, errors, echoes) are skipped; zero-stage rows are
//! dropped by the shared `TokenCounts::is_zero` rule.
//!
//! ## Corruption is a supported state
//! Long-running installs can present a malformed schema (this machine's 2.5 GB
//! db does). Reads are best-effort exactly like the qoder cache db: an
//! unopenable file, a missing table or a corrupt read yields an empty outcome
//! with the **unchanged** cursor — never a fabricated number, and never a
//! cursor advance that would hide rows once the store heals.

use std::path::{Path, PathBuf};

/// Read by CatPaw itself; overridable so a test can point the adapter at a
/// fixture tree instead of a user's home.
pub const ENV_CATPAW_DATA: &str = "CATPAW_DATA_DIR";

/// `~/.sankuai/CatPawAI` — the IDE's cross-workspace data root.
pub fn data_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_CATPAW_DATA) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|h| h.join(".sankuai").join("CatPawAI"))
}

/// The one sqlite file that matters. Cursor = max `rowid` of `t_ui_messages`.
pub fn db_path() -> Option<PathBuf> {
    let p = data_root()?.join("sqliteDB").join("globalCache.sqlite");
    p.is_file().then_some(p)
}

/// Agent transcripts hold no usage numbers; they exist so the panel's "sources"
/// row can point a user somewhere human-readable. Kept for the hint text.
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
}

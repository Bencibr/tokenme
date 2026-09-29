//! Where DSH (DeepSeek's agent desktop, `@deepseek-ai/dsh-app-boot`) keeps its
//! usage, and which of the two trees is authoritative.
//!
//! Measured on this machine (2026-09-25): `~/.dsh/sessions` and
//! `~/Library/Application Support/dsh-desktop/harness/sessions` hold the exact
//! same 18 `session.jsonl.zstd` files (md5-identical) — mirrors, not sources.
//! `~/.dsh` wins, reading both would double-bill.
//!
//! ## Record shape
//! Each file is a zstd-compressed JSONL event stream for one agent session:
//! a `session` header line (`id`, `cwd`), `request/header` lines carrying
//! `data.header.config.model`, and per-step `assistant/chunk` lines whose
//! `data.chunk` is `{"type":"usage","usage":{inputTokens, outputTokens,
//! cacheReadTokens, reasoningTokens}}` — one billed call each.
//!
//! ## Token convention
//! Exclusive: `inputTokens` excludes `cacheReadTokens` (step 1 of a fresh
//! session reads `input 13322 / cacheRead 0`; the next step reads `input 601`
//! with `cacheRead 13440` — the cached context re-appears as its own stage).
//! No `credits` field exists; DSH bills in tokens.
//!
//! ## Cursor
//! `FileKind::Tree`: the whole file is decompressed and re-parsed on change and
//! the cursor lands on the file size; re-emitted events re-enter the dedupe
//! index under stable `<session>#<seq>` keys, so a re-read never double-bills.

use std::path::{Path, PathBuf};

/// Read by DSH itself; overridable so a test can point the adapter at a
/// fixture tree.
pub const ENV_DSH_HOME: &str = "DSH_HOME";

pub fn dsh_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_DSH_HOME) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|h| h.join(".dsh"))
}

/// `~/.dsh/sessions/<workspace-slug>/<session>/session.jsonl.zstd`. The mirror
/// under `~/Library/Application Support/dsh-desktop/harness/sessions` is
/// deliberately not consulted (module notes).
pub fn sessions_dir() -> Option<PathBuf> {
    let p = dsh_home()?.join("sessions");
    p.is_dir().then_some(p)
}

pub fn is_session_file(path: &Path) -> bool {
    path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some("session.jsonl.zstd")
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
    fn home_honours_the_env_override() {
        let _env = lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_DSH_HOME, dir.path());
        assert_eq!(dsh_home().as_deref(), Some(dir.path()));
        assert!(sessions_dir().is_none(), "no sessions under the temp home");
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        assert_eq!(
            sessions_dir().as_deref(),
            Some(dir.path().join("sessions").as_path())
        );
        std::env::remove_var(ENV_DSH_HOME);
        // The fallback is home-resolved even when the real `~/.dsh/sessions`
        // is absent — a machine can carry `.dsh` without ever opening a
        // session — so the existence filter belongs to the expectation, not
        // to `sessions_dir`.
        let expected = dirs::home_dir().map(|h| h.join(".dsh/sessions"));
        assert_eq!(
            sessions_dir().as_deref(),
            expected.filter(|p| p.is_dir()).as_deref()
        );
    }

}

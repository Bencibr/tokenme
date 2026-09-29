//! Where DSH (DeepSeek's agent desktop, `@deepseek-ai/dsh-app-boot`) keeps its
//! usage, and which of the trees is authoritative.
//!
//! Measured on the macOS machine (2026-09-25): `~/.dsh/sessions` and
//! `~/Library/Application Support/dsh-desktop/harness/sessions` hold the exact
//! same 18 `session.jsonl.zstd` files (md5-identical) — mirrors, not sources.
//! `~/.dsh` wins, reading both would double-bill. On Windows there is no
//! `~/.dsh/sessions` at all (measured 2026-09-29: `~/.dsh` carries only
//! agents/plugins/profiles/skills) and the writer's root is
//! `%APPDATA%\dsh-desktop\harness\sessions` — so the roots form a fallback
//! chain, first existing wins, still one source.
//!
//! ## Record shape
//! Each file is a zstd-compressed JSONL event stream for one agent session:
//! a `session` header line (`id`, `cwd`; format v4 adds `"version":4` and
//! renames the file `session.v4.jsonl.zstd`), `request/header` lines carrying
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

/// `~/.dsh/sessions/<workspace-slug>/<session>/session.jsonl.zstd` — the macOS
/// layout, where the `Application Support/dsh-desktop/harness/sessions` mirror
/// is deliberately not consulted while the primary exists (module notes).
/// Windows has no `~/.dsh/sessions` at all (`~/.dsh` carries only
/// agents/plugins/profiles/skills): there the writer's root is
/// `%APPDATA%\dsh-desktop\harness\sessions`, the same tree the macOS mirror
/// mirrors. First existing root wins — one root, never double-billed.
pub fn sessions_dir() -> Option<PathBuf> {
    if std::env::var_os(ENV_DSH_HOME).is_some() {
        // The override is authoritative: a fixture home with no sessions
        // answers "no sessions", it does not fall through to the real tree.
        let p = dsh_home()?.join("sessions");
        return p.is_dir().then_some(p);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".dsh").join("sessions"));
    }
    if let Some(config) = dirs::config_dir() {
        candidates.push(config.join("dsh-desktop").join("harness").join("sessions"));
    }
    candidates.into_iter().find(|p| p.is_dir())
}

/// The writer names its stream `session.jsonl.zstd`; format v4 (the session
/// header carries `"version":4`, everything else the same shape) names it
/// `session.v4.jsonl.zstd` — same directory layout either way.
pub fn is_session_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("session.jsonl.zstd") | Some("session.v4.jsonl.zstd")
    )
}

/// A v3 stream carries the session's per-call events. A v4 stream is
/// header-only by design — its numbers live in the projection cache — so
/// discovery reads v3 streams and skips v4 ones (see [`projcache_dir`]).
pub fn is_v3_stream(path: &Path) -> bool {
    path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some("session.jsonl.zstd")
}

/// The format-v4 stream, named `session.v4.jsonl.zstd`. Its presence is what
/// marks a session as projection-backed: a projection is read only when this
/// file exists for the same session id, so a v3 session is never billed from
/// both its stream and its projection.
pub fn is_v4_stream(path: &Path) -> bool {
    path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some("session.v4.jsonl.zstd")
}

/// `<harness>/storages/session_projcache/sessions` — the format-v4 session
/// projections, one JSON per session, sibling of the sessions root. The
/// v4 stream file stays a one-line header forever; this is where its tokens
/// actually live (module notes in [`proj`]).
pub fn projcache_dir() -> Option<PathBuf> {
    let harness = sessions_dir()?.parent()?.to_path_buf();
    let p = harness.join("storages").join("session_projcache").join("sessions");
    p.is_dir().then_some(p)
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
        // The override lifted, the first *existing* root wins: `~/.dsh/sessions`
        // where it exists, else the writer's app-data root (the Windows layout,
        // identical-content mirror on macOS). A machine can carry `.dsh`
        // without ever opening a session, so the existence filter belongs to
        // the expectation, not to `sessions_dir`.
        let fallbacks: Vec<PathBuf> = [
            dirs::home_dir().map(|h| h.join(".dsh").join("sessions")),
            dirs::config_dir().map(|c| c.join("dsh-desktop").join("harness").join("sessions")),
        ]
        .into_iter()
        .flatten()
        .collect();
        let expected = fallbacks.into_iter().find(|p| p.is_dir());
        assert_eq!(sessions_dir().as_deref(), expected.as_deref());
    }

    #[test]
    fn both_session_stream_names_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let v3 = dir.path().join("session.jsonl.zstd");
        let v4 = dir.path().join("session.v4.jsonl.zstd");
        std::fs::write(&v3, b"x").unwrap();
        std::fs::write(&v4, b"x").unwrap();
        assert!(is_session_file(&v3));
        assert!(is_session_file(&v4));
        // A workspace sibling that only looks close is not a stream.
        let stray = dir.path().join("session.v4.jsonl");
        std::fs::write(&stray, b"x").unwrap();
        assert!(!is_session_file(&stray));
    }
}

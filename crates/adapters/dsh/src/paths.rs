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
/// fixture home.
pub const ENV_DSH_HOME: &str = "DSH_HOME";
/// Overrides the desktop harness root (the `dsh-desktop` app-data tree), same
/// side-by-side-install escape hatch the other adapters carry.
pub const ENV_DSH_DESKTOP: &str = "DSH_DESKTOP_HOME";

pub fn dsh_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_DSH_HOME) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|h| h.join(".dsh"))
}

/// The desktop harness root: the app-data tree that carries `sessions/` and
/// `storages/session_projcache/`.
fn harness_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_DSH_DESKTOP) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::config_dir().map(|c| c.join("dsh-desktop").join("harness"))
}

/// Every existing sessions root, priority order. The trees mirror each other
/// by identity (same workspace slug + session dir in each), so the loader
/// reads a session from the first root that carries it and never both.
///
/// Measured 2026-09-29 on the macOS machine: `~/.dsh/sessions` stopped at
/// Aug 20 (the CLI-era store, 18 v3 streams), while the live desktop app
/// writes v4 streams + projections ONLY under `dsh-desktop/harness/` — the
/// old first-existing-wins single-root choice made every desktop session
/// invisible the moment the stale home root existed. Hence a root LIST.
pub fn sessions_roots() -> Vec<PathBuf> {
    let fixture = env_dir(ENV_DSH_HOME).is_some();
    let mut candidates: Vec<PathBuf> = Vec::new();
    match env_dir(ENV_DSH_HOME) {
        // A fixture home answers from itself; when the desktop root is ALSO
        // pinned both are evaluated (that is the two-root fixture mode).
        Some(home) => candidates.push(home.join("sessions")),
        None => {
            if let Some(h) = dirs::home_dir() {
                candidates.push(h.join(".dsh").join("sessions"));
            }
        }
    }
    match env_dir(ENV_DSH_DESKTOP) {
        Some(desktop) => candidates.push(desktop.join("harness").join("sessions")),
        // Fixture isolation: without an explicit desktop root, a fixture test
        // must not leak the real machine's live sessions into its results.
        None if !fixture => {
            if let Some(h) = harness_root() {
                candidates.push(h.join("sessions"));
            }
        }
        None => {}
    }
    candidates.into_iter().filter(|p| p.is_dir()).collect()
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|d| !d.is_empty()).map(PathBuf::from)
}

/// The first existing root — the display root for `probe`, nothing more.
pub fn sessions_dir() -> Option<PathBuf> {
    sessions_roots().into_iter().next()
}

/// The projection dirs, one sibling of each sessions root, priority order —
/// a root that exists but carries no `storages/` (the stale CLI-era home)
/// simply contributes none. The projection caches are the writer's own
/// cumulative ledger: the accuracy audit (`doctor::ledger`) reads them as the
/// vendor's side of the reconciliation; billing never does (the streams carry
/// the per-call events).
pub fn projcache_dirs() -> Vec<PathBuf> {
    sessions_roots()
        .iter()
        .filter_map(|r| r.parent())
        .map(|h| h.join("storages").join("session_projcache").join("sessions"))
        .filter(|p| p.is_dir())
        .collect()
}

/// The writer names its stream `session.jsonl.zstd`; format v4 (the session
/// header carries `"version":4`, everything else the same shape) names it
/// `session.v4.jsonl.zstd` — same directory layout either way. Both bill from
/// their own events — v3 on `assistant/chunk`, v4 on `assistant/message` (see
/// the parser).
pub fn is_session_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("session.jsonl.zstd") | Some("session.v4.jsonl.zstd")
    )
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
    fn roots_are_priority_ordered_and_existence_filtered() {
        let _env = lock_env();
        let home = tempfile::tempdir().unwrap();
        let desktop = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_DSH_HOME, home.path());
        std::env::set_var(ENV_DSH_DESKTOP, desktop.path());
        // Neither carries a sessions tree yet: no roots, no adapter.
        assert!(sessions_roots().is_empty());
        assert!(projcache_dirs().is_empty());

        // The home root appears first the moment it exists.
        let home_sessions = home.path().join("sessions");
        std::fs::create_dir_all(&home_sessions).unwrap();
        assert_eq!(sessions_roots(), vec![home_sessions.clone()]);

        // The harness root joins the chain when it appears; home keeps priority.
        let harness_sessions = desktop.path().join("harness").join("sessions");
        std::fs::create_dir_all(&harness_sessions).unwrap();
        assert_eq!(sessions_roots(), vec![home_sessions, harness_sessions.clone()]);

        // Projection dirs derive per root and are existence-filtered: the
        // rootless home contributes none, the harness contributes its own.
        let projcache = desktop
            .path()
            .join("harness")
            .join("storages")
            .join("session_projcache")
            .join("sessions");
        std::fs::create_dir_all(&projcache).unwrap();
        assert_eq!(projcache_dirs(), vec![projcache]);

        std::env::remove_var(ENV_DSH_HOME);
        std::env::remove_var(ENV_DSH_DESKTOP);
    }

    #[test]
    fn a_fixture_home_without_a_desktop_root_stays_isolated() {
        let _env = lock_env();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_DSH_HOME, home.path());
        assert!(sessions_roots().is_empty(), "no real tree leaks into a fixture home");
        std::env::remove_var(ENV_DSH_HOME);
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

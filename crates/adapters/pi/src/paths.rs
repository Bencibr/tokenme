//! Where a Pi-dialect product keeps its session logs on this machine.
//!
//! Layout: `<sessions>/<cwd with '/' replaced by '-'>/<ISO>_<uuid>.jsonl`.
//!
//! The coordinates below are the only thing a sibling product changes: a tool
//! that writes the same `type:"message"` transcripts gets this [`crate::parser`]
//! unchanged, and a different sessions root, env var and tool id.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// ccusage's Pi adapter reads this and it names the *sessions* directory
/// (`${PI_AGENT_DIR:-~/.pi/agent/sessions}`), so the two tools agree on one
/// override.
pub(crate) const SESSION_DIR_ENV: &str = "PI_AGENT_DIR";
/// Pi's own variable, documented in `docs/environment-variables.md`; it names
/// the config dir, i.e. one level above the sessions dir.
pub(crate) const CONFIG_DIR_ENV: &str = "PI_CODING_AGENT_DIR";
pub(crate) const SESSIONS_SUBDIR: &str = "sessions";
pub(crate) const CONFIG_DIR_NAME: &str = ".pi";
/// Transcripts written by `pi-subagents` under the sessions root. They replay
/// calls already billed in the parent session, so they are never a source.
pub(crate) const DERIVED_DIR: &str = "subagent-artifacts";

/// One product that writes Pi's transcript shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Product {
    /// Tool id stamped onto every event this product's records produce.
    pub id: &'static str,
    pub display: &'static str,
    /// Names the sessions directory directly. For Pi this is ccusage's
    /// `PI_AGENT_DIR`; Cola publishes no variable of its own (see [`COLA`]), so
    /// its entry here is tokenme's, following the same convention.
    pub sessions_env: &'static str,
    /// Names the *config* directory, i.e. one level above the sessions dir, for
    /// a product that has such a second override.
    pub config_env: Option<&'static str>,
    /// Default sessions root, relative to `$HOME`.
    pub default_rel: &'static [&'static str],
}

pub(crate) static PI: Product = Product {
    id: "pi",
    display: "Pi",
    sessions_env: SESSION_DIR_ENV,
    config_env: Some(CONFIG_DIR_ENV),
    default_rel: &[CONFIG_DIR_NAME, "agent", SESSIONS_SUBDIR],
};

/// Cola keeps its coding transcripts in `~/.cola/sessions/<name>/<ISO>_<uuid>.jsonl`
/// with Pi's record shape, one `type:"message"` line per billed call.
///
/// Verified on this machine: nothing under `~/.cola` — `settings.json`,
/// `crons.json`, `state/`, `identity/`, `logs/` — names a sessions directory,
/// and no `COLA_*` variable appears in any of its config files. Cola therefore
/// has no sessions-dir override to honour, so `COLA_SESSIONS_DIR` is tokenme's
/// own (mirroring `PI_AGENT_DIR`) and `~/.cola/sessions` stays the default.
pub(crate) static COLA: Product = Product {
    id: "cola",
    display: "Cola",
    sessions_env: "COLA_SESSIONS_DIR",
    config_env: None,
    default_rel: &[".cola", SESSIONS_SUBDIR],
};

/// Pi's sessions root. See [`sessions_dir_for`] for the generic form.
#[allow(dead_code)] // frozen surface: behaviour must not change for Pi
pub(crate) fn sessions_dir() -> Option<PathBuf> {
    sessions_dir_for(&PI)
}

/// `$<sessions_env>`, else `$<config_env>/sessions`, else the product's default.
pub(crate) fn sessions_dir_for(product: &Product) -> Option<PathBuf> {
    sessions_dir_in(
        product,
        std::env::var_os(product.sessions_env).as_deref(),
        product
            .config_env
            .and_then(std::env::var_os)
            .as_deref(),
        dirs::home_dir().as_deref(),
    )
}

/// Pi's own precedence table, driven by the frozen [`sessions_dir`] arguments.
/// The adapters go through [`sessions_dir_for`], which reads the environment for
/// whichever product it is given.
#[allow(dead_code)] // frozen surface: the Pi precedence test drives it directly
pub(crate) fn sessions_dir_from(
    sessions: Option<&OsStr>,
    config: Option<&OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    sessions_dir_in(&PI, sessions, config, home)
}

/// Split out from [`sessions_dir_for`] so tests can drive every precedence
/// branch without racing the process-global environment.
pub(crate) fn sessions_dir_in(
    product: &Product,
    sessions: Option<&OsStr>,
    config: Option<&OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = usable(sessions) {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = usable(config) {
        return Some(PathBuf::from(dir).join(SESSIONS_SUBDIR));
    }
    let mut root = home?.to_path_buf();
    for part in product.default_rel {
        root = root.join(part);
    }
    Some(root)
}

fn usable(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|v| !v.is_empty())
}

pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let since = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())?;
    Some(since.as_millis() as i64)
}

/// The session id is also the tail of the file name (`<ISO>_<uuid>.jsonl`),
/// which is how a file whose `session` header line sits behind the cursor is
/// still labelled with the right session.
pub(crate) fn session_from_path(path: &Path) -> String {
    let stem = path.file_stem().and_then(OsStr::to_str).unwrap_or_default();
    match stem.rsplit_once('_') {
        Some((_, uuid)) if !uuid.is_empty() => uuid.to_string(),
        _ => stem.to_string(),
    }
}

/// Reverse of the directory mangling (`/` and any literal `-` both collapse to
/// `-`), so it is lossy for workspaces with dashes in a segment. Only a
/// fallback: the `session` record's own `cwd` is authoritative.
pub(crate) fn decode_cwd(dir: &OsStr) -> Option<String> {
    let raw = dir.to_str()?;
    let body = raw.strip_prefix("--")?.strip_suffix("--")?;
    (!body.is_empty()).then(|| format!("/{}", body.replace('-', "/")))
}

/// The workspace label for a log, recovered from its own path.
pub(crate) fn project_from_path(path: &Path) -> Option<String> {
    decode_cwd(path.parent()?.file_name()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_precedence_picks_the_sessions_root() {
        let home = Path::new("/home/x");
        assert_eq!(
            sessions_dir_from(
                Some(OsStr::new("/tmp/sessions")),
                Some(OsStr::new("/tmp/cfg")),
                Some(home)
            ),
            Some(PathBuf::from("/tmp/sessions"))
        );
        assert_eq!(
            sessions_dir_from(None, Some(OsStr::new("/tmp/cfg")), Some(home)),
            Some(PathBuf::from("/tmp/cfg/sessions"))
        );
        // An empty override is "unset", not "the current directory".
        assert_eq!(
            sessions_dir_from(Some(OsStr::new("")), Some(OsStr::new("")), Some(home)),
            Some(PathBuf::from("/home/x/.pi/agent/sessions"))
        );
        assert_eq!(sessions_dir_from(None, None, None), None);
    }

    #[test]
    fn file_name_and_directory_carry_the_labels() {
        let p = Path::new("/h/.pi/agent/sessions/--Users-sp-work-x--/2026-09-22T14-16-39-708Z_01a0c979-d41a.jsonl");
        assert_eq!(session_from_path(p), "01a0c979-d41a");
        assert_eq!(project_from_path(p).as_deref(), Some("/Users/me/work/x"));
        // Lossy by construction: a dashed segment cannot be told apart from a
        // separator, which is why the record's `cwd` always wins.
        let d = Path::new("/s/--Users-sp-orca-neuro-tdd-backfill--/2026_a.jsonl");
        assert_eq!(
            project_from_path(d).as_deref(),
            Some("/Users/me/orca/neuro/tdd/backfill")
        );
    }

    #[test]
    fn undecodable_paths_yield_no_label() {
        assert_eq!(project_from_path(Path::new("/s/loose/a.jsonl")), None);
        // `----` is the mangled filesystem root: no workspace to name.
        assert_eq!(project_from_path(Path::new("/s/----/a.jsonl")), None);
        assert_eq!(session_from_path(Path::new("")), "");
    }

    /// A sibling changes the coordinates and nothing else: Pi keeps answering
    /// exactly as before, Cola resolves its own root.
    #[test]
    fn each_product_resolves_its_own_sessions_root() {
        let home = Path::new("/home/x");
        // The frozen `sessions_dir()` entry point and the product table cannot drift.
        assert_eq!(sessions_dir(), sessions_dir_for(&PI));
        assert_eq!(
            sessions_dir_in(&PI, None, None, Some(home)),
            Some(PathBuf::from("/home/x/.pi/agent/sessions"))
        );
        assert_eq!(
            sessions_dir_in(&COLA, None, None, Some(home)),
            Some(PathBuf::from("/home/x/.cola/sessions"))
        );
        assert_eq!(
            sessions_dir_in(&COLA, Some(OsStr::new("/tmp/cola-sessions")), None, Some(home)),
            Some(PathBuf::from("/tmp/cola-sessions"))
        );
        // Cola declares no config-dir variable at all, so only its one override
        // and the default exist for it.
        assert_eq!(COLA.config_env, None);
        assert_eq!(PI.config_env, Some(CONFIG_DIR_ENV));
        assert_eq!(
            sessions_dir_in(&COLA, Some(OsStr::new("")), None, Some(home)),
            Some(PathBuf::from("/home/x/.cola/sessions")),
            "an empty override is unset, not the working directory"
        );
    }
}

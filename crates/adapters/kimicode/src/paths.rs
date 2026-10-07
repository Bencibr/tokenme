//! Locating a Kimi Code home and the wire logs inside it.
//!
//! The vendor resolves its own home as `KIMI_CODE_HOME`, else `~/.kimi-code`
//! (`packages/oauth` and the CLI's own theme docs both say **never assume
//! `~/.kimi-code`**), and the pre-v2 product lived at `~/.kimi` — its
//! `packages/migration-legacy` moves those sessions rather than deleting them,
//! so both roots are read and the record signature is what keeps a migrated
//! session from being billed twice.
//!
//! `KIMI_DATA_DIR` is tokenme's own override, following `OPENCODE_DATA_DIR`:
//! a test or a relocated install is the only thing that sets it.
//!
//! The desktop client is the third host: `Kimi.app` provisions a whole kimi-code
//! home under its Electron userData (`<data_dir>/kimi-desktop/daimon-share/daimon/
//! runtime/kimi-code/home` — the app's own provision log calls it the "Kimi Code
//! runtime") and runs the same agent engine there. Its wire logs count as this
//! product's usage; see [`desktop_home`].

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Mutex;

/// The vendor's variable; a single directory.
pub(crate) const ENV_CODE_HOME: &str = "KIMI_CODE_HOME";
/// tokenme's override, so a fixture can pin the root without pretending the
/// product is installed. Comma-separated, like the other roots in this crate.
pub(crate) const ENV_DATA_DIR: &str = "KIMI_DATA_DIR";
/// Keeps the environment-touching tests off each other's variables. Nothing in
/// the runtime path reads an environment variable under a lock, so this exists
/// only for the tests that do.
#[cfg(test)]
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The file name every Kimi Code layout writes its event log under.
const WIRE_FILE: &str = "wire.jsonl";
/// The directory holding one session per workspace, then one agent subtree each.
const SESSIONS_DIR: &str = "sessions";

/// Every existing home, in priority order: an env override pins exactly one, and
/// otherwise `~/.kimi-code` and the legacy `~/.kimi` both count when they exist.
pub(crate) fn homes() -> Vec<PathBuf> {
    for name in [ENV_CODE_HOME, ENV_DATA_DIR] {
        let Ok(value) = std::env::var(name) else { continue };
        if name == ENV_CODE_HOME {
            let single = value.trim();
            if !single.is_empty() {
                return exists(vec![PathBuf::from(single)]);
            }
            continue;
        }
        let listed: Vec<PathBuf> = value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if !listed.is_empty() {
            return exists(listed);
        }
    }
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let mut defaults = vec![home.join(".kimi-code"), home.join(".kimi")];
    if let Some(desktop) = desktop_home() {
        defaults.push(desktop);
    }
    exists(defaults)
}

/// The desktop app's embedded Kimi Code runtime home. The path segments after
/// `kimi-desktop` are the ones the app's provision log prints and its relocation
/// regexes key on ("daimon-share" is treated as a stable marker in the app
/// bundle), so this is the layout, not a version.
fn desktop_home() -> Option<PathBuf> {
    Some(desktop_home_under(&dirs::data_dir()?))
}

fn desktop_home_under(data_dir: &Path) -> PathBuf {
    let mut path = data_dir.to_path_buf();
    for seg in ["kimi-desktop", "daimon-share", "daimon", "runtime", "kimi-code", "home"] {
        path.push(seg);
    }
    path
}

fn exists(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        // The same directory reached twice (a symlinked home, an XDG-style
        // alias) would put one wire file into the manifest under two keys.
        .filter(|p| p.is_dir() && seen.insert(p.clone()))
        .collect()
}

/// Every `wire.jsonl` under the homes, sorted so a pass is reproducible.
///
/// Two layouts are read on purpose: `sessions/<group>/<session>/wire.jsonl`
/// (v1, and the v2 main agent's own log) and
/// `sessions/<workspace>/<session>/agents/<agent>/wire.jsonl` (v2 subagents).
/// The vendor's own scanner does exactly this — `sessionDir/wire.jsonl` plus a
/// recursive sweep of `sessionDir/agents` — so a session whose subagent work was
/// never read is not a session this adapter can claim to have measured.
pub(crate) fn wire_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for home in homes() {
        collect(&home.join(SESSIONS_DIR), &mut files);
    }
    files.sort();
    files.dedup();
    files
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.file_name().is_some_and(|n| n == WIRE_FILE) {
            out.push(path);
        }
    }
}

/// Every `<home>/workspaces.json` that exists, in home order. Each home names
/// its own workspaces (the desktop runtime ids read `wd_<name>_<hash>`), so the
/// lookup goes across all of them — first map to hold an id wins it.
pub(crate) fn workspaces_files() -> Vec<PathBuf> {
    homes()
        .into_iter()
        .map(|h| h.join("workspaces.json"))
        .filter(|p| p.is_file())
        .collect()
}

/// The three path segments a wire log answers for: which workspace, which
/// session, which agent.
///
/// `…/sessions/<ws>/<session>/agents/<agent>/wire.jsonl` is the v2 layout, where
/// a session's subagents each own a directory. The legacy
/// `…/sessions/<group>/<session>/wire.jsonl` has no agents level, and in that
/// layout the file *is* the main agent's log — which is why the agent there is
/// reported as `main` rather than left unnamed.
pub(crate) fn layout(path: &Path) -> (Option<String>, String, String) {
    let name = |p: Option<&Path>| -> String {
        p.and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let present = |s: String| (!s.is_empty()).then_some(s);

    let agent_dir = path.parent();
    let agents_dir = agent_dir.and_then(|p| p.parent());
    if agents_dir.and_then(|p| p.file_name()).is_some_and(|n| n == "agents") {
        let session_dir = agents_dir.and_then(|p| p.parent());
        return (
            present(name(session_dir.and_then(|p| p.parent()))),
            name(session_dir),
            name(agent_dir),
        );
    }
    (
        present(name(agent_dir.and_then(|p| p.parent()))),
        name(agent_dir),
        "main".to_string(),
    )
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vendor_home_variable_pins_exactly_one_root() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_CODE_HOME, dir.path());
        assert_eq!(homes(), vec![dir.path().to_path_buf()]);
        // An empty value is not a root: the ladder falls through to the defaults.
        std::env::set_var(ENV_CODE_HOME, "  ");
        let got = homes();
        assert!(!got.contains(&PathBuf::from("  ")), "blank override ignored: {got:?}");
        std::env::remove_var(ENV_CODE_HOME);
    }

    #[test]
    fn a_missing_root_yields_no_wires() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_DATA_DIR, dir.path().join("absent"));
        assert!(homes().is_empty());
        assert!(wire_files().is_empty());
        assert!(workspaces_files().is_empty());
        std::env::remove_var(ENV_DATA_DIR);
    }

    #[test]
    fn the_desktop_runtime_home_is_the_layout_the_app_provisions() {
        // The app's provision log prints exactly this path under its userData;
        // the segments are what its own relocation regexes key on.
        let base = Path::new("/base");
        assert_eq!(
            desktop_home_under(base),
            base.join("kimi-desktop")
                .join("daimon-share")
                .join("daimon")
                .join("runtime")
                .join("kimi-code")
                .join("home")
        );
    }

    #[test]
    fn both_layouts_are_found_and_named_by_their_session_and_agent() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::env::set_var(ENV_DATA_DIR, root);
        let v2 = root.join("sessions").join("wd_x").join("ses_1").join("agents").join("sub");
        let v2_main = root.join("sessions").join("wd_x").join("ses_1").join("agents").join("main");
        let v1 = root.join("sessions").join("group").join("ses_old");
        for d in [&v2, &v2_main, &v1] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("wire.jsonl"), "").unwrap();
        }
        // A stray jsonl that is not a wire log must not be read.
        std::fs::write(v1.join("other.jsonl"), "").unwrap();
        let found = wire_files();
        assert_eq!(found.len(), 3, "only wire.jsonl counts: {found:?}");
        assert_eq!(layout(&v2.join("wire.jsonl")), (Some("wd_x".into()), "ses_1".into(), "sub".into()));
        assert_eq!(layout(&v2_main.join("wire.jsonl")), (Some("wd_x".into()), "ses_1".into(), "main".into()));
        // v1 has no `agents/` level, and the file is the main agent's own log.
        assert_eq!(layout(&v1.join("wire.jsonl")), (Some("group".into()), "ses_old".into(), "main".into()));
        std::env::remove_var(ENV_DATA_DIR);
    }
}

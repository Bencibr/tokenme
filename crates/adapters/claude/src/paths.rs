//! Where Claude Code keeps its logs on this machine.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

pub(crate) const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
pub(crate) const PROJECTS_DIR: &str = "projects";

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`. `None` when neither resolves lets
/// every entry point bail without touching the filesystem.
pub(crate) fn config_root() -> Option<PathBuf> {
    root_from(std::env::var_os(CONFIG_DIR_ENV).as_deref(), dirs::home_dir().as_deref())
}

pub(crate) fn root_from(env: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = env.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.map(|h| h.join(".claude"))
}

/// The root a log file belongs to, recovered from its own path. Preferred over
/// [`config_root`] once we have a file, so a `CLAUDE_CONFIG_DIR` switch between
/// `discover` and `read` still resolves the whitelists that file was written under.
pub(crate) fn root_for_file(path: &Path) -> Option<PathBuf> {
    let idx = path.components().position(|c| c.as_os_str() == PROJECTS_DIR)?;
    let root = path.components().take(idx).collect::<PathBuf>();
    (!root.as_os_str().is_empty()).then_some(root)
}

pub(crate) fn projects_dir(root: &Path) -> PathBuf {
    root.join(PROJECTS_DIR)
}

pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let since = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok())?;
    Some(since.as_millis() as i64)
}

pub(crate) fn skills_dir(root: &Path) -> PathBuf {
    root.join("skills")
}

/// The flat config file. Out of the box it is `~/.claude.json`, i.e. *beside*
/// `~/.claude`; with `CLAUDE_CONFIG_DIR` set Claude Code writes the same file
/// *inside* the dir. Accept whichever spelling exists.
pub(crate) fn flat_config(root: &Path) -> Option<PathBuf> {
    let parent = root.parent();
    [
        Some(root.join(".claude.json")),
        parent.map(|p| p.join(".claude.json")),
        parent.zip(root.file_name()).map(|(p, name)| p.join(format!("{}.json", name.to_string_lossy()))),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
}

/// The `<root>/projects/<mangled-workspace>/<session>.jsonl` layout carries the
/// workspace path even when a record omits `cwd`.
pub(crate) fn project_segment(path: &Path) -> Option<&OsStr> {
    let mut comps = path.components().peekable();
    while let Some(c) = comps.next() {
        if c.as_os_str() == PROJECTS_DIR {
            // Nested session/subagent dirs still hang off the mangled workspace dir.
            return match comps.next()? {
                Component::Normal(seg) => Some(seg),
                _ => None,
            };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_root_beats_home() {
        assert_eq!(
            root_from(Some(OsStr::new("/tmp/other")), Some(Path::new("/home/x"))),
            Some(PathBuf::from("/tmp/other"))
        );
        assert_eq!(root_from(Some(OsStr::new("")), Some(Path::new("/home/x"))), Some(PathBuf::from("/home/x/.claude")));
        assert_eq!(root_from(None, Some(Path::new("/home/x"))), Some(PathBuf::from("/home/x/.claude")));
        assert_eq!(root_from(None, None), None);
    }

    #[test]
    fn file_path_recovers_its_own_root() {
        let p = Path::new("/h/.claude/projects/-Users-x-proj/sess/subagents/agent-a.jsonl");
        assert_eq!(root_for_file(p), Some(PathBuf::from("/h/.claude")));
        assert_eq!(root_for_file(Path::new("/h/.claude/history.jsonl")), None);
    }

    #[test]
    fn project_segment_is_the_mangled_workspace() {
        let p = Path::new("/h/.claude/projects/-Users-x-proj/sess/agent-a.jsonl");
        assert_eq!(project_segment(p), Some(OsStr::new("-Users-x-proj")));
        assert_eq!(project_segment(Path::new("/elsewhere/a.jsonl")), None);
    }

    #[test]
    fn flat_config_prefers_inside_the_root_then_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".claude");
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(flat_config(&root), None);
        let beside = dir.path().join(".claude.json");
        std::fs::write(&beside, "{}").unwrap();
        assert_eq!(flat_config(&root), Some(beside.clone()));
        let inside = root.join(".claude.json");
        std::fs::write(&inside, "{}").unwrap();
        assert_eq!(flat_config(&root), Some(inside.clone()));
    }
}

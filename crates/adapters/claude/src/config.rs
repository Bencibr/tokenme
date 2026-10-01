//! The user's own MCP / Skill inventory, used to filter the call breakdown down
//! to things they actually installed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

use crate::paths;

#[derive(Debug, Default, Clone)]
pub(crate) struct Config {
    /// Server names from `mcpServers`, top level and per-project.
    mcp: Arc<HashSet<String>>,
    /// Directory names directly under `<root>/skills`.
    skills: Arc<HashSet<String>>,
    /// Install version hint shown in the sources list.
    pub(crate) version: Option<String>,
}

impl Config {
    pub(crate) fn load(root: &Path) -> Self {
        let flat = paths::flat_config(root).and_then(read_json);
        let mut mcp: HashSet<String> = HashSet::new();
        if let Some(cfg) = &flat {
            collect_servers(cfg.get("mcpServers"), &mut mcp);
            if let Some(projects) = cfg.get("projects").and_then(Value::as_object) {
                for project in projects.values() {
                    collect_servers(project.get("mcpServers"), &mut mcp);
                }
            }
        }
        // A missing or unparsable config is normal (fresh install, no MCP added
        // yet): every call then falls out of the breakdown rather than erroring.
        let version = flat
            .as_ref()
            .and_then(|c| c.get("version"))
            .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_f64().map(|n| n.to_string())));
        Config {
            mcp: Arc::new(mcp),
            skills: Arc::new(skill_names(root)),
            version,
        }
    }

    pub(crate) fn allows_mcp(&self, server: &str) -> bool {
        self.mcp.contains(server)
    }

    pub(crate) fn allows_skill(&self, skill: &str) -> bool {
        self.skills.contains(skill)
    }
}

fn read_json(path: PathBuf) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn collect_servers(servers: Option<&Value>, out: &mut HashSet<String>) {
    if let Some(map) = servers.and_then(Value::as_object) {
        out.extend(map.keys().cloned());
    }
}

fn skill_names(root: &Path) -> HashSet<String> {
    let dir = paths::skills_dir(root);
    let mut out = HashSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                out.insert(name.to_string());
            }
        }
    }
    out
}

struct Cached {
    root: PathBuf,
    stamp: (i64, u64),
    config: Arc<Config>,
}

static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();

/// `<root>`'s config, parsed once per ingest batch: the flat file is ~118 KB and
/// `read` runs per log file, so re-parsing it per file would dominate the pass.
/// Keyed on mtime+size so an edit between batches is still picked up.
pub(crate) fn for_root(root: &Path) -> Arc<Config> {
    let stamp = paths::flat_config(root).as_deref().map(file_stamp).unwrap_or((0, 0));
    let mut slot = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner());
    if let Some(cached) = slot.as_ref().filter(|c| c.root == root && c.stamp == stamp) {
        return Arc::clone(&cached.config);
    }
    let config = Arc::new(Config::load(root));
    *slot = Some(Cached { root: root.to_path_buf(), stamp, config: Arc::clone(&config) });
    config
}

fn file_stamp(path: &Path) -> (i64, u64) {
    let Ok(meta) = std::fs::metadata(path) else { return (0, 0) };
    (paths::mtime_ms(&meta).unwrap_or(0), meta.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `<tmp>/.claude` root plus the beside-file `<tmp>/.claude.json`, which is
    /// exactly the layout `~/.claude` uses.
    fn root_with(flat: Option<&str>, skills: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".claude");
        std::fs::create_dir_all(&root).unwrap();
        if let Some(body) = flat {
            std::fs::write(dir.path().join(".claude.json"), body).unwrap();
        }
        for skill in skills {
            std::fs::create_dir_all(root.join("skills").join(skill)).unwrap();
        }
        (dir, root)
    }

    #[test]
    fn missing_config_degrades_to_empty_whitelists() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(&dir.path().join(".claude"));
        assert!(!cfg.allows_mcp("anything"));
        assert!(!cfg.allows_skill("anything"));
        assert!(cfg.version.is_none());
    }

    #[test]
    fn unparsable_config_is_not_an_error() {
        let (_keep, root) = root_with(Some("{ trunc"), &[]);
        assert!(Config::load(&root).mcp.is_empty());
    }

    #[test]
    fn project_servers_and_skills_are_collected() {
        let body = r#"{
            "version": "2.1.278",
            "mcpServers": { "figma": {}, "bugx": {} },
            "projects": { "/x": { "mcpServers": { "firecrawl": {} } }, "/y": {} }
        }"#;
        let (_keep, root) = root_with(Some(body), &["deep-research", "orca"]);
        let cfg = Config::load(&root);
        assert!(cfg.allows_mcp("figma") && cfg.allows_mcp("firecrawl"));
        assert!(!cfg.allows_mcp("builtin-web"));
        assert!(cfg.allows_skill("deep-research") && cfg.allows_skill("orca"));
        assert!(!cfg.allows_skill("Bash"));
        assert_eq!(cfg.version.as_deref(), Some("2.1.278"));
    }

    #[test]
    fn cached_config_is_reloaded_when_the_file_changes() {
        let (keep, root) = root_with(Some(r#"{"mcpServers":{"a":{}}}"#), &[]);
        assert!(for_root(&root).allows_mcp("a"));
        // A hand-edit that changes the file's size or mtime must reach the batch.
        std::fs::write(keep.path().join(".claude.json"), r#"{"mcpServers":{"b":{},"c":{}}}"#)
            .unwrap();
        let second = for_root(&root);
        assert!(second.allows_mcp("b"), "stale cache after edit");
        assert!(!second.allows_mcp("a"));
    }
}

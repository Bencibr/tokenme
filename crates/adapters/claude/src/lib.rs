//! Claude Code adapter.
//!
//! Source: `~/.claude/projects/**/*.jsonl` (append-only, one JSON record per
//! line) plus `~/.claude.json` and `~/.claude/skills/` for the user-installed
//! MCP / Skill whitelists.
//!
//! The public surface (`TOOL_ID`, `ClaudeAdapter`, this trait impl) is frozen:
//! `usage-adapter-all` and the CLI already link against it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use walkdir::WalkDir;

use usage_core::{
    DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

mod config;
mod parser;
mod paths;

use config::Config;

pub const TOOL_ID: &str = "claude";

#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeAdapter;

impl ClaudeAdapter {
    /// The config root is derived from the log file's own path when possible, so
    /// a `CLAUDE_CONFIG_DIR` switch between `discover` and `read` still resolves
    /// the whitelists that file was written under.
    fn config_for(path: &Path) -> Arc<Config> {
        match paths::root_for_file(path).or_else(paths::config_root) {
            Some(root) => config::for_root(&root),
            None => Arc::new(Config::default()),
        }
    }
}

impl SourceAdapter for ClaudeAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            usage_form: usage_core::UsageForm::PerCall,
            meter: usage_core::Meter::Tokens,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let root = paths::config_root()?;
        let projects = paths::projects_dir(&root);
        first_log(&projects)?;
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: vec![projects],
            hint: config::for_root(&root).version.clone(),
        })
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        let Some(root) = paths::config_root() else { return Vec::new() };
        discover_in(&paths::projects_dir(&root), filter)
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        parser::read_file(file, cursor, &Self::config_for(&file.path))
    }
}

/// Session logs live both directly under the mangled project dir and in nested
/// `subagents/` dirs, so this is a walk and not a listing.
fn discover_in(projects: &Path, filter: &DateFilter) -> Vec<SourceFile> {
    if !projects.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<SourceFile> = WalkDir::new(projects)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && is_jsonl(entry.path()))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some(SourceFile {
                path: entry.into_path(),
                kind: FileKind::Jsonl,
                size: meta.len(),
                mtime_ms: paths::mtime_ms(&meta)?,
            })
        })
        // Project dirs are keyed by workspace rather than date, so a file's
        // mtime is the only time signal available before parsing it.
        .filter(|file| filter.since_ms.is_none_or(|since| file.mtime_ms >= since))
        .collect();
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Walk stops at the first readable `*.jsonl`: `probe` runs at startup and must
/// not touch all 430 MB of logs.
fn first_log(projects: &Path) -> Option<PathBuf> {
    let mut walker = WalkDir::new(projects).follow_links(false).into_iter();
    while let Some(Ok(entry)) = walker.next() {
        if !entry.file_type().is_file() || !is_jsonl(entry.path()) {
            continue;
        }
        if readable(entry.path()) {
            return Some(entry.into_path());
        }
    }
    None
}

fn is_jsonl(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("jsonl"))
}

fn readable(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
        && std::fs::File::open(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantics_match_the_declared_contract() {
        // Deliberately spelled out field by field: this is what `usage-index`
        // and the money math key their behaviour off.
        assert_eq!(
            ClaudeAdapter.semantics(),
            Semantics {
                usage_form: usage_core::UsageForm::PerCall,
                meter: usage_core::Meter::Tokens,
                model_attr: usage_core::ModelAttr::Inline,
                dedupes_by_id: true,
                reports_quota: false,
            }
        );
        assert_eq!(ClaudeAdapter.id(), TOOL_ID);
        assert_eq!(ClaudeAdapter.display_name(), "Claude Code");
    }

    #[test]
    fn empty_or_absent_tree_probes_as_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join(paths::PROJECTS_DIR);
        assert_eq!(discover_in(&projects, &DateFilter::default()).len(), 0);
        assert!(first_log(&projects).is_none());
        std::fs::create_dir_all(projects.join("-Users-x")).unwrap();
        // A zero-byte log is not evidence of a working install, so `probe` skips
        // it, but `discover` still lists it: the indexer decides what to do with
        // an empty file instead of the adapter silently hiding it.
        std::fs::write(projects.join("-Users-x").join("empty.jsonl"), "").unwrap();
        assert!(first_log(&projects).is_none());
        assert_eq!(discover_in(&projects, &DateFilter::default()).len(), 1);
    }
}

//! Locating the chat snapshots Tuanjie Codely writes.
//!
//! Both products share one store: the **Codely CLI** (also the agent Cowork's
//! desktop window launches over ACP) saves every session as
//! `~/.codely-cli/tmp/<project-hash>/chat-<session-id>.json`. A message is
//! appended by rewriting the whole object, and the file is the only place the
//! per-call `tokens` object is written down.
//!
//! `CODELY_CLI_HOME` is tokenme's override, following the same pattern as the
//! other adapters: a test or a relocated install pins the root. The vendor's
//! own environment variable is not documented, so the default is the product's
//! real home.

use std::path::{Path, PathBuf};

use serde_json::Value;

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) const ENV_CLI_HOME: &str = "CODELY_CLI_HOME";

/// `~/.codely-cli`, or the override when one is set.
pub(crate) fn cli_home() -> Option<PathBuf> {
    if let Ok(value) = std::env::var(ENV_CLI_HOME) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|home| home.join(".codely-cli"))
}

/// `<home>/tmp`, the directory that holds one project-hash directory per
/// workspace. `None` when the product has never been started.
pub(crate) fn tmp_root() -> Option<PathBuf> {
    Some(cli_home()?.join("tmp"))
}

/// Every `chat-*.json` below `<home>/tmp`, sorted so a pass is reproducible.
///
/// The walk stops at three levels: the real layout is
/// `tmp/<project-hash>/chat-<uuid>.json`, and a deeper tree is not something the
/// product writes. Files such as `logs.json`, `jobs.json` and the session's
/// `tool-call-artifacts/` are deliberately not sources.
pub(crate) fn chat_files() -> Vec<PathBuf> {
    let Some(root) = tmp_root() else { return Vec::new() };
    let mut out = Vec::new();
    collect(&root, 0, &mut out);
    out.sort();
    out.dedup();
    out
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 2 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            collect(&path, depth + 1, out);
        } else if kind.is_file() && is_chat_file(&path) {
            out.push(path);
        }
    }
}

/// `chat-<session>.json` and nothing else.
pub(crate) fn is_chat_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("chat-") && name.ends_with(".json"))
}

/// The workspace label the CLI itself prints into its startup snapshot:
/// `I'm currently working in the directory: <cwd>`. The snapshot is the first
/// `clientHistory` turn, so this never walks the message history.
///
/// The label is the last path segment, matching the other adapters: the panel
/// groups by project name, not by a machine-specific absolute path. A snapshot
/// without the sentence (an older build, a hand-written fixture) yields `None`,
/// never a guessed label.
pub(crate) fn project_from(root: &Value) -> Option<String> {
    let text = root
        .get("clientHistory")?
        .as_array()?
        .first()?
        .get("parts")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()?;
    let rest = text.split_once("I'm currently working in the directory:")?.1;
    let cwd = rest.lines().next()?.trim();
    if cwd.is_empty() {
        return None;
    }
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

/// The payload's session id, then the file tag, then the file name.
pub(crate) fn session_id(root: &Value, file: &Path) -> String {
    root.get("conversationRecord")
        .and_then(|record| record.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| root.get("tag").and_then(Value::as_str).filter(|id| !id.is_empty()))
        .map(str::to_string)
        .or_else(|| {
            file.file_stem()
                .and_then(|stem| stem.to_str())
                .map(|stem| stem.strip_prefix("chat-").unwrap_or(stem).to_string())
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_chat_snapshots_are_sources() {
        assert!(is_chat_file(Path::new("/tmp/a/chat-1.json")));
        assert!(!is_chat_file(Path::new("/tmp/a/logs.json")));
        assert!(!is_chat_file(Path::new("/tmp/a/chat-1.json.tmp")));
        assert!(!is_chat_file(Path::new("/tmp/a/chat-mcp.json.bak")));
    }

    #[test]
    fn the_project_is_the_last_segment_of_the_snapshot_cwd() {
        let value = serde_json::json!({
            "clientHistory": [{"parts": [{"text": "Today's date is …\nI'm currently working in the directory: C:/w/demo\nHere is the folder structure"}]}]
        });
        assert_eq!(project_from(&value).as_deref(), Some("demo"));
        assert_eq!(project_from(&serde_json::json!({})), None);
    }

    #[test]
    fn the_payload_session_wins_over_the_file_name() {
        let root = serde_json::json!({"conversationRecord": {"sessionId": "payload"}});
        assert_eq!(session_id(&root, Path::new("/tmp/h/chat-file.json")), "payload");
        let tagged = serde_json::json!({"tag": "tagged"});
        assert_eq!(session_id(&tagged, Path::new("/tmp/h/chat-file.json")), "tagged");
        assert_eq!(session_id(&serde_json::json!({}), Path::new("/tmp/h/chat-file.json")), "file");
    }

    #[test]
    fn a_missing_override_falls_back_to_the_products_home() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(ENV_CLI_HOME, "  ");
        let home = cli_home().unwrap();
        assert_eq!(home.file_name().and_then(|n| n.to_str()), Some(".codely-cli"));
        std::env::remove_var(ENV_CLI_HOME);
    }
}

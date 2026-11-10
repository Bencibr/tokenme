//! Locating Grok Build session usage files.
//!
//! Grok Build (`grok` / `agent`) stores every conversation under
//! `$GROK_HOME/sessions/<cwd-encoded>/<session-id>/`. The billable ledger is
//! `usage.json` (session + per-turn token totals). `GROK_HOME` defaults to
//! `~/.grok` on Windows, macOS, and Linux — the same path the CLI documents.

use std::path::{Path, PathBuf};

pub(crate) const ENV_GROK_HOME: &str = "GROK_HOME";

/// `$GROK_HOME`, or `~/.grok` when the override is unset / blank.
pub(crate) fn grok_home() -> Option<PathBuf> {
    if let Ok(value) = std::env::var(ENV_GROK_HOME) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|home| home.join(".grok"))
}

/// `$GROK_HOME/sessions`, or `None` when the product has never been started.
pub(crate) fn sessions_root() -> Option<PathBuf> {
    Some(grok_home()?.join("sessions"))
}

/// Every `usage.json` under `sessions/<cwd>/<session-id>/`, sorted for a
/// reproducible pass. The walk stops at three levels below `sessions/`: the
/// real layout never nests deeper, and files such as `chat_history.jsonl`,
/// `events.jsonl`, and `session_search.sqlite` are deliberately not sources.
pub(crate) fn usage_files() -> Vec<PathBuf> {
    let Some(root) = sessions_root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect(&root, 0, &mut out);
    out.sort();
    out.dedup();
    out
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            collect(&path, depth + 1, out);
        } else if kind.is_file() && is_usage_file(&path) {
            out.push(path);
        }
    }
}

pub(crate) fn is_usage_file(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("usage.json")
}

/// Session id is the directory that holds `usage.json`.
pub(crate) fn session_id(usage_path: &Path) -> String {
    usage_path
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

/// Workspace label from sibling `summary.json` (`info.cwd`), else the
/// percent-decoded parent of the session directory (Grok encodes the cwd as
/// the sessions folder name).
pub(crate) fn project_for(usage_path: &Path) -> Option<String> {
    if let Some(from_summary) = cwd_from_summary(usage_path) {
        return Some(from_summary);
    }
    decode_cwd_dir(usage_path)
}

fn cwd_from_summary(usage_path: &Path) -> Option<String> {
    let summary = usage_path.parent()?.join("summary.json");
    let text = std::fs::read_to_string(summary).ok()?;
    let root: serde_json::Value = serde_json::from_str(&text).ok()?;
    root.get("info")
        .and_then(|info| info.get("cwd"))
        .and_then(|cwd| cwd.as_str())
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string)
}

fn decode_cwd_dir(usage_path: &Path) -> Option<String> {
    let encoded = usage_path.parent()?.parent()?.file_name()?.to_str()?;
    let decoded = percent_decode(encoded);
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Minimal percent-decoder for Grok's cwd folder names (`C%3A%5CUsers%5C…`).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_restores_windows_cwd() {
        assert_eq!(
            percent_decode("C%3A%5CUsers%5Ckouyo"),
            r"C:\Users\kouyo"
        );
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
    }

    #[test]
    fn usage_file_name_is_exact() {
        assert!(is_usage_file(Path::new("/tmp/sessions/a/b/usage.json")));
        assert!(!is_usage_file(Path::new("/tmp/sessions/a/b/chat_history.jsonl")));
    }
}

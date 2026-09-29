//! Where Cline keeps its transcripts, and how to walk back from a transcript to
//! the session directory that names it.
//!
//! Layout measured here: `~/.cline/data/sessions/<ts>_<id>/` holding
//! `<ts>_<id>.json` (session meta) and `<ts>_<id>.messages.json` (the transcript).
//! 10 session dirs exist, only 5 have a transcript, so the pairing must be
//! tolerant in both directions.

use std::path::{Path, PathBuf};

/// Env var Cline's own CLI reads for this directory (`session-paths.ts`), honoured
/// first so a test or a relocated profile can point the adapter elsewhere.
pub const ENV_SESSIONS_DIR: &str = "CLINE_DATA_DIR_SESSIONS";

/// Default sessions dir for the CLI: `~/.cline/data/sessions`.
pub const DEFAULT_SESSIONS_SUBPATH: &str = ".cline/data/sessions";

/// Only the transcript carries per-call usage.
pub const MESSAGES_SUFFIX: &str = ".messages.json";

pub fn sessions_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_SESSIONS_DIR) {
        if !dir.is_empty() {
            let p = PathBuf::from(dir);
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    let p = dirs::home_dir()?.join(DEFAULT_SESSIONS_SUBPATH);
    p.is_dir().then_some(p)
}

/// True for the one file per session dir that carries per-call `metrics`.
pub fn is_messages_file(path: &Path) -> bool {
    path.is_file()
        && path.file_name().and_then(|s| s.to_str()).is_some_and(|n| n.len() > MESSAGES_SUFFIX.len() && n.ends_with(MESSAGES_SUFFIX))
}

/// `<sessions>/<dir>/<ts>_<id>.messages.json` → the sibling `<ts>_<id>.json`.
pub fn session_meta_path(messages: &Path) -> Option<PathBuf> {
    let file = messages.file_name()?.to_str()?;
    Some(messages.with_file_name(format!("{}.json", file.strip_suffix(MESSAGES_SUFFIX)?)))
}

/// Session id: the transcript's own `sessionId` field is authoritative; the
/// directory/file stem is what Cline writes there anyway, so it is a safe
/// fallback and keeps `read` working on a fixture with no payload header.
pub fn session_id(messages: &Path, payload_session: Option<&str>) -> String {
    if let Some(s) = payload_session.filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    if let Some(stem) = messages.file_name().and_then(|s| s.to_str()).and_then(|n| n.strip_suffix(MESSAGES_SUFFIX)) {
        if !stem.is_empty() {
            return stem.to_string();
        }
    }
    messages.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or("cline-session").to_string()
}

/// Project label from the *sibling session meta* (`cwd`, else `workspace_root`).
///
/// Its `metadata.usage` / `metadata.aggregateUsage` accumulators are never
/// parsed: they are two copies of one cumulative total that already equals the
/// per-message sums, so emitting events from them would double every session.
/// Only the label is taken, and no event ever comes from that file.
pub fn project_label(messages: &Path) -> Option<String> {
    let meta_path = session_meta_path(messages)?;
    let text = std::fs::read_to_string(meta_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let cwd = ["cwd", "workspace_root", "workspaceRoot"]
        .iter()
        .filter_map(|k| value.get(*k).and_then(serde_json::Value::as_str))
        .find(|s| !s.is_empty())?;
    Path::new(cwd).file_name().and_then(|n| n.to_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_and_meta_pair_by_name() {
        let p = Path::new("/s/1787490736874_djat1/1787490736874_djat1.messages.json");
        assert_eq!(session_meta_path(p), Some(PathBuf::from("/s/1787490736874_djat1/1787490736874_djat1.json")));
        assert_eq!(session_id(p, Some("payload_9")), "payload_9", "the payload wins");
        assert_eq!(session_id(p, None), "1787490736874_djat1");
        assert_eq!(session_id(Path::new("/x/weird.name.messages.json"), None), "weird.name");
        assert_eq!(session_id(Path::new("/sessions/1_ts_a/notes.txt"), None), "1_ts_a", "fall back to the directory Cline names after the id");
        assert!(!is_messages_file(Path::new("/s/x/x.json")), "the meta file is not a source");
        assert!(!is_messages_file(Path::new("/s/x/.messages.json")), "a bare suffix is not a source");
    }

    #[test]
    fn label_comes_from_cwd_and_only_from_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("1_ts");
        std::fs::create_dir_all(&sess).unwrap();
        let msgs = sess.join("1_ts.messages.json");
        std::fs::write(&msgs, b"{\"messages\":[]}").unwrap();
        assert_eq!(project_label(&msgs), None, "no sibling meta yet");
        std::fs::write(sess.join("1_ts.json"), br#"{"workspace_root":"/a/b/apppty"}"#).unwrap();
        assert_eq!(project_label(&msgs).as_deref(), Some("apppty"));
        std::fs::write(sess.join("1_ts.json"), br#"{"cwd":"/Users/dev/workspace/apppty","workspace_root":"/other"}"#).unwrap();
        assert_eq!(project_label(&msgs).as_deref(), Some("apppty"), "cwd takes priority");
        std::fs::write(sess.join("1_ts.json"), br#"{"cwd":"","metadata":{"usage":{"inputTokens":1}}}"#).unwrap();
        assert_eq!(project_label(&msgs), None, "empty cwd is no label; usage stays unread");
        std::fs::write(sess.join("1_ts.json"), b"{oops").unwrap();
        assert_eq!(project_label(&msgs), None);
    }
}

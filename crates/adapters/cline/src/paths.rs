//! Where Cline keeps its transcripts, and how to walk back from a transcript to
//! the session meta that names its workspace.
//!
//! Layout measured here (`~/.cline/data/sessions`, 40 dirs on 2026-10-06): the
//! CLI writes `<ts>_<id>_cli/`, the older desktop build `<ts>_<id>/`, the current
//! one `session_<ts>_<id>/`; each holds `<name>.json` (session meta) and
//! `<name>.messages.json` (the transcript). A desktop sub-agent's transcript is
//! written *into its parent's* dir as `agent_<uuid>.messages.json`, while its own
//! meta goes to a sibling `session_…__agent_<uuid>/` dir — and of this machine's
//! ten agents only four have one; the other six are labelled by their session's
//! meta, since one session runs in one workspace. Many dirs hold no transcript at
//! all, so the pairing must be tolerant in both directions.

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

/// Meta files that can name this transcript's workspace, nearest truth first.
///
/// The CLI and the older desktop build write `<stem>.json` beside the transcript.
/// The current desktop build writes a sub-agent's meta into a sibling
/// `<session>__agent_<uuid>/` dir instead, and when even that is missing the
/// agent still ran in its session's workspace, so the containing session's own
/// meta labels it. Measured here: 10 agent transcripts, 4 with their own meta,
/// 6 with only the session's.
pub fn session_meta_candidates(messages: &Path) -> Vec<PathBuf> {
    let Some(sibling) = session_meta_path(messages) else {
        return Vec::new();
    };
    let stem = messages
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(MESSAGES_SUFFIX));
    let (Some(stem), Some(dir)) = (stem, messages.parent()) else {
        return vec![sibling];
    };
    let Some(dir_name) = dir.file_name().and_then(|n| n.to_str()) else {
        return vec![sibling];
    };
    let mut out = vec![sibling];
    if dir_name != stem {
        // Only a sub-agent's transcript is named differently from its directory,
        // and only for one does the desktop layout offer a second and third home.
        if let Some(root) = dir.parent() {
            let name = format!("{dir_name}__{stem}");
            out.push(root.join(&name).join(format!("{name}.json")));
        }
        // One session runs in one workspace, so the orchestrator's meta labels it.
        out.push(dir.join(format!("{dir_name}.json")));
    }
    out
}

/// Project label from the session meta (`cwd`, else `workspace_root`).
///
/// The meta's `metadata.usage` / `metadata.aggregateUsage` accumulators are never
/// parsed: they are cumulative totals that already equal the per-message sums —
/// `usage` for the orchestrator transcript, `aggregateUsage` for that transcript
/// plus every sub-agent's — so emitting events from them would double each
/// session. Only the label is taken, and no event ever comes from a meta file.
pub fn project_label(messages: &Path) -> Option<String> {
    for meta in session_meta_candidates(messages) {
        let Ok(text) = std::fs::read_to_string(meta) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let Some(cwd) = ["cwd", "workspace_root", "workspaceRoot"]
            .iter()
            .filter_map(|k| value.get(*k).and_then(serde_json::Value::as_str))
            .find(|s| !s.is_empty())
        else {
            continue;
        };
        if let Some(label) = Path::new(cwd).file_name().and_then(|n| n.to_str()) {
            return Some(label.to_string());
        }
    }
    None
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

    #[test]
    fn a_desktop_sub_agent_is_labelled_by_its_own_meta_then_the_session_s() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sess = root.join("session_1_ts");
        std::fs::create_dir_all(&sess).unwrap();
        let msgs = sess.join("agent_aaa.messages.json");
        std::fs::write(&msgs, b"{\"messages\":[]}").unwrap();
        assert_eq!(project_label(&msgs), None, "no meta anywhere yet");

        // Six of this machine's ten agents have only the session's own meta.
        std::fs::write(sess.join("session_1_ts.json"), br#"{"cwd":"/w/AutoRSA"}"#).unwrap();
        assert_eq!(project_label(&msgs).as_deref(), Some("AutoRSA"), "one session, one workspace");

        // Four carry a desktop agent meta of their own, and that is nearer truth.
        let agent = root.join("session_1_ts__agent_aaa");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("session_1_ts__agent_aaa.json"), br#"{"workspace_root":"/w/elsewhere"}"#).unwrap();
        assert_eq!(project_label(&msgs).as_deref(), Some("elsewhere"), "the agent's own cwd beats the session's");
        std::fs::write(agent.join("session_1_ts__agent_aaa.json"), br#"{"cwd":""}"#).unwrap();
        assert_eq!(project_label(&msgs).as_deref(), Some("AutoRSA"), "an empty cwd is no label, so the chain moves on");

        // The orchestrator's transcript never grows candidates of its own.
        let parent = sess.join("session_1_ts.messages.json");
        std::fs::write(&parent, b"{\"messages\":[]}").unwrap();
        assert_eq!(session_meta_candidates(&parent), vec![sess.join("session_1_ts.json")]);
        assert_eq!(
            session_meta_candidates(&msgs),
            vec![
                sess.join("agent_aaa.json"),
                root.join("session_1_ts__agent_aaa").join("session_1_ts__agent_aaa.json"),
                sess.join("session_1_ts.json"),
            ]
        );
    }
}

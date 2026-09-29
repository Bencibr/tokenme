//! Where AtomCode keeps its session transcripts on this machine.
//!
//! Layout, straight from the writer: `$ATOMCODE_HOME/sessions/<project_hash>/
//! <session-id>.jsonl`, defaulting to `~/.atomcode/sessions/…`.
//!
//! * `ATOMCODE_HOME` is AtomCode's own variable, `pub const HOME_ENV: &str =
//!   "ATOMCODE_HOME"` in `crates/atomcode-config/src/distribution.rs:50` (MIT,
//!   `atomgit_atomcode/atomcode` @ e4215f7), and it names the *config/data dir*:
//!   `SessionManager::sessions_root()` is `config_dir().join("sessions")`
//!   (`crates/atomcode-capabilities/src/session/manager.rs:947-949`), where
//!   `config_dir()` is "`$ATOMCODE_HOME` if set & non-empty, else `~/.atomcode`"
//!   (`crates/atomcode-capabilities/src/paths.rs:17-26`). An empty value counts
//!   as unset there, so it does here too.
//! * `<project_hash>` is a one-way `stable_project_hash(working_dir)`
//!   (`manager.rs:969-1006`), so the directory names no workspace: 9 buckets
//!   here hold 9 distinct `working_dir`s and none of them decodes from the hash.
//!   The label therefore comes from the sibling `<id>.meta`'s `working_dir`.
//! * Siblings of the transcript are deliberately not ingested: `<id>.snapshot`
//!   is the rewritten whole live state (its per-message `meta.tokens` restate
//!   the same calls), `<id>.meta` is rewritten on every turn, and `<id>.ui.json`
//!   / `<id>.lease` / `<id>.meta.lock` carry no usage at all.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// AtomCode's data-dir override; it points one level above `sessions/`.
pub(crate) const HOME_ENV: &str = "ATOMCODE_HOME";
pub(crate) const SESSIONS_SUBDIR: &str = "sessions";
pub(crate) const CONFIG_DIR_NAME: &str = ".atomcode";

/// `$ATOMCODE_HOME/sessions`, else `~/.atomcode/sessions`. Split out from
/// [`sessions_dir`] so tests can drive both branches without racing the
/// process-global environment (AtomCode itself notes the same hazard at
/// `crates/atomcode-capabilities/src/paths.rs:28-34`).
pub(crate) fn sessions_dir() -> Option<PathBuf> {
    sessions_dir_from(std::env::var_os(HOME_ENV).as_deref(), dirs::home_dir().as_deref())
}

pub(crate) fn sessions_dir_from(home: Option<&OsStr>, user_home: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = home.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join(SESSIONS_SUBDIR));
    }
    user_home.map(|h| h.join(CONFIG_DIR_NAME).join(SESSIONS_SUBDIR))
}

pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let since = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())?;
    Some(since.as_millis() as i64)
}

/// The file stem is the session uuid (`<id>.jsonl`), which keeps a transcript
/// labelled with the right session even if a record omits `session_id`
/// (absent from 0 of the 64 records on this machine).
pub(crate) fn session_from_path(path: &Path) -> String {
    path.file_stem().and_then(OsStr::to_str).unwrap_or_default().to_string()
}

/// `<id>.meta` beside `<id>.jsonl` — AtomCode's `path_for(id, "meta")`.
pub(crate) fn meta_path(jsonl: &Path) -> PathBuf {
    jsonl.with_extension("meta")
}

/// The workspace label, from the session meta's `working_dir`. `SessionMeta` is
/// one pretty-printed JSON object rewritten in place, and `working_dir` is the
/// only place the real path survives: the bucket directory is hashed.
pub(crate) fn working_dir(jsonl: &Path) -> Option<String> {
    let text = std::fs::read_to_string(meta_path(jsonl)).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&text).ok()?;
    let dir = meta.get("working_dir").and_then(serde_json::Value::as_str)?;
    (!dir.is_empty()).then(|| dir.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_precedence_picks_the_sessions_root() {
        let home = Path::new("/home/x");
        assert_eq!(
            sessions_dir_from(Some(OsStr::new("/tmp/ac")), Some(home)),
            Some(PathBuf::from("/tmp/ac/sessions"))
        );
        // An empty override is "unset", exactly as `paths::config_dir` treats it.
        assert_eq!(
            sessions_dir_from(Some(OsStr::new("")), Some(home)),
            Some(PathBuf::from("/home/x/.atomcode/sessions"))
        );
        assert_eq!(
            sessions_dir_from(None, Some(home)),
            Some(PathBuf::from("/home/x/.atomcode/sessions"))
        );
        assert_eq!(sessions_dir_from(None, None), None);
    }

    #[test]
    fn siblings_are_addressed_by_extension() {
        let jsonl = Path::new("/h/.atomcode/sessions/45d727130d2f41d9/242f9f29-b7.jsonl");
        assert_eq!(
            meta_path(jsonl),
            PathBuf::from("/h/.atomcode/sessions/45d727130d2f41d9/242f9f29-b7.meta")
        );
        assert_eq!(session_from_path(jsonl), "242f9f29-b7");
        // No meta beside it (a legacy `<id>.json` session, or the file was
        // deleted): no label, rather than a guessed one.
        assert_eq!(working_dir(jsonl), None);
    }
}

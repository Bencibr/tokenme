//! Where ZCode keeps its data, and which of the trees is authoritative.
//!
//! Measured on this machine (2026-09-23), all under `~/.zcode/cli/`:
//!
//! | tree | holds | volume |
//! |---|---|---|
//! | `db/db.sqlite` → table `model_usage` | one row per billed model call | 18,112 rows / 151 sessions |
//! | `rollout/model-io-<session>.jsonl` | the full HTTP request/response of each call, per session | 151 records / **3** sessions |
//! | `agents/<session>/…/transcript.jsonl` | agent event stream, `model_complete` carries a usage snapshot | 2,467 records |
//! | `artifacts/sess_*/call_*.json` | file diffs of one tool call (`{kind, toolName, files:[{beforeContent…}]}`) | 2,484 files, **no token fields at all** |
//! | `log/zcode-<date>.jsonl` | trace/debug stream | 54 MB, no per-call usage |
//!
//! ## Authoritative source: the `model_usage` table
//! Every rollout record joins into it (151 of 151 matched on
//! `(session_id, trace_id, completed_at)`) and the numbers are the same call
//! normalised: provider `input_tokens 702 + cache_read_input_tokens 381_888` =
//! the row's `input_tokens 382_590`. The rollout files are a rotating buffer
//! (`modelIOReset.maxFileBytes = 67_108_864`) and only 3 of the 151 sessions'
//! files survive, so reading them alone would hide 99% of history while reading
//! both would double-bill the overlap. `discover` therefore returns the db when
//! it exists and the rollout files only as a fallback (see `loader`).
//!
//! The one thing only the rollout keeps is per-attempt granularity: `attempt_index`
//! is 0 in all 18,112 rows (`retry_count > 0` in 275), i.e. the db collapses a
//! retried request into one row while the rollout logs each attempt separately.
//! Completeness of 151 sessions beats granularity on those retries; and when the
//! db is missing the fallback still keys on `<session>#<requestId>#<attempt>` so
//! two attempts never merge.

use std::path::{Path, PathBuf};

/// Read by ZCode itself (`resolveZcodeHome` in the shipped app:
/// `env.ZCODE_HOME?.trim() || join(env.HOME, ".zcode")`).
pub const ENV_ZCODE_HOME: &str = "ZCODE_HOME";

/// `model_usage` is append-only with an integer rowid, which is the cursor.
pub const USAGE_TABLE: &str = "model_usage";

/// Prefix of every rollout file, for both layouts (main session and subagent).
pub const ROLLOUT_PREFIX: &str = "model-io-";

pub const ROLLOUT_SUFFIX: &str = ".jsonl";

/// `~/.zcode`, honouring `ZCODE_HOME`.
pub fn zcode_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_ZCODE_HOME) {
        let trimmed = dir.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    #[cfg(target_os = "windows")]
    let home = std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir);
    #[cfg(not(target_os = "windows"))]
    let home = dirs::home_dir();
    home.map(|h| h.join(".zcode"))
}

pub fn cli_dir() -> Option<PathBuf> {
    Some(zcode_home()?.join("cli"))
}

/// `~/.zcode/cli/db/db.sqlite` — the complete per-call record.
pub fn db_path() -> Option<PathBuf> {
    let p = cli_dir()?.join("db").join("db.sqlite");
    p.is_file().then_some(p)
}

/// `~/.zcode/cli/rollout` — the rotating per-session request/response buffer.
pub fn rollout_dir() -> Option<PathBuf> {
    let p = cli_dir()?.join("rollout");
    p.is_dir().then_some(p)
}

/// Name-only half of [`is_rollout_file`]: `model-io-sess_<uuid>.jsonl` and
/// `model-io-sess_subagent_agent_<uuid>.jsonl`, which are the two layouts.
pub fn is_rollout_file_name(name: &str) -> bool {
    let Some(stem) = name.strip_prefix(ROLLOUT_PREFIX).and_then(|n| n.strip_suffix(ROLLOUT_SUFFIX)) else {
        return false;
    };
    // `model-io-sess_` is what ZCode writes; anything shorter is not a session.
    stem.starts_with("sess_") && stem.len() > 5
}

pub fn is_rollout_file(path: &Path) -> bool {
    path.is_file() && path.file_name().and_then(|s| s.to_str()).is_some_and(is_rollout_file_name)
}

/// `model-io-sess_X.jsonl` / `model-io-sess_subagent_agent_X.jsonl` → `sess_X…`.
/// The record's own `sessionId` wins; this is the fallback for a trimmed line.
pub fn session_from_file_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix(ROLLOUT_PREFIX)?.strip_suffix(ROLLOUT_SUFFIX)?;
    (!stem.is_empty()).then_some(stem.to_string())
}

/// Env vars are process-global, so every test that points the adapter at a temp
/// home holds this lock (shared across the crate's test modules).
#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_rollout_layouts_are_recognised_by_name() {
        assert!(is_rollout_file_name("model-io-sess_7de7ea43-96fe-4efe-9bdb-4afd85127203.jsonl"));
        assert!(is_rollout_file_name("model-io-sess_subagent_agent_bd60e736-e6f4-495f-bb9d-6458cdc9db73.jsonl"));
        assert!(!is_rollout_file_name("model-io-.jsonl"));
        assert!(!is_rollout_file_name("model-io-sess_.jsonl"), "an empty session id is not a file we created");
        assert!(!is_rollout_file_name("rollout-real.jsonl"));
        assert!(!is_rollout_file_name("model-io-sess_1.log"));
        assert!(!is_rollout_file_name("model-io-index.jsonl"), "not a session file");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("model-io-sess_a.jsonl");
        std::fs::write(&file, b"{}\n").unwrap();
        assert!(is_rollout_file(&file));
        assert!(!is_rollout_file(dir.path()), "the directory itself is not a file");
        assert!(!is_rollout_file(&dir.path().join("model-io-sess_missing.jsonl")), "not on disk");
    }

    #[test]
    fn session_id_is_read_out_of_the_file_name() {
        let p = Path::new("/r/model-io-sess_7de7ea43-96fe-4efe-9bdb-4afd85127203.jsonl");
        assert_eq!(session_from_file_name(p).as_deref(), Some("sess_7de7ea43-96fe-4efe-9bdb-4afd85127203"));
        let p = Path::new("/r/model-io-sess_subagent_agent_bd60e736.jsonl");
        assert_eq!(session_from_file_name(p).as_deref(), Some("sess_subagent_agent_bd60e736"));
        assert_eq!(session_from_file_name(Path::new("/r/other.jsonl")), None);
    }

    #[test]
    fn zcode_home_override() {
        let _env = lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_ZCODE_HOME, dir.path());
        assert_eq!(zcode_home().as_deref(), Some(dir.path()));
        assert_eq!(cli_dir().as_deref(), Some(dir.path().join("cli").as_path()));
        assert!(db_path().is_none(), "no db under the temp home");
        assert!(rollout_dir().is_none());
        std::fs::create_dir_all(dir.path().join("cli").join("db")).unwrap();
        std::fs::write(dir.path().join("cli").join("db").join("db.sqlite"), b"SQLite format 3").unwrap();
        assert_eq!(db_path().as_deref(), Some(dir.path().join("cli").join("db").join("db.sqlite").as_path()));
        std::env::remove_var(ENV_ZCODE_HOME);
        assert_eq!(zcode_home().as_deref(), dirs::home_dir().map(|h| h.join(".zcode")).as_deref());
    }
}

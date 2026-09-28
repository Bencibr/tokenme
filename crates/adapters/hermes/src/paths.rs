//! Where Hermes keeps its state database.
//!
//! `HERMES_HOME` wins when set — it is the tool's own documented override and
//! the installer honours it too (this machine installs to `$env:USERPROFILE\hermes` through
//! it). The platform default mirrors `hermes_constants._get_platform_default_hermes_home`:
//! `%LOCALAPPDATA%\hermes` on Windows, `~/.hermes` elsewhere.

use std::path::PathBuf;

/// Serialises every test that touches `HERMES_HOME`: the var is process-global
/// and this machine really has it set, so a parallel test would read a
/// neighbour's fixture home.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `<HERMES_HOME>/state.db`, when it exists.
pub fn db_path() -> Option<PathBuf> {
    let path = hermes_home().join("state.db");
    path.is_file().then_some(path)
}

pub fn hermes_home() -> PathBuf {
    if let Some(home) = std::env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        return home;
    }
    // Same data-directory suffix hook the tool itself reads.
    let suffix = std::env::var("HERMES_DATA_DIR_SUFFIX").unwrap_or_default();
    if cfg!(target_os = "windows") {
        let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
        base.join(format!("hermes{suffix}"))
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(format!(".hermes{suffix}"))
    }
}

pub fn stat_file(path: &std::path::Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let mtime_ms = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((meta.len(), mtime_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_override_wins_over_the_platform_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HERMES_HOME", dir.path());
        assert_eq!(hermes_home(), dir.path().to_path_buf());
        std::env::remove_var("HERMES_HOME");
    }
}

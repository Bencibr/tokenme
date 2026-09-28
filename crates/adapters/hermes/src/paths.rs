//! Where Hermes keeps its state database.
//!
//! `HERMES_HOME` wins when set — it is the tool's own documented override and
//! the installer honours it too (this machine installs to `$env:USERPROFILE\hermes` through
//! it). The tool resolves the value through `os.path.expanduser`, so a literal
//! `~/…` entry lands on the real home directory; the leading-tilde case is
//! mirrored here. The platform default mirrors
//! `hermes_constants._get_platform_default_hermes_home`:
//! `%LOCALAPPDATA%\hermes` on Windows, `~/.hermes` elsewhere (macOS included —
//! verified against hermes-agent's own sources, not assumed from the
//! Windows-side install).

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
        return expand_tilde(home);
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

/// `os.path.expanduser` for the one form that matters in env files: `~` and
/// `~/…`. The `~user` form needs a passwd lookup and stays unsupported; on
/// Windows a leading tilde is not a shell convention and passes through.
fn expand_tilde(path: PathBuf) -> PathBuf {
    let Some(home) = dirs::home_dir() else { return path };
    match path.to_str() {
        Some("~") => home,
        Some(text) if text.starts_with("~/") => home.join(&text[2..]),
        _ => path,
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

    /// The tool resolves HERMES_HOME through os.path.expanduser, so the literal
    /// tilde form an env file likes to carry must land on the real home.
    #[test]
    fn a_tilde_override_expands_to_the_home_directory() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = dirs::home_dir().unwrap();
        std::env::set_var("HERMES_HOME", "~");
        assert_eq!(hermes_home(), home.clone());
        std::env::set_var("HERMES_HOME", "~/.hermes-work");
        assert_eq!(hermes_home(), home.join(".hermes-work"));
        // `~user` needs a passwd lookup and passes through untouched.
        std::env::set_var("HERMES_HOME", "~sp/.hermes");
        assert_eq!(hermes_home(), PathBuf::from("~sp/.hermes"));
        std::env::remove_var("HERMES_HOME");
    }
}

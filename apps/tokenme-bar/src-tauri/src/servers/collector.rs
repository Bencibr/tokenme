//! Locating and identifying the collector binaries the panel ships.
//!
//! `scripts/bundle-collector.sh` copies the static musl builds into
//! `resources/collector/` where Tauri bundles them; the wizard uploads the one
//! matching the server's `uname -m`. In a dev tree (`cargo tauri dev`) the
//! resource lookup can miss, so the crate-relative path is the fallback.

use std::path::PathBuf;

use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager, Runtime};

use super::SshError;

/// Server architecture → the suffix of our bundled binary, `None` for
/// anything we do not ship a collector for (armv7l and friends) — that is a
/// hard stop with a pointer at `install-linux.sh --push`.
pub fn arch_target(uname_m: &str) -> Option<&'static str> {
    match uname_m.trim() {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    }
}

pub fn file_name_for(arch: &str) -> String {
    format!("tokenme-{arch}")
}

/// Resolves the bundled collector for `arch` and returns its bytes plus their
/// sha256 (the same digest `sha256sum` prints on the server).
pub fn load<R: Runtime>(app: &AppHandle<R>, arch: &str) -> Result<(String, Vec<u8>, String), SshError> {
    let fname = file_name_for(arch);
    let path = resolve(app, &fname)?;
    let bytes = std::fs::read(&path).map_err(|e| {
        SshError::new("collector_missing", format!("cannot read {}: {e}", path.display()))
    })?;
    if bytes.is_empty() {
        return Err(SshError::new(
            "collector_missing",
            format!("{} is empty — run scripts/bundle-collector.sh", path.display()),
        ));
    }
    let sha = sha_of(&bytes);
    Ok((fname, bytes, sha))
}

fn resolve<R: Runtime>(app: &AppHandle<R>, fname: &str) -> Result<PathBuf, SshError> {
    let rel = format!("collector/{fname}");
    if let Ok(path) = app.path().resolve(&rel, BaseDirectory::Resource) {
        if path.exists() {
            return Ok(path);
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join("collector").join(fname);
    if dev.exists() {
        return Ok(dev);
    }
    Err(SshError::new(
        "collector_missing",
        format!("bundled collector {fname} not found — run scripts/bundle-collector.sh"),
    ))
}

pub fn sha_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_mapping() {
        assert_eq!(arch_target("x86_64"), Some("x86_64"));
        assert_eq!(arch_target("x86_64\n"), Some("x86_64"));
        assert_eq!(arch_target("aarch64"), Some("aarch64"));
        assert_eq!(arch_target("arm64"), Some("aarch64"));
        assert_eq!(arch_target("armv7l"), None);
        assert_eq!(arch_target(""), None);
        assert_eq!(file_name_for("x86_64"), "tokenme-x86_64");
    }

    #[test]
    fn sha_matches_expected_vector() {
        // sha256 of the empty string, a value every shell also knows.
        assert_eq!(sha_of(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha_of(b"tokenme").len(), 64);
    }
}

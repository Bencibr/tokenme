//! The one dedicated key pair server pulls authenticate with.
//!
//! Generated on first install under the panel's config dir (`tokenme_ed25519`,
//! mode 0600), its public line is appended to the server's
//! `~/.ssh/authorized_keys` exactly once, and every later pull — plus a later
//! "clean up the server" removal — authenticates with it. The password a user
//! typed during the wizard never reaches this file.

use std::io::Write;
use std::path::{Path, PathBuf};

use russh::keys::{ssh_key, Algorithm, PrivateKey};

use super::panel_dir;
use super::SshError;

pub const KEY_FILE: &str = "tokenme_ed25519";

#[derive(Debug, Clone)]
pub struct KeyInfo {
    pub path: PathBuf,
    /// The base64 part of the public key (`ssh-ed25519 <blob> <comment>`).
    pub blob: String,
    pub comment: String,
}

impl KeyInfo {
    pub fn line(&self) -> String {
        format!("ssh-ed25519 {} {}", self.blob, self.comment)
    }
}

pub fn dedicated_key_path() -> Result<PathBuf, SshError> {
    panel_dir()
        .map(|d| d.join(KEY_FILE))
        .ok_or_else(|| SshError::new("local_io", "no config directory for the dedicated key"))
}

/// Loads the dedicated key pair, generating it on first use.
pub fn ensure_keypair() -> Result<KeyInfo, SshError> {
    let dir = panel_dir()
        .ok_or_else(|| SshError::new("local_io", "no config directory for the dedicated key"))?;
    ensure_keypair_at(&dir)
}

pub fn ensure_keypair_at(dir: &Path) -> Result<KeyInfo, SshError> {
    let path = dir.join(KEY_FILE);
    let key = if path.exists() {
        russh::keys::load_secret_key(&path, None).map_err(|e| {
            SshError::new(
                "key_missing",
                format!("cannot read the dedicated key {}: {e} (delete it to regenerate)", path.display()),
            )
        })?
    } else {
        std::fs::create_dir_all(dir)
            .map_err(|e| SshError::new("local_io", format!("cannot create {}: {e}", dir.display())))?;
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .map_err(|e| SshError::new("local_io", format!("cannot generate a key: {e}")))?;
        let openssh = key
            .to_openssh(ssh_key::LineEnding::LF)
            .map_err(|e| SshError::new("local_io", format!("cannot encode the key: {e}")))?;
        let tmp = path.with_extension("tmp");
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts
                .open(&tmp)
                .map_err(|e| SshError::new("local_io", format!("cannot write {}: {e}", tmp.display())))?;
            f.write_all(openssh.as_bytes())
                .and_then(|_| f.flush())
                .map_err(|e| SshError::new("local_io", format!("cannot write {}: {e}", tmp.display())))?;
        }
        usage_core::replace_file(&tmp, &path)
            .map_err(|e| SshError::new("local_io", format!("cannot store {}: {e}", path.display())))?;
        key
    };

    let line = key
        .public_key()
        .to_openssh()
        .map_err(|e| SshError::new("local_io", format!("cannot encode the public key: {e}")))?;
    let mut parts = line.split_whitespace();
    let _algo = parts.next();
    let blob = parts
        .next()
        .ok_or_else(|| SshError::new("local_io", "public key line has no blob"))?
        .to_string();
    Ok(KeyInfo { path, blob, comment: comment_for(&local_hostname()) })
}

/// `tokenme@host`, comment-only, filtered to characters that are safe inside a
/// shell single-quoted string (the comment is interpolated into the append
/// command; the same validator runs again in `remote::cmd_pubkey_append`).
fn comment_for(host: &str) -> String {
    let mut clean = String::with_capacity(host.len());
    for c in host.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            clean.push(c);
        } else {
            clean.push('-');
        }
    }
    if clean.is_empty() {
        clean.push_str("unknown");
    }
    format!("tokenme@{clean}")
}

fn local_hostname() -> String {
    usage_index::hostname()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn keypair_is_generated_then_reused() {
        let dir = std::env::temp_dir().join(format!("tokenme-key-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = ensure_keypair_at(&dir).unwrap();
        assert!(first.path.exists());
        let mode = std::fs::metadata(&first.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "private key must be 0600");
        assert!(first.path.extension().is_none() || first.path.file_name().unwrap() == KEY_FILE);
        // The `.tmp` staging file must not survive.
        assert!(!dir.join("tokenme_ed25519.tmp").exists());

        let second = ensure_keypair_at(&dir).unwrap();
        assert_eq!(first.blob, second.blob, "second call must reuse the same key");
        assert_eq!(second.line(), format!("ssh-ed25519 {} {}", first.blob, first.comment));
        // The stored file is a private key, not a public one.
        let raw = std::fs::read_to_string(&second.path).unwrap();
        assert!(raw.contains("PRIVATE KEY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn comment_is_shell_safe() {
        let c = comment_for("weird host;$(rm -rf /)");
        assert!(c.starts_with("tokenme@"));
        assert!(!c.contains('\''), "comment must not be able to close the shell quote");
        assert!(c.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '@')));
        assert_eq!(comment_for(""), "tokenme@unknown");
    }
}

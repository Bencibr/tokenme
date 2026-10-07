//! The fixed remote command surface and its validators.
//!
//! Everything the panel runs on a server is built here. The command strings
//! are constants; interpolation is limited to the server name (the export
//! origin) and the window in days, and both booleans are validated first —
//! the server name against the frozen `^[A-Za-z0-9._-]{1,64}$` rule *and*
//! `usage_core::origin_ok` (the same gate the sync import enforces), the days
//! against `1..=3650`. A blob or comment interpolated into a shell line is
//! re-validated against a whitelist so even a corrupted key file cannot close
//! a quote.

use super::SshError;

/// `1..=64` chars of `[A-Za-z0-9._-]`, and safe as a sync `origin`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && usage_core::origin_ok(name)
}

/// A hostname, IPv4 or (bracketed or bare) IPv6 literal. Never interpolated
/// into a shell command — the transport resolves it — but still held to a
/// conservative shape so garbage fails in the wizard, not in DNS.
pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']' | '_'))
}

/// A conservative sshd username; also never shell-interpolated.
pub fn valid_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 64
        && !user.starts_with('-')
        && user.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn valid_days(days: i64) -> bool {
    (1..=3650).contains(&days)
}

/// 1 minute to 24 hours.
pub fn valid_every(secs: u64) -> bool {
    (60..=86_400).contains(&secs)
}

fn valid_blob(blob: &str) -> bool {
    !blob.is_empty() && blob.len() <= 1024 && blob.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

fn valid_comment(comment: &str) -> bool {
    comment.len() <= 128
        && comment.starts_with("tokenme@")
        && comment.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

// ---- fixed commands ----------------------------------------------------------

pub const CMD_UNAME: &str = "uname -m";

pub const CMD_HOSTNAME: &str = "hostname 2>/dev/null || cat /proc/sys/kernel/hostname";

pub const CMD_MKDIR: &str =
    "mkdir -p ~/.tokenme/bin ~/.tokenme/sync && chmod 700 ~/.tokenme && chmod 755 ~/.tokenme/bin && chmod 700 ~/.tokenme/sync";

pub const CMD_SHA_BIN: &str = "sha256sum ~/.tokenme/bin/tokenme 2>/dev/null | cut -d' ' -f1";

pub const CMD_SHA_TMP: &str = "sha256sum ~/.tokenme/bin/tokenme.tmp 2>/dev/null | cut -d' ' -f1";

pub const CMD_INSTALL_BIN: &str = "chmod 755 ~/.tokenme/bin/tokenme.tmp && mv -f ~/.tokenme/bin/tokenme.tmp ~/.tokenme/bin/tokenme && ~/.tokenme/bin/tokenme --version";

pub const CMD_DETECT: &str = "~/.tokenme/bin/tokenme detect --json --quiet";

pub const CMD_CLEANUP_BIN: &str = "rm -f ~/.tokenme/bin/tokenme ~/.tokenme/bin/tokenme.tmp && exit 0";

/// `tokenme export` with the two validated values. `--origin NAME` makes the
/// bundle name `tokenme-NAME.jsonl.gz` and stamps `origin: NAME` inside it.
pub fn cmd_export(name: &str, days: i64) -> Result<String, SshError> {
    if !valid_name(name) {
        return Err(SshError::new("proto", format!("invalid server name {name:?}")));
    }
    if !valid_days(days) {
        return Err(SshError::new("proto", format!("invalid export window {days}")));
    }
    Ok(format!("~/.tokenme/bin/tokenme export --days {days} --out ~/.tokenme/sync --origin {name}"))
}

/// Appends our public key line to `~/.ssh/authorized_keys`, idempotently.
/// `grep -q -F` means an already-installed key exits 0 without a second copy;
/// every failure path funnels to exit 3 so the caller cannot confuse "already
/// there" with "could not write".
pub fn cmd_pubkey_append(blob: &str, comment: &str) -> Result<String, SshError> {
    if !valid_blob(blob) || !valid_comment(comment) {
        return Err(SshError::new("proto", "refusing to build a pubkey append command from an invalid key"));
    }
    Ok(format!(
        "mkdir -p ~/.ssh && chmod 700 ~/.ssh; \
f=~/.ssh/authorized_keys; touch \"$f\" && chmod 600 \"$f\"; \
grep -q -F '{blob}' \"$f\" && exit 0; \
printf '%s\\n' 'ssh-ed25519 {blob} {comment}' >> \"$f\" && exit 0; exit 3"
    ))
}

/// Removes our key line from `~/.ssh/authorized_keys`. The rewrite only lands
/// when `grep -v` exited 0 (removed some, kept some) or 1 (file contained only
/// our line — an empty result is the honest one); a read error (rc ≥ 2) leaves
/// the original file untouched.
pub fn cmd_pubkey_remove(blob: &str) -> Result<String, SshError> {
    if !valid_blob(blob) {
        return Err(SshError::new("proto", "refusing to build a pubkey removal command from an invalid key"));
    }
    Ok(format!(
        "f=~/.ssh/authorized_keys; [ -f \"$f\" ] || exit 0; \
grep -v -F '{blob}' \"$f\" > \"$f.tokenme.tmp\"; rc=$?; \
if [ $rc -le 1 ]; then mv -f \"$f.tokenme.tmp\" \"$f\" && chmod 600 \"$f\" && exit 0; fi; \
rm -f \"$f.tokenme.tmp\"; exit 2"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation_matches_the_frozen_regex() {
        assert!(valid_name("box"));
        assert!(valid_name("linux-01.dev_2"));
        assert!(valid_name(&"a".repeat(64)));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(65)));
        for bad in ["a b", "a'b", "a;rm -rf /", "a$(x)", "a/B", "a\\B", "a:b", "a\nb"] {
            assert!(!valid_name(bad), "{bad:?} must be rejected");
        }
        assert!(valid_name("-leading"), "hyphen must stay allowed");
    }

    #[test]
    fn host_and_user_validation() {
        assert!(valid_host("example.com"));
        assert!(valid_host("192.0.2.4"));
        assert!(valid_host("fe80::1"));
        assert!(valid_host("[fe80::1]"));
        assert!(!valid_host(""));
        assert!(!valid_host("a b"));
        assert!(!valid_host("a;b"));
        assert!(!valid_host("a'b"));
        assert!(!valid_host("evil\nhost"));

        assert!(valid_user("root"));
        assert!(valid_user("deploy.user-1"));
        assert!(!valid_user(""));
        assert!(!valid_user("-x"));
        assert!(!valid_user("a b"));
        assert!(!valid_user("a'b"));
    }

    #[test]
    fn export_command_is_pinned() {
        assert_eq!(
            cmd_export("box", 30).unwrap(),
            "~/.tokenme/bin/tokenme export --days 30 --out ~/.tokenme/sync --origin box"
        );
        assert!(cmd_export("bad name", 30).is_err());
        assert!(cmd_export("box", 0).is_err());
        assert!(cmd_export("box", 10_000).is_err());
    }

    #[test]
    fn pubkey_append_is_idempotent_and_injection_safe() {
        let blob = "AAAAC3NzaC1lZDI1NTE5AAAAIExample";
        let cmd = cmd_pubkey_append(blob, "tokenme@host").unwrap();
        assert!(cmd.contains("grep -q -F"));
        assert!(cmd.contains("exit 0"));
        assert!(cmd.contains("exit 3"));
        assert!(cmd.contains(&format!("'ssh-ed25519 {blob} tokenme@host'")));
        // No raw newline sneaks in: the printf format carries a literal \n.
        assert!(cmd.contains("printf '%s\\n'"));

        assert!(cmd_pubkey_append("bad'blob", "tokenme@host").is_err());
        assert!(cmd_pubkey_append(blob, "evil$(x)").is_err());
        assert!(cmd_pubkey_append(blob, "notokenme").is_err());
    }

    #[test]
    fn pubkey_remove_never_empties_on_error() {
        let blob = "AAAAC3NzaC1lZDI1NTE5AAAAIExample";
        let cmd = cmd_pubkey_remove(blob).unwrap();
        // rc must be read before anything is replaced, and rc ≥ 2 must bail.
        let rc_idx = cmd.find("rc=$?").unwrap();
        let mv_idx = cmd.find("mv -f").unwrap();
        assert!(rc_idx < mv_idx);
        assert!(cmd.contains("[ $rc -le 1 ]"));
        assert!(cmd.contains("exit 2"));
        assert!(cmd_pubkey_remove("bad blob").is_err());
    }
}

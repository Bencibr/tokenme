//! A thin runtime around `russh` — connect with TOFU pinning, authenticate by
//! password / private key / agent / the dedicated key, run one-shot commands,
//! and move files over an SFTP subsystem.
//!
//! Every wait has a timeout and every failure is classified into a stable
//! `kind` so the UI can say what actually went wrong (DNS vs auth vs a changed
//! host key) instead of showing a generic "failed".

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use russh::client::{self, AuthResult, Handle};
use russh::keys::agent::client::AgentClient;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect, MethodSet};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(60);
/// The first `tokenme export` on a machine ingests its full log retention;
/// later ones are incremental. Generous, because the first one is the slow one.
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(900);
pub const SFTP_TIMEOUT: Duration = Duration::from_secs(120);
/// The probe's `uname`/`hostname` reads — deliberately short.
pub const PROBE_EXEC_TIMEOUT: Duration = Duration::from_secs(20);
/// Output kept per stream; anything longer is truncated (nothing we run
/// legitimately prints more).
pub const OUTPUT_CAP: usize = 256 * 1024;
/// Cap for a bundle or manifest we are willing to pull from a server.
pub const BUNDLE_CAP: u64 = 64 * 1024 * 1024;

/// A classified failure. `kind` is the machine-readable bucket the UI
/// localizes; `detail` rides along verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshError {
    pub kind: String,
    pub detail: String,
}

impl SshError {
    pub fn new(kind: &str, detail: impl Into<String>) -> Self {
        Self { kind: kind.to_string(), detail: detail.into() }
    }

    pub fn timeout(what: impl fmt::Display) -> Self {
        Self::new("timeout", format!("timed out: {what}"))
    }
}

impl fmt::Display for SshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.detail)
    }
}

impl From<russh::Error> for SshError {
    fn from(e: russh::Error) -> Self {
        Self::new("proto", e.to_string())
    }
}

impl From<russh_sftp::client::error::Error> for SshError {
    fn from(e: russh_sftp::client::error::Error) -> Self {
        Self::new("sftp", e.to_string())
    }
}

impl From<std::io::Error> for SshError {
    fn from(e: std::io::Error) -> Self {
        Self::new("local_io", e.to_string())
    }
}

/// How to authenticate to a server. The password variant exists only inside an
/// open wizard session — it is zeroized when the wizard completes and never
/// touches `servers.json`.
#[derive(Clone)]
pub enum Auth {
    Password(Zeroizing<String>),
    Key { path: PathBuf, passphrase: Option<Zeroizing<String>> },
    /// The user's already-configured passwordless setup: ssh-agent, then the
    /// default key files.
    Default,
    /// The key pair this panel generated (`tokenme_ed25519`).
    Dedicated,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::Password(_) => f.write_str("Password(***)"),
            Auth::Key { path, passphrase } => {
                f.debug_struct("Key").field("path", path).field("passphrase", &passphrase.is_some()).finish()
            }
            Auth::Default => f.write_str("Default"),
            Auth::Dedicated => f.write_str("Dedicated"),
        }
    }
}

/// The host-key callback: records whatever fingerprint the server presented
/// and accepts it only when it matches the pin (or there is no pin yet — the
/// first-contact TOFU probe).
pub struct FpHandler {
    capture: Arc<StdMutex<Option<String>>>,
    pinned: Option<String>,
}

impl client::Handler for FpHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, server_public_key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let fp = fingerprint_of(server_public_key);
        if let Ok(mut slot) = self.capture.lock() {
            *slot = Some(fp.clone());
        }
        Ok(self.pinned.as_deref().map_or(true, |pinned| pinned == fp))
    }
}

pub fn fingerprint_of(key: &PublicKeyOrCertificate) -> String {
    match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => {
            key.fingerprint(russh::keys::HashAlg::Sha256).to_string()
        }
        // A certificate's own blob has no fingerprint in ssh-key 0.7; the pin
        // is the certified key's, computed the way the plain arm does it.
        PublicKeyOrCertificate::Certificate(cert) => {
            cert.public_key().fingerprint(russh::keys::HashAlg::Sha256).to_string()
        }
    }
}

pub struct Conn {
    pub handle: Handle<FpHandler>,
    pub fingerprint: String,
}

/// Connects (10 s) and performs the host-key check. `pin = None` is the
/// first-contact probe; a later connect with `pin = Some(fp)` fails with
/// `host_key_changed` if the server now presents a different key.
pub async fn connect(host: &str, port: u16, pin: Option<&str>) -> Result<Conn, SshError> {
    // Resolve first so a name that cannot resolve is reported as `dns`, not
    // as a generic connect failure.
    let addrs = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| SshError::timeout("DNS resolution"))?
        .map_err(|e| SshError::new("dns", format!("cannot resolve {host}: {e}")))?;
    let addr = addrs
        .into_iter()
        .next()
        .ok_or_else(|| SshError::new("dns", format!("{host} resolved to no address")))?;

    let capture: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
    let handler = FpHandler { capture: capture.clone(), pinned: pin.map(str::to_string) };
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(30)),
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 6,
        ..Default::default()
    });

    match tokio::time::timeout(CONNECT_TIMEOUT, client::connect(config, addr, handler)).await {
        Ok(Ok(handle)) => {
            let fingerprint = capture
                .lock()
                .ok()
                .and_then(|c| c.clone())
                .ok_or_else(|| SshError::new("proto", "server presented no host key"))?;
            if let Some(pinned) = pin {
                if pinned != fingerprint {
                    return Err(SshError::new(
                        "host_key_changed",
                        format!("expected {pinned}, server presented {fingerprint}"),
                    ));
                }
            }
            Ok(Conn { handle, fingerprint })
        }
        Ok(Err(e)) => {
            let got = capture.lock().ok().and_then(|c| c.clone());
            match (pin, got) {
                (Some(pinned), Some(got)) if pinned != got => Err(SshError::new(
                    "host_key_changed",
                    format!("expected {pinned}, server presented {got}"),
                )),
                _ => Err(SshError::new("unreachable", format!("{host}:{port}: {e}"))),
            }
        }
        Err(_) => Err(SshError::timeout(format!("the connection to {host}:{port}"))),
    }
}

/// Authenticates; a rejected method is `kind = auth`, a broken key file is
/// `kind = key_missing`, a transport failure is `proto`.
pub async fn authenticate(conn: &mut Conn, user: &str, auth: &Auth) -> Result<(), SshError> {
    let result = match auth {
        Auth::Password(password) => conn
            .handle
            .authenticate_password(user, password.as_str())
            .await
            .map_err(|e| SshError::new("proto", format!("authentication failed: {e}")))?,
        Auth::Key { path, passphrase } => {
            auth_with_private_key(&mut conn.handle, user, path, passphrase.as_deref().map(String::as_str))
                .await?
        }
        Auth::Dedicated => {
            let path = super::keys::dedicated_key_path()?;
            auth_with_private_key(&mut conn.handle, user, &path, None).await?
        }
        Auth::Default => auth_default(&mut conn.handle, user).await?,
    };
    if result.success() {
        Ok(())
    } else {
        Err(SshError::new(
            "auth",
            match auth {
                Auth::Password(_) => "the server rejected the username or password".to_string(),
                Auth::Key { .. } => "the server rejected the private key".to_string(),
                Auth::Dedicated => "the server rejected the dedicated tokenme key".to_string(),
                Auth::Default => {
                    "no agent or default key the server accepts (try `ssh-add` or pick a key)".to_string()
                }
            },
        ))
    }
}

/// `~` and `~/…` are what a user types into the key-path field; the ssh-key
/// loader has no notion of a home directory.
fn expand_tilde(path: &Path) -> std::path::PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    path.to_path_buf()
}

async fn auth_with_private_key(
    handle: &mut Handle<FpHandler>,
    user: &str,
    path: &Path,
    passphrase: Option<&str>,
) -> Result<AuthResult, SshError> {
    let path = &expand_tilde(path);
    let key = load_secret_key(path, passphrase).map_err(|e| {
        SshError::new("key_missing", format!("cannot read the private key {}: {e}", path.display()))
    })?;
    let hash_alg = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| SshError::new("proto", format!("key exchange failed: {e}")))?
        .flatten();
    let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
    handle
        .authenticate_publickey(user, key)
        .await
        .map_err(|e| SshError::new("proto", format!("authentication failed: {e}")))
}

async fn auth_default(handle: &mut Handle<FpHandler>, user: &str) -> Result<AuthResult, SshError> {
    // 1) ssh-agent (SSH_AUTH_SOCK on unix, Pageant on Windows).
    #[cfg(unix)]
    let agent = AgentClient::connect_env().await;
    #[cfg(windows)]
    let agent = AgentClient::connect_pageant().await;
    if let Ok(mut agent) = agent {
        if let Ok(identities) = agent.request_identities().await {
            for id in identities {
                let public = id.public_key().into_owned();
                match handle.authenticate_publickey_with(user, public, None, &mut agent).await {
                    Ok(r) if r.success() => return Ok(r),
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
        }
    }
    // 2) The usual default key files.
    if let Some(home) = dirs::home_dir() {
        for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            let path = home.join(".ssh").join(name);
            if !path.exists() {
                continue;
            }
            if let Ok(r) = auth_with_private_key(handle, user, &path, None).await {
                if r.success() {
                    return Ok(r);
                }
            }
        }
    }
    Ok(AuthResult::Failure { remaining_methods: MethodSet::empty(), partial_success: false })
}

#[derive(Debug, Clone)]
pub struct ExecOut {
    pub code: Option<u32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

impl ExecOut {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// A compact one-line summary for error details.
    pub fn summary(&self) -> String {
        let mut s = format!("exit {}", self.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()));
        let stderr = self.stderr.trim();
        let stdout = self.stdout.trim();
        if !stderr.is_empty() {
            s.push_str(&format!("; stderr: {}", first_line(stderr, 300)));
        } else if !stdout.is_empty() {
            s.push_str(&format!("; stdout: {}", first_line(stdout, 300)));
        }
        if self.truncated {
            s.push_str(" (output truncated)");
        }
        s
    }
}

fn first_line(s: &str, cap: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.len() > cap {
        format!("{}…", &line[..cap])
    } else {
        line.to_string()
    }
}

/// Runs one command and collects stdout/stderr/exit status until the channel
/// closes. The command string is always built by `remote.rs` from constants.
pub async fn exec(handle: &Handle<FpHandler>, cmd: &str, timeout: Duration) -> Result<ExecOut, SshError> {
    let fut = async {
        let mut channel = handle.channel_open_session().await?;
        channel.exec(true, cmd).await?;
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let mut code = None;
        let mut truncated = false;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => push_capped(&mut out, &data, &mut truncated),
                ChannelMsg::ExtendedData { data, .. } => push_capped(&mut err, &data, &mut truncated),
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                _ => {}
            }
        }
        Ok::<_, russh::Error>(ExecOut {
            code,
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            truncated,
        })
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(SshError::new("proto", format!("remote command failed: {e}"))),
        Err(_) => Err(SshError::timeout(format!("the remote command `{}`", first_line(cmd, 80)))),
    }
}

fn push_capped(buf: &mut Vec<u8>, data: &[u8], truncated: &mut bool) {
    if buf.len() + data.len() > OUTPUT_CAP {
        *truncated = true;
        return;
    }
    buf.extend_from_slice(data);
}

/// Opens the SFTP subsystem on its own channel.
pub async fn sftp(handle: &Handle<FpHandler>) -> Result<SftpSession, SshError> {
    let fut = async {
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::new("sftp", format!("cannot open a channel: {e}")))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| SshError::new("sftp", format!("the server refused the sftp subsystem: {e}")))?;
        SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| SshError::new("sftp", format!("cannot start sftp: {e}")))
    };
    tokio::time::timeout(SFTP_TIMEOUT, fut)
        .await
        .map_err(|_| SshError::timeout("the sftp subsystem"))?
}

/// SFTP paths are relative to the login directory (the protocol has no `~`),
/// which is why the collector lives under `.tokenme/` in both worlds.
pub async fn sftp_read(sn: &SftpSession, path: &str, cap: u64) -> Result<Vec<u8>, SshError> {
    let md = tokio::time::timeout(SFTP_TIMEOUT, sn.metadata(path))
        .await
        .map_err(|_| SshError::timeout(format!("sftp metadata for {path}")))?
        .map_err(|e| SshError::new("sftp", format!("{path}: {e}")))?;
    if md.len() > cap {
        return Err(SshError::new("bundle_too_big", format!("{path} is {} bytes (cap {cap})", md.len())));
    }
    let data = tokio::time::timeout(SFTP_TIMEOUT, sn.read(path))
        .await
        .map_err(|_| SshError::timeout(format!("sftp read of {path}")))?
        .map_err(|e| SshError::new("sftp", format!("{path}: {e}")))?;
    if data.len() as u64 > cap {
        return Err(SshError::new("bundle_too_big", format!("{path} grew past {cap} bytes mid-read")));
    }
    Ok(data)
}

pub async fn sftp_write(sn: &SftpSession, path: &str, data: &[u8]) -> Result<(), SshError> {
    // `SftpSession::write` opens with `WRITE` alone, so a not-yet-existing file
    // fails with SSH_FX_NO_SUCH_FILE — which is every first collector upload.
    // Open with CREATE|TRUNCATE and stream the bytes ourselves.
    let fut = async {
        let mut file = sn
            .open_with_flags(path, OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE)
            .await
            .map_err(|e| SshError::new("sftp", format!("{path}: {e}")))?;
        file.write_all(data).await.map_err(|e| SshError::new("sftp", format!("{path}: {e}")))?;
        file.close().await.map_err(|e| SshError::new("sftp", format!("{path}: {e}")))?;
        Ok(())
    };
    tokio::time::timeout(SFTP_TIMEOUT, fut)
        .await
        .map_err(|_| SshError::timeout(format!("sftp write of {path}")))?
}

pub async fn sftp_close(conn: &mut Conn) {
    let _ = conn.handle.disconnect(Disconnect::ByApplication, "done", "en").await;
}

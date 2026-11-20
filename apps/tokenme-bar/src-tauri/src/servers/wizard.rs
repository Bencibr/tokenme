//! The add-server wizard's backend: a TOFU probe that captures the host key
//! and checks the architecture, then the nine-step install that provisions the
//! dedicated key, the collector, and the first bundle.
//!
//! Credential discipline: the password lives only inside the in-memory
//! `WizardSession` (a `Zeroizing<String>`) and is dropped the moment the
//! dedicated key is proven to work — step "clear_pw" is real. It never reaches
//! `servers.json`, a bundle name, or a log line.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Runtime};

use super::fetch;
use super::keys;
use super::remote;
use super::ssh::{self, Auth};
use super::{collector, host_port_key, ServerRecord, ServerView, ServersState, SshError, SyncEvent};
use super::EV_INSTALL_PROGRESS;

/// The wizard's step list, in order. Keys are stable — the UI maps them to
/// localized labels.
pub const STEPS: [&str; 9] =
    ["keygen", "pubkey", "reconnect", "clear_pw", "arch", "collector", "export", "detect", "merge"];

/// Re-login path for an existing server: rewrite the dedicated key only.
/// Same keys as the first four install steps so the UI reuses their labels.
pub const REAUTH_STEPS: [&str; 4] = ["keygen", "pubkey", "reconnect", "clear_pw"];

/// How long the last step waits for the engine to publish the merge before
/// saying "written, merge pending" instead.
const MERGE_WAIT: Duration = Duration::from_secs(20);

/// At most this many open sessions; oldest falls off first. A leaked session
/// (app closed mid-wizard, no abort) must not pin a password forever.
const MAX_SESSIONS: usize = 4;

pub struct WizardSession {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Auth,
    /// The fingerprint the probe saw; install re-pins to it.
    pub fingerprint: String,
    /// When set, this session is for `server_reauth_*` against an existing
    /// record. `install` must refuse it (would create a duplicate); apply
    /// updates that id instead.
    pub reauth_of: Option<u64>,
}
#[derive(Debug, Deserialize)]
pub struct ProbeReq {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub user: String,
    pub auth: AuthReq,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthReq {
    Password { password: String },
    Key {
        path: String,
        #[serde(default)]
        passphrase: Option<String>,
    },
    Default,
}

impl AuthReq {
    fn into_auth(self) -> Auth {
        use zeroize::Zeroizing;
        match self {
            AuthReq::Password { password } => Auth::Password(Zeroizing::new(password)),
            AuthReq::Key { path, passphrase } => Auth::Key {
                path: path.into(),
                passphrase: passphrase.map(Zeroizing::new),
            },
            AuthReq::Default => Auth::Default,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ProbeOutcome {
    pub ok: bool,
    pub session: Option<u64>,
    pub fingerprint: Option<String>,
    /// What `servers.json` currently pins for this host:port, if anything.
    pub known_fingerprint: Option<String>,
    /// The server presented a different key than the pin — the UI must make
    /// the user explicitly re-confirm before install may proceed.
    pub mismatch: bool,
    pub arch: Option<String>,
    pub hostname: Option<String>,
    pub error: Option<SshError>,
}

fn probe_error(kind: &str, detail: impl Into<String>) -> ProbeOutcome {
    ProbeOutcome {
        ok: false,
        session: None,
        fingerprint: None,
        known_fingerprint: None,
        mismatch: false,
        arch: None,
        hostname: None,
        error: Some(SshError::new(kind, detail)),
    }
}

/// Connects, authenticates and reads `uname -m` + `hostname`. Stores the
/// session (auth included) on success so `install` can continue without
/// asking for the password again.
pub async fn probe(state: &Arc<ServersState>, req: ProbeReq) -> ProbeOutcome {
    let port = req.port.unwrap_or(22);
    if !remote::valid_name(&req.name) {
        return probe_error("name", "name must be 1–64 characters of A–Z a–z 0–9 . _ -");
    }
    if req.name == usage_index::hostname() {
        return probe_error("name", "this name equals the local hostname; pick a different one");
    }
    if !remote::valid_host(&req.host) {
        return probe_error("host", "invalid host");
    }
    if !remote::valid_user(&req.user) {
        return probe_error("user", "invalid user name");
    }
    if port == 0 {
        return probe_error("host", "invalid port");
    }
    {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return probe_error("proto", "server store is busy"),
        };
        if store.servers.iter().any(|r| r.name == req.name) {
            return probe_error("name", "another server already uses this name");
        }
    }

    let auth = req.auth.into_auth();
    let mut conn = match ssh::connect(&req.host, port, None).await {
        Ok(c) => c,
        Err(e) => return probe_error(&e.kind, e.detail),
    };
    if let Err(e) = ssh::authenticate(&mut conn, &req.user, &auth).await {
        ssh::sftp_close(&mut conn).await;
        return ProbeOutcome {
            ok: false,
            session: None,
            fingerprint: Some(conn.fingerprint.clone()),
            error: Some(e),
            ..empty_outcome()
        };
    }

    let uname = ssh::exec(&conn.handle, remote::CMD_UNAME, ssh::PROBE_EXEC_TIMEOUT).await;
    let arch_raw = match uname {
        Ok(out) if out.ok() => out.stdout.trim().to_string(),
        Ok(out) => {
            ssh::sftp_close(&mut conn).await;
            return ProbeOutcome {
                fingerprint: Some(conn.fingerprint.clone()),
                error: Some(SshError::new("remote_cmd", format!("uname failed: {}", out.summary()))),
                ..empty_outcome()
            };
        }
        Err(e) => {
            ssh::sftp_close(&mut conn).await;
            return ProbeOutcome { fingerprint: Some(conn.fingerprint.clone()), error: Some(e), ..empty_outcome() };
        }
    };
    if collector::arch_target(&arch_raw).is_none() {
        ssh::sftp_close(&mut conn).await;
        return ProbeOutcome {
            fingerprint: Some(conn.fingerprint.clone()),
            arch: Some(arch_raw.clone()),
            error: Some(SshError::new(
                "unsupported_arch",
                format!("architecture {arch_raw:?} has no bundled collector — use scripts/install-linux.sh --push on this machine"),
            )),
            ..empty_outcome()
        };
    }

    // Best-effort: the display name of the box, for the confirm screen.
    let hostname = match ssh::exec(&conn.handle, remote::CMD_HOSTNAME, ssh::PROBE_EXEC_TIMEOUT).await {
        Ok(out) if out.ok() => {
            let h = out.stdout.trim().to_string();
            if h.is_empty() { None } else { Some(h) }
        }
        _ => None,
    };

    let fingerprint = conn.fingerprint.clone();
    ssh::sftp_close(&mut conn).await;

    let known = state
        .store
        .lock()
        .ok()
        .and_then(|s| s.known_hosts.get(&host_port_key(&req.host, port)).cloned());
    let mismatch = known.as_deref().is_some_and(|k| k != fingerprint);

    let id = state.next_id();
    if let Ok(mut wizards) = state.wizards.lock() {
        if wizards.len() >= MAX_SESSIONS {
            if let Some(oldest) = wizards.keys().min().copied() {
                wizards.remove(&oldest);
            }
        }
        wizards.insert(
            id,
            WizardSession {
                name: req.name,
                host: req.host,
                port,
                user: req.user,
                auth,
                fingerprint: fingerprint.clone(),
                reauth_of: None,
            },
        );
    }

    ProbeOutcome {
        ok: true,
        session: Some(id),
        fingerprint: Some(fingerprint),
        known_fingerprint: known,
        mismatch,
        arch: Some(arch_raw),
        hostname,
        error: None,
    }
}

#[derive(Debug, Deserialize)]
pub struct ReauthProbeReq {
    pub id: u64,
    /// Optional overrides — leave unset to keep the stored value. Used when
    /// the box moved to a new IP (or the login user changed) but it is still
    /// the same registered server.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: Option<String>,
    pub auth: AuthReq,
    /// Set after the UI shows a fingerprint mismatch and the user explicitly
    /// trusts the new key. Without this, a changed host key stops the probe
    /// *before* any password / private-key material is sent.
    #[serde(default)]
    pub ack_mismatch: bool,
    /// The fingerprint the user confirmed on the mismatch screen. Required
    /// when `ack_mismatch` is true: if the host now presents a different key,
    /// credentials are still withheld and the UI must ask again.
    #[serde(default)]
    pub expected_fingerprint: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReauthApplyReq {
    pub session: u64,
    /// The fingerprint the user confirmed (equal to what the probe saw).
    pub fingerprint: String,
}

/// Probe an already-registered server for re-login. Name stays fixed; host /
/// port / user may change. Does not treat the existing name as a collision.
pub async fn reauth_probe(state: &Arc<ServersState>, req: ReauthProbeReq) -> ProbeOutcome {
    let (name, host, port, user, prev_fp) = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return probe_error("proto", "server store is busy"),
        };
        let Some(rec) = store.servers.iter().find(|r| r.id == req.id) else {
            return probe_error("proto", "server not found");
        };
        let host = req.host.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&rec.host).to_string();
        let port = req.port.unwrap_or(rec.port);
        let user = req.user.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&rec.user).to_string();
        (rec.name.clone(), host, port, user, rec.fingerprint.clone())
    };

    if !remote::valid_host(&host) {
        return probe_error("host", "invalid host");
    }
    if !remote::valid_user(&user) {
        return probe_error("user", "invalid user name");
    }
    if port == 0 {
        return probe_error("host", "invalid port");
    }

    let auth = req.auth.into_auth();
    // TOFU with no pin: an IP change presents a key against a new host:port,
    // and we still need the fingerprint even when it differs from the record.
    let mut conn = match ssh::connect(&host, port, None).await {
        Ok(c) => c,
        Err(e) => return probe_error(&e.kind, e.detail),
    };
    let presented = conn.fingerprint.clone();
    let mismatch = !prev_fp.is_empty() && presented != prev_fp;
    // Never send the one-time password / user key to a host whose key differs
    // from the pin we already trust for this server id — that would hand the
    // credential to an impersonator before the mismatch UI can stop us.
    if mismatch && !req.ack_mismatch {
        ssh::sftp_close(&mut conn).await;
        return ProbeOutcome {
            ok: false,
            session: None,
            fingerprint: Some(presented),
            known_fingerprint: Some(prev_fp),
            mismatch: true,
            arch: None,
            hostname: None,
            error: None,
        };
    }
    if mismatch {
        // Ack alone is not enough: bind approval to the exact fingerprint the
        // user saw. A TOCTOU swap between screens must not inherit that ack.
        let expected = req.expected_fingerprint.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let Some(expected) = expected else {
            ssh::sftp_close(&mut conn).await;
            return probe_error("proto", "ack_mismatch requires expected_fingerprint");
        };
        if presented != expected {
            ssh::sftp_close(&mut conn).await;
            return ProbeOutcome {
                ok: false,
                session: None,
                fingerprint: Some(presented),
                known_fingerprint: Some(prev_fp),
                mismatch: true,
                arch: None,
                hostname: None,
                error: None,
            };
        }
    }
    if let Err(e) = ssh::authenticate(&mut conn, &user, &auth).await {
        ssh::sftp_close(&mut conn).await;
        // Fingerprint was already accepted for this attempt (matched pin or
        // ack+expected). Do not set mismatch — that would send the UI back to
        // the trust screen and hide the auth error (wrong password, etc.).
        return ProbeOutcome {
            ok: false,
            session: None,
            fingerprint: Some(conn.fingerprint.clone()),
            known_fingerprint: Some(prev_fp),
            mismatch: false,
            arch: None,
            hostname: None,
            error: Some(e),
        };
    }

    let uname = ssh::exec(&conn.handle, remote::CMD_UNAME, ssh::PROBE_EXEC_TIMEOUT).await;
    let arch_raw = match uname {
        Ok(out) if out.ok() => out.stdout.trim().to_string(),
        Ok(out) => {
            ssh::sftp_close(&mut conn).await;
            return ProbeOutcome {
                fingerprint: Some(conn.fingerprint.clone()),
                known_fingerprint: Some(prev_fp),
                error: Some(SshError::new("remote_cmd", format!("uname failed: {}", out.summary()))),
                ..empty_outcome()
            };
        }
        Err(e) => {
            ssh::sftp_close(&mut conn).await;
            return ProbeOutcome {
                fingerprint: Some(conn.fingerprint.clone()),
                known_fingerprint: Some(prev_fp),
                error: Some(e),
                ..empty_outcome()
            };
        }
    };
    if collector::arch_target(&arch_raw).is_none() {
        ssh::sftp_close(&mut conn).await;
        return ProbeOutcome {
            fingerprint: Some(conn.fingerprint.clone()),
            known_fingerprint: Some(prev_fp),
            arch: Some(arch_raw.clone()),
            error: Some(SshError::new(
                "unsupported_arch",
                format!("architecture {arch_raw:?} has no bundled collector — use scripts/install-linux.sh --push on this machine"),
            )),
            ..empty_outcome()
        };
    }

    let hostname = match ssh::exec(&conn.handle, remote::CMD_HOSTNAME, ssh::PROBE_EXEC_TIMEOUT).await {
        Ok(out) if out.ok() => {
            let h = out.stdout.trim().to_string();
            if h.is_empty() { None } else { Some(h) }
        }
        _ => None,
    };

    let fingerprint = conn.fingerprint.clone();
    ssh::sftp_close(&mut conn).await;

    let known = if !prev_fp.is_empty() { Some(prev_fp) } else { None };

    let session_id = state.next_id();
    if let Ok(mut wizards) = state.wizards.lock() {
        if wizards.len() >= MAX_SESSIONS {
            if let Some(oldest) = wizards.keys().min().copied() {
                wizards.remove(&oldest);
            }
        }
        wizards.insert(
            session_id,
            WizardSession {
                name,
                host,
                port,
                user,
                auth,
                fingerprint: fingerprint.clone(),
                reauth_of: Some(req.id),
            },
        );
    }

    ProbeOutcome {
        ok: true,
        session: Some(session_id),
        fingerprint: Some(fingerprint),
        known_fingerprint: known,
        // After an explicit ack the new key is accepted; the UI should not
        // re-prompt. A matching pin never set mismatch above.
        mismatch: false,
        arch: Some(arch_raw),
        hostname,
        error: None,
    }
}

fn empty_outcome() -> ProbeOutcome {
    ProbeOutcome {
        ok: false,
        session: None,
        fingerprint: None,
        known_fingerprint: None,
        mismatch: false,
        arch: None,
        hostname: None,
        error: None,
    }
}

pub fn abort(state: &Arc<ServersState>, session: u64) {
    if let Ok(mut wizards) = state.wizards.lock() {
        wizards.remove(&session); // dropping the session zeroizes any password
    }
}

#[derive(Debug, Deserialize)]
pub struct InstallReq {
    pub session: u64,
    pub every_secs: u64,
    pub days: i64,
    /// The fingerprint the user confirmed (equal to what the probe saw; the
    /// UI only offers this after the "仍要信任并更新" checkbox on mismatch).
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepState {
    pub key: String,
    /// `pending` | `active` | `done` | `warn` | `error`.
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressPayload {
    pub session: u64,
    pub steps: Vec<StepState>,
    pub done: bool,
    pub ok: bool,
    pub error: Option<SshError>,
}

struct Progress<'a, R: Runtime> {
    app: &'a AppHandle<R>,
    session: u64,
    steps: Vec<StepState>,
    failed: bool,
}

impl<'a, R: Runtime> Progress<'a, R> {
    fn new(app: &'a AppHandle<R>, session: u64) -> Self {
        Self::with_steps(app, session, &STEPS)
    }

    fn for_reauth(app: &'a AppHandle<R>, session: u64) -> Self {
        Self::with_steps(app, session, &REAUTH_STEPS)
    }

    fn with_steps(app: &'a AppHandle<R>, session: u64, keys: &[&str]) -> Self {
        let steps = keys
            .iter()
            .map(|k| StepState { key: (*k).to_string(), state: "pending".into(), detail: None })
            .collect();
        let p = Self { app, session, steps, failed: false };
        p.emit(false, true, None);
        p
    }

    fn set(&mut self, key: &str, state: &str, detail: Option<String>) {
        if let Some(step) = self.steps.iter_mut().find(|s| s.key == key) {
            step.state = state.to_string();
            step.detail = detail;
        }
    }

    fn active(&mut self, key: &str) {
        self.set(key, "active", None);
        self.emit(false, true, None);
    }

    fn done(&mut self, key: &str, detail: Option<String>) {
        self.set(key, "done", detail);
        self.emit(false, true, None);
    }

    fn warn(&mut self, key: &str, detail: Option<String>) {
        self.set(key, "warn", detail);
        self.emit(false, true, None);
    }

    fn fail(&mut self, key: &str, err: &SshError) {
        self.set(key, "error", Some(err.detail.clone()));
        self.failed = true;
        self.emit(true, false, Some(err.clone()));
    }

    fn finish(&mut self) {
        self.emit(true, !self.failed, None);
    }

    fn emit(&self, done: bool, ok: bool, error: Option<SshError>) {
        let payload = ProgressPayload {
            session: self.session,
            steps: self.steps.clone(),
            done,
            ok,
            error,
        };
        let _ = self.app.emit(EV_INSTALL_PROGRESS, payload);
    }
}

#[derive(Debug, Serialize)]
pub struct InstallOutcome {
    pub ok: bool,
    pub server: Option<ServerView>,
    pub detected: Vec<String>,
    pub rows: u64,
    /// The bundle is on disk but the engine had not published its merge yet.
    pub merge_pending: bool,
    pub error: Option<SshError>,
}

fn install_fail(error: SshError) -> InstallOutcome {
    InstallOutcome { ok: false, server: None, detected: vec![], rows: 0, merge_pending: false, error: Some(error) }
}

/// The nine-step install. Consumes the wizard session at entry (a failure
/// mid-install means restarting the wizard — where the user re-enters a
/// password — rather than keeping one alive indefinitely).
pub async fn install<R: Runtime>(app: &AppHandle<R>, state: &Arc<ServersState>, req: InstallReq) -> InstallOutcome {
    let session = state.wizards.lock().ok().and_then(|mut m| m.remove(&req.session));
    let Some(mut session) = session else {
        return install_fail(SshError::new("proto", "the wizard session expired — start over"));
    };
    if session.reauth_of.is_some() {
        return install_fail(SshError::new(
            "proto",
            "this session belongs to re-login — use server_reauth_apply",
        ));
    }
    if !remote::valid_every(req.every_secs) {
        return install_fail(SshError::new("proto", "sync interval out of range"));
    }
    if !remote::valid_days(req.days) {
        return install_fail(SshError::new("proto", "sync window out of range"));
    }
    if req.fingerprint != session.fingerprint {
        return install_fail(SshError::new(
            "host_key_changed",
            format!("the confirmed fingerprint {} differs from the one the probe saw ({})", req.fingerprint, session.fingerprint),
        ));
    }

    let mut prog = Progress::new(app, req.session);

    // 1. keygen — the dedicated key pair, generated locally on first use.
    prog.active("keygen");
    let key = match keys::ensure_keypair() {
        Ok(k) => k,
        Err(e) => {
            prog.fail("keygen", &e);
            return install_fail(e);
        }
    };
    prog.done("keygen", Some(key.path.display().to_string()));

    // 2. pubkey — append over the *current* (password / user key) session.
    prog.active("pubkey");
    let mut conn = match ssh::connect(&session.host, session.port, Some(&session.fingerprint)).await {
        Ok(c) => c,
        Err(e) => {
            prog.fail("pubkey", &e);
            return install_fail(e);
        }
    };
    if let Err(e) = ssh::authenticate(&mut conn, &session.user, &session.auth).await {
        prog.fail("pubkey", &e);
        return install_fail(e);
    }
    let append = match remote::cmd_pubkey_append(&key.blob, &key.comment) {
        Ok(cmd) => ssh::exec(&conn.handle, &cmd, ssh::EXEC_TIMEOUT).await,
        Err(e) => Err(e),
    };
    match append {
        Ok(out) if out.ok() => prog.done("pubkey", None),
        Ok(out) => {
            let e = SshError::new("remote_cmd", format!("writing authorized_keys failed: {}", out.summary()));
            prog.fail("pubkey", &e);
            return install_fail(e);
        }
        Err(e) => {
            prog.fail("pubkey", &e);
            return install_fail(e);
        }
    }
    ssh::sftp_close(&mut conn).await;

    // 3. reconnect — a brand-new connection authenticating with the dedicated
    //    key alone; this is the proof that passwordless access took.
    prog.active("reconnect");
    let mut conn = match ssh::connect(&session.host, session.port, Some(&session.fingerprint)).await {
        Ok(c) => c,
        Err(e) => {
            prog.fail("reconnect", &e);
            return install_fail(e);
        }
    };
    if let Err(e) = ssh::authenticate(&mut conn, &session.user, &Auth::Dedicated).await {
        let e = SshError::new(
            "auth",
            format!(
                "the dedicated key was written but does not authenticate yet ({}) — check sshd's AuthorizedKeysFile and ~/.ssh permissions",
                e.detail
            ),
        );
        prog.fail("reconnect", &e);
        return install_fail(e);
    }
    prog.done("reconnect", None);

    // 4. clear_pw — drop the one-time credential for real.
    prog.active("clear_pw");
    session.auth = Auth::Dedicated;
    prog.done("clear_pw", None);

    // 5. arch — checked again on the live session (the probe's answer could
    //    be stale by now, and this is the flag the upload follows).
    prog.active("arch");
    let uname = match ssh::exec(&conn.handle, remote::CMD_UNAME, ssh::PROBE_EXEC_TIMEOUT).await {
        Ok(out) if out.ok() => out.stdout.trim().to_string(),
        Ok(out) => {
            let e = SshError::new("remote_cmd", format!("uname failed: {}", out.summary()));
            prog.fail("arch", &e);
            return install_fail(e);
        }
        Err(e) => {
            prog.fail("arch", &e);
            return install_fail(e);
        }
    };
    if collector::arch_target(&uname).is_none() {
        let e = SshError::new(
            "unsupported_arch",
            format!("architecture {uname:?} has no bundled collector — use scripts/install-linux.sh --push on this machine"),
        );
        prog.fail("arch", &e);
        return install_fail(e);
    }
    prog.done("arch", Some(uname.clone()));

    // 6. collector — sftp upload, sha verify, atomic install.
    prog.active("collector");
    let sn = match ssh::sftp(&conn.handle).await {
        Ok(sn) => sn,
        Err(e) => {
            prog.fail("collector", &e);
            return install_fail(e);
        }
    };
    let uploaded = match fetch::ensure_collector(app, &conn, &sn).await {
        Ok(u) => u,
        Err(e) => {
            prog.fail("collector", &e);
            return install_fail(e);
        }
    };
    prog.done("collector", Some(if uploaded { "uploaded and verified".into() } else { "already current".into() }));

    // 7. export — the first collection; ingests the server's full retention,
    //    so this is the slow step.
    prog.active("export");
    if let Err(e) = fetch::run_export(&conn, &session.name, req.days).await {
        prog.fail("export", &e);
        return install_fail(e);
    }
    prog.done("export", None);

    // 8. detect — informational; a failure here does not fail the install.
    prog.active("detect");
    let detected = match fetch::detect_tools(&conn).await {
        Ok(tools) => {
            prog.done("detect", Some(format!("{} tool(s)", tools.len())));
            tools
        }
        Err(e) => {
            prog.warn("detect", Some(e.detail.clone()));
            Vec::new()
        }
    };

    // 9. merge — bring the bundle home and wake the engine.
    prog.active("merge");
    let fetched = match fetch::fetch_bundle(&sn, &session.name).await {
        Ok(f) => f,
        Err(e) => {
            prog.fail("merge", &e);
            return install_fail(e);
        }
    };
    let gz_name = format!("tokenme-{}.jsonl.gz", session.name);
    let mut merge_pending = false;
    let rows = if fetched.written {
        fetch::wake_engine(app);
        match fetch::await_merge(app, &gz_name, &fetched.sha256, MERGE_WAIT).await {
            Some(rows) => {
                prog.done("merge", Some(format!("{rows} rows merged")));
                rows
            }
            None => {
                merge_pending = true;
                prog.warn("merge", Some("written to ~/tokenme-sync; the merge will complete shortly".into()));
                fetched.rows
            }
        }
    } else {
        prog.done("merge", Some(format!("{} rows already merged", fetched.rows)));
        fetched.rows
    };
    ssh::sftp_close(&mut conn).await;

    // Persist: the record plus this host's fingerprint pin.
    let now_ms = usage_core::report::now_ms();
    let id = state.next_id();
    let record = ServerRecord {
        id,
        name: session.name.clone(),
        host: session.host.clone(),
        port: session.port,
        user: session.user.clone(),
        fingerprint: session.fingerprint.clone(),
        every_secs: req.every_secs,
        days: req.days,
        enabled: true,
        created_at_ms: now_ms,
        last_ok_ms: Some(now_ms),
        last_error: None,
        fail_count: 0,
        last_rows: rows,
        last_took_ms: 0,
        tools: detected.clone(),
        history: vec![SyncEvent { at_ms: now_ms, rows, took_ms: 0, ok: true }],
    };
    let host_key = host_port_key(&session.host, session.port);
    let fingerprint = session.fingerprint.clone();
    if let Err(e) = state.mutate(|store| {
        store.known_hosts.insert(host_key, fingerprint);
        store.servers.push(record);
    }) {
        let e = SshError::new("local_io", e);
        prog.fail("merge", &e);
        return install_fail(e);
    }
    if let Ok(mut due) = state.next_due.lock() {
        due.insert(id, Instant::now() + Duration::from_secs(req.every_secs.max(60)));
    }
    state.hub_send(super::hub::HubMsg::Changed);
    prog.finish();
    state.emit_updated(app);

    let view = state.views(now_ms).into_iter().find(|v| v.id == id);
    InstallOutcome { ok: true, server: view, detected, rows, merge_pending, error: None }
}

/// Re-login against an existing record: rewrite the dedicated key, update
/// host/port/user/fingerprint on the same id, clear backoff, and pull now.
/// Does not create a second server and does not re-upload the collector.
pub async fn reauth_apply<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<ServersState>,
    req: ReauthApplyReq,
) -> InstallOutcome {
    let session = state.wizards.lock().ok().and_then(|mut m| m.remove(&req.session));
    let Some(mut session) = session else {
        return install_fail(SshError::new("proto", "the wizard session expired — start over"));
    };
    let Some(server_id) = session.reauth_of else {
        return install_fail(SshError::new(
            "proto",
            "this session belongs to add-server — use server_install",
        ));
    };
    if req.fingerprint != session.fingerprint {
        return install_fail(SshError::new(
            "host_key_changed",
            format!(
                "the confirmed fingerprint {} differs from the one the probe saw ({})",
                req.fingerprint, session.fingerprint
            ),
        ));
    }

    // Confirm the target still exists before burning the one-time password
    // on a pubkey write that we cannot attach to a record.
    let prev = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return install_fail(SshError::new("proto", "server store is busy")),
        };
        match store.servers.iter().find(|r| r.id == server_id) {
            Some(r) => (r.host.clone(), r.port),
            None => return install_fail(SshError::new("proto", "server not found")),
        }
    };

    let mut prog = Progress::for_reauth(app, req.session);

    prog.active("keygen");
    let key = match keys::ensure_keypair() {
        Ok(k) => k,
        Err(e) => {
            prog.fail("keygen", &e);
            return install_fail(e);
        }
    };
    prog.done("keygen", Some(key.path.display().to_string()));

    prog.active("pubkey");
    let mut conn = match ssh::connect(&session.host, session.port, Some(&session.fingerprint)).await {
        Ok(c) => c,
        Err(e) => {
            prog.fail("pubkey", &e);
            return install_fail(e);
        }
    };
    if let Err(e) = ssh::authenticate(&mut conn, &session.user, &session.auth).await {
        prog.fail("pubkey", &e);
        return install_fail(e);
    }
    let append = match remote::cmd_pubkey_append(&key.blob, &key.comment) {
        Ok(cmd) => ssh::exec(&conn.handle, &cmd, ssh::EXEC_TIMEOUT).await,
        Err(e) => Err(e),
    };
    match append {
        Ok(out) if out.ok() => prog.done("pubkey", None),
        Ok(out) => {
            let e = SshError::new("remote_cmd", format!("writing authorized_keys failed: {}", out.summary()));
            prog.fail("pubkey", &e);
            return install_fail(e);
        }
        Err(e) => {
            prog.fail("pubkey", &e);
            return install_fail(e);
        }
    }
    ssh::sftp_close(&mut conn).await;

    prog.active("reconnect");
    let mut conn = match ssh::connect(&session.host, session.port, Some(&session.fingerprint)).await {
        Ok(c) => c,
        Err(e) => {
            prog.fail("reconnect", &e);
            return install_fail(e);
        }
    };
    if let Err(e) = ssh::authenticate(&mut conn, &session.user, &Auth::Dedicated).await {
        let e = SshError::new(
            "auth",
            format!(
                "the dedicated key was written but does not authenticate yet ({}) — check sshd's AuthorizedKeysFile and ~/.ssh permissions",
                e.detail
            ),
        );
        prog.fail("reconnect", &e);
        return install_fail(e);
    }
    prog.done("reconnect", None);
    ssh::sftp_close(&mut conn).await;

    prog.active("clear_pw");
    session.auth = Auth::Dedicated;
    prog.done("clear_pw", None);

    let new_host = session.host.clone();
    let new_port = session.port;
    let new_user = session.user.clone();
    let new_fp = session.fingerprint.clone();
    let (prev_host, prev_port) = prev;

    if let Err(e) = state.mutate(|store| {
        let Some(rec) = store.servers.iter_mut().find(|r| r.id == server_id) else {
            return;
        };
        rec.host = new_host.clone();
        rec.port = new_port;
        rec.user = new_user.clone();
        rec.fingerprint = new_fp.clone();
        rec.last_error = None;
        rec.fail_count = 0;

        // Move the TOFU pin with the endpoint. Drop the old host:port pin only
        // when no other registered server still uses it.
        store.known_hosts.insert(host_port_key(&new_host, new_port), new_fp.clone());
        if (prev_host != new_host || prev_port != new_port)
            && !store.servers.iter().any(|r| r.id != server_id && r.host == prev_host && r.port == prev_port)
        {
            store.known_hosts.remove(&host_port_key(&prev_host, prev_port));
        }
    }) {
        let e = SshError::new("local_io", e);
        prog.fail("clear_pw", &e);
        return install_fail(e);
    }

    if let Ok(mut due) = state.next_due.lock() {
        due.insert(server_id, Instant::now());
    }
    state.hub_send(super::hub::HubMsg::SyncNow(server_id));
    prog.finish();
    state.emit_updated(app);

    let now_ms = usage_core::report::now_ms();
    let view = state.views(now_ms).into_iter().find(|v| v.id == server_id);
    InstallOutcome {
        ok: true,
        server: view,
        detected: vec![],
        rows: 0,
        merge_pending: false,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_stable() {
        assert_eq!(STEPS.len(), 9);
        assert_eq!(STEPS[0], "keygen");
        assert_eq!(STEPS[8], "merge");
        assert_eq!(REAUTH_STEPS, ["keygen", "pubkey", "reconnect", "clear_pw"]);
    }

    #[test]
    fn auth_req_deserializes_tagged() {
        let a: AuthReq = serde_json::from_str(r#"{"kind":"password","password":"x"}"#).unwrap();
        assert!(matches!(a, AuthReq::Password { .. }));
        let b: AuthReq = serde_json::from_str(r#"{"kind":"key","path":"/k"}"#).unwrap();
        assert!(matches!(b, AuthReq::Key { .. }));
        let c: AuthReq = serde_json::from_str(r#"{"kind":"default"}"#).unwrap();
        assert!(matches!(c, AuthReq::Default));
    }

    /// The literal payloads `bridge.ts` invokes with (the `types.ts` shapes):
    /// `server_probe` `{req}` and `server_install` `{req}` must deserialize
    /// field-for-field — a camelCase drift would fail only at runtime.
    #[test]
    fn frontend_payloads_deserialize_as_typed() {
        let p: ProbeReq = serde_json::from_str(
            r#"{"name":"box","host":"127.0.0.1","port":2222,"user":"e2e","auth":{"kind":"password","password":"x"}}"#,
        )
        .unwrap();
        assert_eq!((p.name.as_str(), p.port), ("box", Some(2222)));
        assert!(matches!(p.auth, AuthReq::Password { .. }));

        let k: ProbeReq = serde_json::from_str(
            r#"{"name":"box","host":"h","user":"u","auth":{"kind":"key","path":"~/.ssh/id_ed25519","passphrase":null}}"#,
        )
        .unwrap();
        assert_eq!(k.port, None);
        match k.auth {
            AuthReq::Key { path, passphrase } => {
                assert_eq!(path, "~/.ssh/id_ed25519");
                assert!(passphrase.is_none());
            }
            other => panic!("want key auth, got {other:?}"),
        }

        let d: ProbeReq =
            serde_json::from_str(r#"{"name":"box","host":"h","user":"u","auth":{"kind":"default"}}"#)
                .unwrap();
        assert!(matches!(d.auth, AuthReq::Default));

        let i: InstallReq =
            serde_json::from_str(r#"{"session":7,"every_secs":900,"days":30,"fingerprint":"SHA256:x"}"#)
                .unwrap();
        assert_eq!((i.session, i.every_secs, i.days), (7, 900, 30));

        let r: ReauthProbeReq = serde_json::from_str(
            r#"{"id":3,"host":"203.0.113.10","port":22,"user":"root","auth":{"kind":"password","password":"x"}}"#,
        )
        .unwrap();
        assert_eq!((r.id, r.host.as_deref(), r.port, r.user.as_deref()), (3, Some("203.0.113.10"), Some(22), Some("root")));
        assert!(matches!(r.auth, AuthReq::Password { .. }));
        assert!(!r.ack_mismatch);

        let r: ReauthProbeReq = serde_json::from_str(
            r#"{"id":3,"auth":{"kind":"default"},"ack_mismatch":true,"expected_fingerprint":"SHA256:x"}"#,
        )
        .unwrap();
        assert!(r.host.is_none() && r.port.is_none() && r.user.is_none());
        assert!(r.ack_mismatch);
        assert_eq!(r.expected_fingerprint.as_deref(), Some("SHA256:x"));

        let a: ReauthApplyReq =
            serde_json::from_str(r#"{"session":9,"fingerprint":"SHA256:x"}"#).unwrap();
        assert_eq!((a.session, a.fingerprint.as_str()), (9, "SHA256:x"));
    }

    #[test]
    fn auth_debug_never_leaks_the_password() {
        let auth = Auth::Password(zeroize::Zeroizing::new("hunter2".to_string()));
        let dbg = format!("{auth:?}");
        assert!(!dbg.contains("hunter2"));
        assert!(dbg.contains("***"));
    }
}

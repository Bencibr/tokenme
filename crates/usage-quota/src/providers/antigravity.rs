//! Antigravity: the quota the vendor's own language server already computes for
//! its IDE, read from the local Connect RPC that server exposes.
//!
//! ## Why this attaches to the running server instead of running the CLI
//!
//! `agy`'s auth is a three-step chain (its own logs, this machine): the file
//! credential the IDE maintains (`~/.gemini/jetski-standalone-oauth-token`) is
//! *rejected* for CLI use, the CLI's real credential lives in the keyring, and
//! when both fail print mode falls back to interactive OAuth — it opens the
//! browser (`printmode.go:395 → browser.go:56`). The IDE and the CLI hold
//! independent credentials, so "the user is logged in" (the IDE works) does not
//! stop the CLI from demanding a browser login: a broken keyring night here
//! produced 72 browser flows at the poll cadence before this probe grew a
//! circuit breaker.
//!
//! The language server the IDE launches, though, is already logged in and
//! already serves the quota to its own UI:
//!
//! ```text
//! POST {http,https}://127.0.0.1:<port>/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary
//! Connect-Protocol-Version: 1
//! X-Codeium-Csrf-Token: <the server's own --csrf_token command-line flag>
//! {"forceRefresh": true}
//! ```
//!
//! — the protocol `steipete/CodexBar`'s `AntigravityStatusProbe.swift` maps out.
//! The CSRF token sits in the server's process arguments, the ports in its
//! socket table; we never touch tokens, never refresh anything, and
//! structurally cannot trigger a login. Only when no server is discoverable
//! does this probe fall back to the vendor CLI (`agy -p /usage`), and that
//! fallback is the one step able to pop a browser, so it sits behind
//! [`SPAWN_BREAKER`]. (Refreshing on our own was also rejected: Google can
//! rotate the `refresh_token` on refresh, and a probe that refreshes without
//! persisting the new one orphans the login — the trap every third-party
//! refresher dodges by writing back, `orrisroot/agy-usage:client.rs:256-258`.)

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::QuotaProbe;

pub struct AntigravityQuota;

/// The CLI's own window names, and the one hard requirement for the JSON shape:
/// `/usage --output-format json` only exists from 1.1.11 (`CodexBar`
/// `AgyProvider.swift:186-193` gates on it the same way).
const WINDOW_MINUTES: [(&str, i64); 2] = [("weekly", 10_080), ("5h", 300)];
/// Measured on this machine: 10.4 s warm, and a run that needed the full
/// `--print-timeout` was killed at 12 s. The wait has to outlast the CLI's own
/// budget or the answer is thrown away, so this sits above its 10 s print timeout
/// and below the 30 s a one-shot CLI command is willing to spend.
const RUN_BUDGET: Duration = Duration::from_secs(25);

/// Consecutive fruitless `agy` runs before the fallback stops being tried.
const BREAKER_THRESHOLD: usize = 2;
/// How long the fallback stays switched off after [`BREAKER_THRESHOLD`].
/// Thirty minutes turns "a browser popup every poll" into "two popups, then
/// silence the panel recovers from on its own".
const BREAKER_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// The local Connect RPC path the language server serves (`CodexBar`
/// `AntigravityStatusProbe.swift:847-852`).
const QUOTA_SUMMARY_PATH: &str = "/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary";
/// Local calls are answered in milliseconds; the budget only exists so a
/// wedged server cannot eat the probe thread.
const LOCAL_CONNECT: Duration = Duration::from_secs(3);
const LOCAL_READ: Duration = Duration::from_secs(6);
/// A desktop install runs one language server; two covers an app + IDE pair.
const MAX_SERVERS: usize = 2;
const MAX_PORTS: usize = 4;

impl QuotaProbe for AntigravityQuota {
    fn tool(&self) -> &'static str {
        "antigravity"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        // Attach first: the running server is logged in already, and a local
        // HTTP call cannot trigger any auth flow.
        if let Some(samples) = attach() {
            if !samples.is_empty() {
                return samples;
            }
        }
        // Fallback: the vendor CLI. The one step here that can pop a browser,
        // so the breaker decides whether it runs at all.
        if !SPAWN_BREAKER.allows() {
            return Vec::new();
        }
        let parsed = run_usage().and_then(|body| serde_json::from_str(&body).ok());
        match parsed.map(|v: Value| samples_from(&v)).filter(|s| !s.is_empty()) {
            Some(samples) => {
                SPAWN_BREAKER.note_success();
                samples
            }
            None => {
                SPAWN_BREAKER.note_failure();
                Vec::new()
            }
        }
    }
}

/// The fallback circuit breaker: `BREAKER_THRESHOLD` consecutive fruitless
/// `agy` runs switch spawns off for [`BREAKER_BACKOFF`]; any answered run
/// resets it. Times are injected so the states are testable without waiting
/// out the backoff.
struct SpawnBreaker {
    failures: AtomicUsize,
    blocked_until: Mutex<Option<Instant>>,
}

impl SpawnBreaker {
    const fn new() -> Self {
        Self {
            failures: AtomicUsize::new(0),
            blocked_until: Mutex::new(None),
        }
    }

    fn allows(&self) -> bool {
        self.allows_at(Instant::now())
    }

    fn allows_at(&self, now: Instant) -> bool {
        match *self.slot() {
            Some(until) => now >= until,
            None => true,
        }
    }

    fn note_failure(&self) {
        self.note_failure_at(Instant::now());
    }

    fn note_failure_at(&self, now: Instant) {
        if self.failures.fetch_add(1, Ordering::Relaxed) + 1 >= BREAKER_THRESHOLD {
            *self.slot() = Some(now + BREAKER_BACKOFF);
        }
    }

    fn note_success(&self) {
        self.failures.store(0, Ordering::Relaxed);
        *self.slot() = None;
    }

    /// The lock can be poisoned by a panic between an expiry write and a read;
    /// a broken breaker state must fail open, not take the probe down.
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.blocked_until.lock().unwrap_or_else(|e| e.into_inner())
    }
}

static SPAWN_BREAKER: SpawnBreaker = SpawnBreaker::new();

// ---------------------------------------------------------------------------
// Attach: the running language server
// ---------------------------------------------------------------------------

/// A discovered Antigravity language server process and, for desktop installs,
/// the CSRF token its own command line carries. A tokenless desktop match is
/// not discovered at all: the server would 401 every call (`CodexBar` skips
/// those the same way), so only the CLI's server may go without. The Debug
/// impl redacts the token — test output must not leak it.
struct LanguageServer {
    pid: u32,
    csrf: Option<String>,
}

impl std::fmt::Debug for LanguageServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanguageServer")
            .field("pid", &self.pid)
            .field("csrf", &self.csrf.is_some())
            .finish()
    }
}

#[cfg(unix)]
fn attach() -> Option<Vec<QuotaSample>> {
    let servers = find_language_servers()?;
    for server in servers.iter().take(MAX_SERVERS) {
        for port in listening_ports(server.pid).into_iter().take(MAX_PORTS) {
            for scheme in ["http", "https"] {
                let Some(value) = quota_summary(server, port, scheme) else { continue };
                let samples = samples_from(&value);
                if !samples.is_empty() {
                    return Some(samples);
                }
            }
        }
    }
    None
}

/// `ps -axo pid=,command=`, classified the way `CodexBar`
/// `antigravityProcessKind` classifies: a `language_server` binary inside an
/// Antigravity/Gemini path (desktop app, IDE extension), or the CLI's own
/// server. Desktop matches without a `--csrf_token` are skipped so a server
/// that would 401 is never hammered; the CLI's server needs no token.
#[cfg(unix)]
fn find_language_servers() -> Option<Vec<LanguageServer>> {
    let output = Command::new("ps").args(["-axo", "pid=,command="]).output().ok()?;
    let mut servers = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.trim_start().splitn(2, char::is_whitespace);
        // A malformed line is skipped, not fatal: one odd `ps` row must not
        // end discovery.
        let Some(pid_text) = parts.next() else { continue };
        let Ok(pid) = pid_text.parse::<u32>() else { continue };
        let Some(command) = parts.next() else { continue };
        let lower = command.to_lowercase();
        let desktop = is_desktop_language_server(&lower);
        let cli = !desktop && is_cli_language_server(&lower);
        if !desktop && !cli {
            continue;
        }
        let csrf = flag_value(command, "--csrf_token");
        if desktop && csrf.is_none() {
            continue;
        }
        servers.push(LanguageServer { pid, csrf });
    }
    (!servers.is_empty()).then_some(servers)
}

/// A `language_server` executable inside an Antigravity/Gemini desktop path or
/// carrying the desktop's `--app_data_dir antigravity` flag. The path markers
/// need their separators so unrelated names ("notantigravity/") cannot match.
#[cfg(unix)]
fn is_desktop_language_server(lower: &str) -> bool {
    if !["/language_server", "/language-server", "\\language_server", "\\language-server"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return false;
    }
    lower.contains("--app_data_dir antigravity")
        || lower.contains("--app_data_dir=antigravity")
        || lower.contains("antigravity.app/")
        || lower.contains("antigravity ide.app/")
        || lower.contains("/antigravity/")
        || lower.contains("gemini.app/")
}

/// The CLI hosts the same server under its own name (`agy`,
/// `antigravity-cli/`) and needs no CSRF token.
#[cfg(unix)]
fn is_cli_language_server(lower: &str) -> bool {
    if lower.contains("/antigravity-cli/") || lower.contains("\\antigravity-cli\\") || lower.contains("/antigravity_cli/") {
        return true;
    }
    lower
        .split_whitespace()
        .next()
        .map(|exe| {
            let base = exe.rsplit(['/', '\\']).next().unwrap_or(exe);
            base == "agy" || base == "agy.exe"
        })
        .unwrap_or(false)
}

/// The value of a `--flag value` or `--flag=value` token, scanning whole argv —
/// `ps` command lines can carry unrelated text, so a plain substring match is
/// not enough.
#[cfg(unix)]
fn flag_value(command: &str, flag: &str) -> Option<String> {
    let mut tokens = command.split_whitespace();
    while let Some(token) = tokens.next() {
        if let Some(value) = token.strip_prefix(flag).and_then(|rest| rest.strip_prefix('=')) {
            return Some(value.to_string());
        }
        if token == flag {
            return tokens.next().map(|value| value.to_string());
        }
    }
    None
}

/// `lsof -a -p <pid> -iTCP -sTCP:LISTEN -P -n`, loopback ports only.
#[cfg(unix)]
fn listening_ports(pid: u32) -> Vec<u16> {
    let Ok(output) = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-iTCP", "-sTCP:LISTEN", "-P", "-n"])
        .output()
    else {
        return Vec::new();
    };
    let mut ports = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if !line.ends_with("(LISTEN)") {
            continue;
        }
        let Some(name) = line.split_whitespace().find(|t| t.contains(':')) else { continue };
        if !(name.starts_with("127.0.0.1:") || name.starts_with("[::1]:") || name.starts_with("localhost:")) {
            continue;
        }
        if let Ok(port) = name.rsplit(':').next().unwrap_or_default().parse::<u16>() {
            if port != 0 && !ports.contains(&port) {
                ports.push(port);
            }
        }
    }
    ports
}

/// One Connect RPC call. The desktop server answers over TLS with a
/// certificate only it signed, and usually also over plain HTTP on its second
/// port; the loopback address makes both safe to try.
#[cfg(unix)]
fn quota_summary(server: &LanguageServer, port: u16, scheme: &str) -> Option<Value> {
    let url = format!("{scheme}://127.0.0.1:{port}{QUOTA_SUMMARY_PATH}");
    let agent = match scheme {
        "https" => https_agent(),
        _ => http_agent(),
    };
    let mut request = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set("Connect-Protocol-Version", "1");
    if let Some(csrf) = &server.csrf {
        request = request.set("X-Codeium-Csrf-Token", csrf);
    }
    let response = request.send_json(serde_json::json!({ "forceRefresh": true })).ok()?;
    if response.status() != 200 {
        return None;
    }
    response.into_json().ok()
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| ureq::AgentBuilder::new().timeout_connect(LOCAL_CONNECT).timeout_read(LOCAL_READ).build())
}

/// The language server's TLS certificate is self-signed; the loopback address
/// is the trust anchor here, not the certificate, so the verifier accepts
/// anything the local socket presents.
#[cfg(unix)]
fn https_agent() -> &'static ureq::Agent {
    use std::sync::Arc;
    use ureq::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use ureq::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use ureq::rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

    #[derive(Debug)]
    struct AcceptLoopbackCert;

    impl ServerCertVerifier for AcceptLoopbackCert {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, TlsError> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, TlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, TlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            use ureq::rustls::SignatureScheme::*;
            vec![
                RSA_PSS_SHA256,
                RSA_PSS_SHA384,
                RSA_PSS_SHA512,
                RSA_PKCS1_SHA256,
                RSA_PKCS1_SHA384,
                RSA_PKCS1_SHA512,
                ECDSA_NISTP256_SHA256,
                ECDSA_NISTP384_SHA384,
                ECDSA_NISTP521_SHA512,
                ED25519,
            ]
        }
    }

    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        let verifier: Arc<dyn ServerCertVerifier> = Arc::new(AcceptLoopbackCert);
        // ureq speaks HTTP/1.1; without the ALPN pin a server could negotiate
        // h2 and leave ureq staring at a binary stream.
        let mut config = ureq::rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        ureq::AgentBuilder::new()
            .tls_config(Arc::new(config))
            .timeout_connect(LOCAL_CONNECT)
            .timeout_read(LOCAL_READ)
            .build()
    })
}

// ---------------------------------------------------------------------------
// Fallback: the vendor CLI
// ---------------------------------------------------------------------------

/// Locate the binary the same way the installer does: `~/.local/bin/agy`, else PATH.
fn binary() -> Option<PathBuf> {
    if let Some(home) = dirs::home_dir() {
        let candidate = home.join(".local").join("bin").join("agy");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    which("agy")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        #[cfg(windows)]
        for extension in std::env::var_os("PATHEXT")
            .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_else(|| vec![PathBuf::from(".EXE")])
        {
            let extension = extension.to_string_lossy();
            let candidate = dir.join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// `{}` on every failure path: no CLI, no network, an old CLI, a timeout.
fn run_usage() -> Option<String> {
    let bin = binary()?;
    let mut command = Command::new(bin);
    command
        .args(["-p", "/usage", "--output-format", "json", "--print-timeout", "10s"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // The probe runs from the GUI bar and agy is a console binary: without
    // CREATE_NO_WINDOW Windows gives it a visible console for the CLI's whole
    // print budget, once per TTL. Non-Windows targets have no such flag.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + RUN_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let mut buf = Vec::new();
    use std::io::Read as _;
    child.stdout.as_mut().and_then(|s| s.read_to_end(&mut buf).ok())?;
    let text = String::from_utf8_lossy(&buf).to_string();
    (!text.is_empty()).then_some(text)
}

// ---------------------------------------------------------------------------
// Parsing: both the local RPC and the CLI answer
// ---------------------------------------------------------------------------

/// `remaining_fraction` is the share *left*, so it inverts. A bucket without one is
/// unknown, not free: protobuf drops a zero, so the field is simply absent.
///
/// Two envelopes carry the same groups: the local RPC answers
/// `{"response":{"groups":[...]}}` with camelCase fields, the CLI's
/// `--output-format json` answers `{"status":"SUCCESS","command":{"data":{
/// "groups":[...]}}}` with snake_case. Both are read.
pub(crate) fn samples_from(value: &Value) -> Vec<QuotaSample> {
    let Some(groups) = groups_of(value) else { return Vec::new() };
    let mut out = Vec::new();
    for group in groups.iter().filter_map(Value::as_object) {
        let name = str_field(group, &["displayName", "name"]).unwrap_or_else(|| "Antigravity".to_string());
        let Some(buckets) = group.get("buckets").and_then(Value::as_array) else { continue };
        let mut rows: Vec<QuotaSample> = buckets
            .iter()
            .filter_map(Value::as_object)
            .filter(|bucket| !bucket.get("disabled").and_then(Value::as_bool).unwrap_or(false))
            .filter_map(|bucket| {
                let remaining = num_field(bucket, &["remainingFraction", "remaining_fraction"])?;
                if !(0.0..=1.0).contains(&remaining) {
                    return None;
                }
                let window = str_field(bucket, &["window"]).unwrap_or_default();
                let minutes = WINDOW_MINUTES.iter().find(|(k, _)| *k == window).map(|(_, m)| *m).unwrap_or(0);
                Some(QuotaSample {
                    used_percent: (1.0 - remaining) * 100.0,
                    window_minutes: minutes,
                    resets_at_ms: str_field(bucket, &["resetTime", "reset_time"])
                        .and_then(|t| parse_ts_ms(&t))
                        .unwrap_or(0),
                    label: Some(match str_field(bucket, &["displayName", "name"]) {
                        Some(n) if !n.is_empty() => format!("{} · {}", group_label(&name), short(&n, &window)),
                        _ => format!("{} · {window}", group_label(&name)),
                    }),
                    id: None,
                })
            })
            .collect();
        // The vendor groups by model and answers with a 5-hour and a weekly
        // window per group; keep the groups in the vendor's own order and the
        // windows shortest-first inside a group, so the panel can show one
        // model's two windows together instead of interleaving the models.
        rows.sort_by_key(|s| s.window_minutes);
        out.extend(rows);
    }
    out
}

/// The RPC wraps in `response`, the CLI in `command.data`.
fn groups_of(value: &Value) -> Option<&Vec<Value>> {
    value
        .pointer("/response/groups")
        .or_else(|| value.pointer("/command/data/groups"))
        .and_then(Value::as_array)
}

fn str_field<'a>(object: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| object.get(*k).and_then(Value::as_str).map(str::to_string))
}

fn num_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| object.get(*k).and_then(Value::as_f64))
}

/// The vendor names its buckets "Weekly Limit Remaining" — a *remaining* share —
/// while this bar shows the used share, so "剩余" would read backwards. Keep the
/// vendor's group name (it is what the IDE shows) and label the window only.
fn short(name: &str, window: &str) -> String {
    match window {
        "weekly" => "周",
        "5h" => "5 小时",
        _ => return name.to_string(),
    }
    .to_string()
}

/// The CLI's group names are sentences ("Claude and GPT models"); the panel has
/// ~130 px for the whole label, so groups get their shortest honest form.
/// Named `group_label` because the loop below binds `group`.
fn group_label(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains("gemini") {
        "Gemini".to_string()
    } else if lower.contains("claude") && lower.contains("gpt") {
        "Claude/GPT".to_string()
    } else {
        name.split_whitespace().next().unwrap_or(name).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(json: &str) -> Value {
        serde_json::from_str(json).unwrap_or_else(|e| panic!("fixture parses: {e}"))
    }

    /// What the local Connect RPC answered on this machine, field names and all.
    const RPC: &str = r#"{
      "response":{"groups":[
        {"displayName":"Gemini Models","buckets":[
          {"bucketId":"gemini-weekly","displayName":"Weekly Limit Remaining","window":"weekly","remainingFraction":0.33282027,"resetTime":"2026-10-05T11:30:16Z"},
          {"bucketId":"gemini-5h","displayName":"Five Hour Limit Remaining","window":"5h","remainingFraction":0.858642,"resetTime":"2026-09-30T10:46:35Z"}]},
        {"displayName":"Claude and GPT models","buckets":[
          {"bucketId":"3p-weekly","displayName":"Weekly Limit Remaining","window":"weekly","remainingFraction":1,"resetTime":"2026-10-07T06:55:14Z"},
          {"bucketId":"3p-5h","displayName":"Five Hour Limit Remaining","window":"5h","remainingFraction":1,"resetTime":"2026-09-30T11:55:14Z"}]}
      ]}}"#;

    /// Trimmed copy of what `agy -p /usage --output-format json` answers on this
    /// machine, field names and all.
    const LIVE: &str = r#"{
      "status":"SUCCESS","num_turns":0,
      "usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0},
      "command":{"name":"usage","data":{"description":"Within each group...","groups":[
        {"name":"Gemini Models","buckets":[
          {"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":0.9499499797821045,"reset_time":"2026-09-30T03:34:13Z"},
          {"id":"gemini-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":0.7587900161743164,"reset_time":"2026-09-23T20:16:54Z"}]},
        {"name":"Claude and GPT models","buckets":[
          {"id":"other-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":1.0,"reset_time":"2026-09-30T16:12:03Z"},
          {"id":"other-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":1.0,"reset_time":"2026-09-23T21:12:03Z"}]}
      ]}}}"#;

    #[test]
    fn the_local_rpc_answer_parses_like_the_cli_answer() {
        for raw in [RPC, LIVE] {
            let s = samples_from(&body(raw));
            assert_eq!(s.len(), 4, "{raw}");
            assert_eq!(s[0].window_minutes, 300, "5-hour windows sort first");
            assert_eq!(s[0].label.as_deref(), Some("Gemini · 5 小时"));
            assert_eq!(s[1].label.as_deref(), Some("Gemini · 周"));
            assert_eq!(s[1].window_minutes, 10_080);
            assert_eq!(s[2].label.as_deref(), Some("Claude/GPT · 5 小时"));
            assert!(s.iter().all(|x| x.resets_at_ms > 1_790_000_000_000), "{s:?}");
            assert!(s.iter().all(|x| x.used_percent >= 0.0 && x.used_percent <= 100.0));
        }
        let rpc = samples_from(&body(RPC));
        assert!((rpc[0].used_percent - 14.14).abs() < 0.01, "1 - 0.8586: {:?}", rpc[0]);
        assert!((rpc[1].used_percent - 66.72).abs() < 0.01, "1 - 0.3328: {:?}", rpc[1]);
        let live = samples_from(&body(LIVE));
        assert!((live[0].used_percent - 24.12).abs() < 0.01, "1 - 0.7588: {:?}", live[0]);
    }

    #[test]
    fn an_unused_bucket_is_still_a_zero_bar() {
        let s = samples_from(&body(LIVE));
        let untouched = s.iter().filter(|x| (x.used_percent - 0.0).abs() < 1e-9).count();
        assert_eq!(untouched, 2, "the Claude and GPT group is untouched");
    }

    #[test]
    fn missing_impossible_and_disabled_buckets_are_dropped_not_invented() {
        let s = samples_from(&body(
            r#"{"response":{"groups":[{"displayName":"G","buckets":[
                 {"bucketId":"a","window":"weekly","remainingFraction":0.5},
                 {"bucketId":"b","window":"5h","remainingFraction":3.5},
                 {"bucketId":"c","window":"5h","remainingFraction":0.2,"disabled":true},
                 {"bucketId":"d","window":"5h"},
                 {"bucketId":"e","window":"weird","displayName":"Odd","remainingFraction":0.25} ]}]}}"#,
        ));
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[0].used_percent, 75.0, "an unknown window sorts first and stays unlabelled");
        assert_eq!(s[0].window_minutes, 0);
        assert_eq!(s[0].label.as_deref(), Some("G · Odd"), "the vendor's own bucket name is all we have");
        assert_eq!(s[1].used_percent, 50.0);
        assert_eq!(s[1].window_minutes, 10_080);
        assert_eq!(s[1].label.as_deref(), Some("G · weekly"), "a bucket with no name shows the raw window");
        assert_eq!(s[1].resets_at_ms, 0, "a missing reset_time is unknown, not now");
    }

    #[test]
    fn anything_other_than_success_is_nothing() {
        for raw in [
            r#"{"status":"ERROR","command":{"data":{"groups":[]}}}"#,
            r#"{"status":"SUCCESS"}"#,
            r#"{"status":"SUCCESS","command":{"data":{"groups":null}}}"#,
            r#"{"response":{}}"#,
            r#"{"code":"unauthenticated","message":"invalid CSRF token"}"#,
            "not json at all",
        ] {
            let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            assert!(samples_from(&v).is_empty(), "{raw} must answer nothing");
        }
    }

    // -- process classification -------------------------------------------

    /// The real command line of the language server on this machine (token
    /// replaced), as `ps -axo pid=,command=` reports it.
    const PS_COMMAND: &str = "/Applications/Antigravity.app/Contents/Resources/bin/language_server --standalone --override_ide_name antigravity --subclient_type hub --csrf_token 0123456789abcdef --app_data_dir antigravity";

    /// The same classify-and-resolve rule `find_language_servers` applies.
    fn classify(command: &str) -> Option<(bool, Option<String>)> {
        let lower = command.to_lowercase();
        let desktop = is_desktop_language_server(&lower);
        let cli = !desktop && is_cli_language_server(&lower);
        if !desktop && !cli {
            return None;
        }
        let csrf = flag_value(command, "--csrf_token");
        if desktop && csrf.is_none() {
            return None;
        }
        Some((desktop, csrf))
    }

    #[test]
    fn the_desktop_server_is_found_with_its_token() {
        let (desktop, csrf) = classify(PS_COMMAND).expect("the real command line classifies");
        assert!(desktop);
        assert_eq!(csrf.as_deref(), Some("0123456789abcdef"));
    }

    #[test]
    fn a_tokenless_desktop_server_is_skipped() {
        let command = "/Applications/Antigravity.app/Contents/Resources/bin/language_server --standalone --app_data_dir antigravity";
        assert_eq!(classify(command), None, "it would 401 every call");
    }

    #[test]
    fn the_cli_server_is_found_without_a_token() {
        let (desktop, csrf) = classify("/Users/me/.local/bin/agy language-server --port 8080")
            .expect("the CLI's server classifies");
        assert!(!desktop);
        assert_eq!(csrf, None, "the CLI needs no CSRF token");
    }

    #[test]
    fn an_unrelated_server_is_ignored() {
        assert_eq!(classify("/usr/local/bin/language_server --port 9"), None);
        assert_eq!(classify("/usr/libexec/logd"), None);
        assert_eq!(
            classify("/opt/notantigravity/bin/language_server --csrf_token x"),
            None,
            "the path markers need their separators"
        );
    }

    #[test]
    fn the_csrf_flag_parses_in_both_spellings() {
        assert_eq!(flag_value("server --csrf_token ABC --x", "--csrf_token").as_deref(), Some("ABC"));
        assert_eq!(flag_value("server --csrf_token=ABC", "--csrf_token").as_deref(), Some("ABC"));
        assert_eq!(flag_value("server --other", "--csrf_token"), None);
    }

    // -- lsof parsing ------------------------------------------------------

    #[test]
    fn loopback_listen_lines_yield_their_ports_in_order() {
        let lsof = "COMMAND   PID USER   FD   TYPE NODE NAME\n\
                    language_ 123 sp    7u  IPv4 TCP 127.0.0.1:57939 (LISTEN)\n\
                    language_ 123 sp    8u  IPv4 TCP [::1]:57940 (LISTEN)\n\
                    language_ 123 sp    9u  IPv4 TCP *:9000 (LISTEN)\n\
                    language_ 123 sp   10u  IPv4 TCP 10.0.0.2:9100 (LISTEN)\n";
        let ports: Vec<u16> = lsof
            .lines()
            .filter(|l| l.ends_with("(LISTEN)"))
            .filter_map(|l| l.split_whitespace().find(|t| t.contains(':')))
            .filter(|n| n.starts_with("127.0.0.1:") || n.starts_with("[::1]:") || n.starts_with("localhost:"))
            .filter_map(|n| n.rsplit(':').next().unwrap_or_default().parse::<u16>().ok())
            .filter(|p| *p != 0)
            .collect();
        assert_eq!(ports, vec![57939, 57940], "non-loopback listeners never surface");
    }

    // -- the breaker -------------------------------------------------------

    #[test]
    fn two_fruitless_spawns_block_the_fallback_and_an_answer_resets_it() {
        let breaker = SpawnBreaker::new();
        let now = Instant::now();
        assert!(breaker.allows_at(now));
        breaker.note_failure_at(now);
        assert!(breaker.allows_at(now), "one failure is not enough");
        breaker.note_failure_at(now);
        assert!(!breaker.allows_at(now), "two failures trip the breaker");
        assert!(breaker.allows_at(now + BREAKER_BACKOFF), "the backoff expires");
        breaker.note_success();
        assert!(breaker.allows_at(now), "an answered run resets everything");
    }

    // -- live (opt-in) ------------------------------------------------------

    /// Talks to the real language server running on this machine.
    #[test]
    #[ignore = "needs the Antigravity language server running locally"]
    fn the_running_server_answers_without_a_subprocess() {
        let Some(servers) = find_language_servers() else {
            println!("[antigravity] no language server process found");
            return;
        };
        for server in &servers {
            println!("[antigravity] pid {} csrf={}", server.pid, server.csrf.is_some());
            for port in listening_ports(server.pid) {
                println!("[antigravity] port {port}");
                for scheme in ["http", "https"] {
                    let Some(value) = quota_summary(server, port, scheme) else { continue };
                    let s = samples_from(&value);
                    println!("[antigravity] {scheme} answered {} windows", s.len());
                    assert!(!s.is_empty(), "a logged-in server always has windows");
                    return;
                }
            }
        }
        panic!("servers found but none answered: {servers:?}");
    }
}

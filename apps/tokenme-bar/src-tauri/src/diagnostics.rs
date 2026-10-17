//! The Tauri-side diagnostics boundary.
//!
//! Transport, grant signing, and MCP JSON-RPC framing live outside this app
//! slice. This module owns the local fail-closed session gate and translates
//! the fixed protocol tool names into existing Tauri handlers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::sync::{atomic::{AtomicBool, AtomicI64, Ordering}, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;
use rand::RngExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::{self, SyncManagementRequest, SyncDevicesView, SyncControl};
use crate::{logging, panel};

pub const CODE_DIAGNOSTICS_DISABLED: &str = "diagnostics_disabled";
pub const CODE_SESSION_NOT_FOUND: &str = "session_not_found";
pub const CODE_SESSION_EXPIRED: &str = "session_expired";
pub const CODE_SESSION_REVOKED: &str = "session_revoked";
pub const CODE_ORIGIN_MISMATCH: &str = "origin_mismatch";
pub const CODE_TARGET_MISMATCH: &str = "target_mismatch";
pub const CODE_SCOPE_DENIED: &str = "scope_denied";
pub const CODE_UNKNOWN_TOOL: &str = "unknown_tool";
pub const CODE_INVALID_PARAMS: &str = "invalid_params";
pub const CODE_HANDLER_FAILED: &str = "handler_failed";
pub const CODE_OPERATION_NOT_FOUND: &str = "operation_not_found";
pub const CODE_TOOL_UNAVAILABLE: &str = "tool_unavailable";
pub const CODE_PRIMARY_UNAVAILABLE: &str = "primary_unavailable";
pub const CODE_GRANT_FAILED: &str = "grant_failed";

const MAX_SESSION_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MAX_PARAMS_BYTES: usize = 32 * 1024;
const MAX_OPERATION_HISTORY: usize = 64;
const LOCAL_MCP_PATH: &str = "/mcp";
const LOCAL_MCP_PORT: u16 = 47822;
const LOCAL_MCP_MAX_HEADER_BYTES: usize = 64 * 1024;
const LOCAL_MCP_MAX_BODY_BYTES: usize = 256 * 1024;
const LOCAL_MCP_TOKEN_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const LOCAL_MCP_MAX_AUTH_BYTES: usize = 256;
const LOCAL_MCP_MAX_CONNECTIONS: usize = 4;
const LOCAL_MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

fn infer_local_lan_host() -> Result<String, String> {
    // Connecting a UDP socket only asks the OS which interface it would use;
    // it does not send a packet.  Prefer IPv4 because the listener below is
    // deliberately IPv4 and this is the address other LAN clients can paste.
    let socket = UdpSocket::bind(("0.0.0.0", 0)).map_err(|error| format!("无法读取局域网地址: {error}"))?;
    socket.connect(("8.8.8.8", 80)).map_err(|error| format!("无法确定局域网地址: {error}"))?;
    let address = socket.local_addr().map_err(|error| format!("无法读取局域网地址: {error}"))?.ip();
    match address {
        IpAddr::V4(address) if !address.is_loopback() && !address.is_unspecified() => Ok(address.to_string()),
        _ => Err("无法确定可供局域网访问的 IPv4 地址；请先连接 Wi‑Fi 或以太网后再开启 MCP".into()),
    }
}

fn new_local_mcp_token() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    format!("tm_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference: u8 = if left.len() == right.len() { 0 } else { 1 };
    for index in 0..left.len().max(right.len()) {
        difference |= left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0);
    }
    difference == 0
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiagnosticsError {
    pub code: &'static str,
    pub message: &'static str,
}

impl DiagnosticsError {
    fn new(code: &'static str, message: &'static str) -> Self { Self { code, message } }
}

impl std::fmt::Display for DiagnosticsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DiagnosticsError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Read,
    Control,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::Read => "diagnostics.read",
            Self::Control => "sync.control",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SessionRequest {
    pub session_id: String,
    pub target_device: String,
    pub origin: String,
    pub scopes: Vec<String>,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionView {
    pub session_id: String,
    pub target_device: String,
    pub origin: String,
    pub scopes: Vec<String>,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiagnosticsStatus {
    pub enabled: bool,
    pub active_sessions: usize,
    pub retained_operations: usize,
}

/// The state returned to the panel.  Session values are intentionally bounded
/// metadata only; the exported config never contains logs, credentials, or
/// device usage data.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiagnosticsSnapshot {
    pub enabled: bool,
    pub sessions: Vec<SessionView>,
    pub retained_operations: usize,
    pub lan_control_enabled: bool,
    pub lan_control_expires_at_ms: Option<i64>,
    pub last_operation: Option<DiagnosticsOperationView>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiagnosticsOperationView {
    pub operation_id: String,
    pub action: String,
    pub state: String,
    pub at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiagnosticsConfig {
    pub schema: String,
    pub endpoint: String,
    /// TLS verification name for the self-issued certificate.  LAN clients
    /// connect to the advertised endpoint host, but the certificate remains
    /// issued for localhost by the primary certificate generator.
    pub server_name: String,
    pub certificate_pin: String,
    pub certificate_der_b64: String,
    pub grant: usage_device_sync::DiagnosticsGrant,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiagnosticsResponse {
    #[serde(flatten)]
    pub snapshot: DiagnosticsSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<DiagnosticsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DiagnosticsErrorView>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiagnosticsErrorView {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DiagnosticsManagementRequest {
    Status,
    Configure { enabled: bool, ttl_ms: Option<i64> },
    ConfigureLanControl { enabled: bool },
    CreateGrant { debugger_device_id: String, scopes: Vec<String>, ttl_ms: i64 },
    RevokeGrant { grant_id: String },
    RevokeAll,
}

#[derive(Debug, Clone)]
struct GrantSession {
    view: SessionView,
}

#[derive(Debug, Clone)]
struct Session {
    view: SessionView,
    scopes: BTreeSet<String>,
    revoked: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
struct OperationView {
    operation_id: String,
    session_id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'static str>,
}

#[derive(Debug, Default)]
struct Inner {
    enabled: bool,
    next_operation: u64,
    next_management_operation: u64,
    sessions: BTreeMap<String, Session>,
    grants: BTreeMap<String, GrantSession>,
    operations: BTreeMap<String, OperationView>,
    last_operation: Option<DiagnosticsOperationView>,
}

#[derive(Debug)]
struct LocalMcpHttpServer {
    endpoint: String,
    read_token: String,
    expires_at_ms: i64,
    /// The lifetime this token was minted with; a Configure carrying a
    /// different ttl must re-mint — the old listener keeps the old clock.
    ttl_ms: i64,
    /// One credential, one switch: on, this credential IS the LAN control
    /// auth; off, the very same credential is read-only.
    control_enabled: bool,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Scope {
    fn local_origin(self) -> &'static str {
        match self { Self::Read => "mcp-http", Self::Control => "mcp-http-control" }
    }
    fn local_scopes(self) -> Vec<String> {
        match self { Self::Read => vec!["diagnostics.read".into()], Self::Control => vec!["diagnostics.read".into(), "sync.control".into()] }
    }
}

#[derive(Debug, Default)]
pub struct DiagnosticsState {
    inner: Mutex<Inner>,
    local_http: Mutex<Option<LocalMcpHttpServer>>,
    /// Kept for management-request compatibility; local MCP credentials always
    /// use the fixed seven-day lifetime below.
    local_token_ttl_ms: AtomicI64,
}

const LOCAL_TOKEN_TTL_MIN_MS: i64 = LOCAL_MCP_TOKEN_TTL_MS;
const LOCAL_TOKEN_TTL_MAX_MS: i64 = LOCAL_MCP_TOKEN_TTL_MS;

impl DiagnosticsState {
    fn local_token_ttl(&self) -> i64 {
        let stored = self.local_token_ttl_ms.load(Ordering::Relaxed);
        if stored == 0 { LOCAL_MCP_TOKEN_TTL_MS } else { stored.clamp(LOCAL_TOKEN_TTL_MIN_MS, LOCAL_TOKEN_TTL_MAX_MS) }
    }

    fn set_local_token_ttl(&self, ttl_ms: Option<i64>) {
        if let Some(ttl) = ttl_ms {
            // The old API accepted a requested TTL; normalize it so no caller
            // can mint a shorter-lived MCP Bearer.
            self.local_token_ttl_ms.store(ttl.clamp(LOCAL_TOKEN_TTL_MIN_MS, LOCAL_TOKEN_TTL_MAX_MS), Ordering::Relaxed);
        }
    }

    /// True when a live listener was minted with a lifetime different from the
    /// one now requested; the caller stops it so the next start re-mints.
    fn local_http_ttl_changed(&self) -> bool {
        self.local_http.lock().ok()
            .and_then(|server| server.as_ref().map(|server| now_ms() < server.expires_at_ms && server.ttl_ms != self.local_token_ttl()))
            .unwrap_or(false)
    }

    pub(crate) fn local_http_endpoint(&self) -> Option<String> {
        if !self.status().enabled { return None; }
        self.local_http.lock().ok().and_then(|server| {
            server.as_ref().filter(|server| now_ms() < server.expires_at_ms).map(|server| server.endpoint.clone())
        })
    }

    pub(crate) fn local_http_client_config(&self) -> Option<(String, String, i64)> {
        if !self.status().enabled { return None; }
        self.local_http.lock().ok().and_then(|server| {
            server.as_ref().filter(|server| now_ms() < server.expires_at_ms)
                .map(|server| (server.endpoint.clone(), server.read_token.clone(), server.expires_at_ms))
        })
    }

    fn configure_lan_control(&self, enabled: bool) -> Result<(), DiagnosticsError> {
        if enabled && !self.status().enabled { return Err(DiagnosticsError::new(CODE_DIAGNOSTICS_DISABLED, "enable MCP before LAN control")); }
        {
            let mut current = self.local_http.lock().map_err(|_| handler_error())?;
            if let Some(server) = current.as_mut() {
                if enabled && now_ms() >= server.expires_at_ms {
                    server.read_token = new_local_mcp_token();
                    server.ttl_ms = self.local_token_ttl();
                    server.expires_at_ms = now_ms().saturating_add(server.ttl_ms);
                }
                server.control_enabled = enabled;
            } else if enabled { return Err(DiagnosticsError::new(CODE_DIAGNOSTICS_DISABLED, "enable MCP before LAN control")); }
        }
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        // A mode flip invalidates handshakes: read sessions must not silently
        // grow control powers, and control sessions must not keep them.
        for session in state.sessions.values_mut().filter(|session| session.view.origin.starts_with("mcp-http")) {
            session.revoked = Some(CODE_SESSION_REVOKED);
        }
        Ok(())
    }

    fn local_http_authorized(&self, authorization: Option<&str>) -> Result<Scope, &'static str> {
        if !self.status().enabled { return Err("mcp_disabled") }
        let Some(authorization) = authorization else { return Err("bearer_missing") };
        let Some(token) = authorization.strip_prefix("Bearer ") else { return Err("bearer_invalid") };
        if token.len() > LOCAL_MCP_MAX_AUTH_BYTES { return Err("bearer_invalid") }
        let server = self.local_http.lock().map_err(|_| "mcp_state_busy")?;
        let Some(server) = server.as_ref() else { return Err("mcp_disabled") };
        if !constant_time_equal(token.as_bytes(), server.read_token.as_bytes()) { return Err("bearer_invalid") }
        if now_ms() >= server.expires_at_ms { return Err("bearer_expired") }
        Ok(if server.control_enabled { Scope::Control } else { Scope::Read })
    }

    fn local_http_session_ttl(&self, scope: Scope) -> i64 {
        let current = self.local_http.lock().unwrap_or_else(|error| error.into_inner());
        let expiry = current.as_ref().and_then(|server| match scope {
            Scope::Read => Some(server.expires_at_ms),
            Scope::Control => Some(server.expires_at_ms),
        }).unwrap_or(0);
        expiry.saturating_sub(now_ms()).max(0)
    }

    fn local_http_session_active(&self, session_id: &str, scope: Scope) -> bool {
        let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        state.enabled && state.sessions.get(session_id).is_some_and(|session| session.revoked.is_none()
            && now_ms() < session.view.expires_at_ms && session.view.origin == scope.local_origin())
    }

    pub(crate) fn start_local_http(&self, app: &AppHandle) -> Result<String, String> {
        let mut current = self.local_http.lock().map_err(|_| "MCP HTTP 服务状态被占用".to_string())?;
        if let Some(server) = current.take() {
            if now_ms() < server.expires_at_ms {
                let endpoint = server.endpoint.clone();
                *current = Some(server);
                return Ok(endpoint);
            }
            // An expired token must never be revived by toggling the switch.
            // Stop and join the old listener before minting the replacement.
            server.stop.store(true, Ordering::Release);
            drop(current);
            if let Some(join) = server.join { let _ = join.join(); }
            return self.start_local_http(app);
        }
        let host = infer_local_lan_host()?;
        // Keep the documented port stable when possible.  If another process
        // owns it, ask the OS for an ephemeral port and advertise that actual
        // port in the generated MCP configuration.
        let listener = TcpListener::bind(("0.0.0.0", LOCAL_MCP_PORT))
            .or_else(|_| TcpListener::bind(("0.0.0.0", 0)))
            .map_err(|error| format!("无法启动局域网 MCP HTTP 服务: {error}"))?;
        listener.set_nonblocking(true).map_err(|error| format!("无法配置局域网 MCP HTTP 服务: {error}"))?;
        let port = listener.local_addr().map_err(|error| format!("无法读取局域网 MCP HTTP 端口: {error}"))?.port();
        let endpoint = format!("http://{host}:{port}{LOCAL_MCP_PATH}");
        let read_token = new_local_mcp_token();
        let ttl_ms = self.local_token_ttl();
        let expires_at_ms = now_ms().saturating_add(ttl_ms);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let app = app.clone();
        let join = thread::Builder::new()
            .name("tokenme-mcp-http".into())
            .spawn(move || run_local_mcp_http(listener, app, thread_stop))
            .map_err(|error| format!("无法启动局域网 MCP HTTP 服务线程: {error}"))?;
        *current = Some(LocalMcpHttpServer { endpoint: endpoint.clone(), read_token, expires_at_ms, ttl_ms, control_enabled: false, stop, join: Some(join) });
        Ok(endpoint)
    }

    pub(crate) fn stop_local_http(&self) {
        let server = self.local_http.lock().ok().and_then(|mut slot| slot.take());
        let Some(mut server) = server else { return; };
        server.stop.store(true, Ordering::Release);
        if let Some(join) = server.join.take() {
            let _ = join.join();
        }
    }

    pub fn status(&self) -> DiagnosticsStatus {
        let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        DiagnosticsStatus {
            enabled: state.enabled,
            active_sessions: state.sessions.values().filter(|session| session.revoked.is_none()).count()
                + state.grants.values().count(),
            retained_operations: state.operations.len(),
        }
    }

    pub fn snapshot(&self) -> DiagnosticsSnapshot {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let now = now_ms();
        for session in state.sessions.values_mut() {
            if session.revoked.is_none() && now >= session.view.expires_at_ms {
                session.revoked = Some(CODE_SESSION_EXPIRED);
            }
        }
        state.grants.retain(|_, grant| now < grant.view.expires_at_ms);
        let (lan_control_enabled, lan_control_expires_at_ms) = {
            let current = self.local_http.lock().unwrap_or_else(|error| error.into_inner());
            current.as_ref().map_or((false, None), |server| {
                let live = state.enabled && now < server.expires_at_ms;
                (live && server.control_enabled, live.then_some(server.expires_at_ms))
            })
        };
        DiagnosticsSnapshot {
            lan_control_enabled,
            lan_control_expires_at_ms,
            enabled: state.enabled,
            sessions: state.sessions.values().filter(|session| session.revoked.is_none()).map(|session| session.view.clone())
                .chain(state.grants.values().map(|grant| grant.view.clone()))
                .collect(),
            retained_operations: state.operations.len(),
            last_operation: state.last_operation.clone(),
        }
    }

    pub fn set_enabled(&self, enabled: bool) -> DiagnosticsStatus {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        state.enabled = enabled;
        if !enabled {
            for session in state.sessions.values_mut() {
                session.revoked = Some(CODE_DIAGNOSTICS_DISABLED);
            }
            state.grants.clear();
        }
        let status = self.status_after(state);
        if !enabled {
            // Rotate the in-memory bearer even when a legacy caller only
            // toggles the state flag. Re-enabling must never revive a token
            // issued before the disable operation.
            if let Ok(mut current) = self.local_http.lock() {
                if let Some(server) = current.as_mut() {
                    server.read_token = new_local_mcp_token();
                    server.control_enabled = false;
                }
            }
        }
        status
    }

    fn status_after(&self, state: std::sync::MutexGuard<'_, Inner>) -> DiagnosticsStatus {
        DiagnosticsStatus {
            enabled: state.enabled,
            active_sessions: state.sessions.values().filter(|session| session.revoked.is_none()).count()
                + state.grants.values().count(),
            retained_operations: state.operations.len(),
        }
    }

    fn record_grant(&self, config: DiagnosticsConfig) {
        let grant = &config.grant;
        let view = SessionView {
            session_id: grant.grant_id.clone(),
            target_device: grant.target_device_id.clone(),
            origin: grant.debugger_origin.clone(),
            scopes: grant.scopes.iter().map(|scope| scope.as_str().into()).collect(),
            issued_at_ms: grant.issued_at_ms,
            expires_at_ms: grant.expires_at_ms,
        };
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        state.grants.insert(grant.grant_id.clone(), GrantSession { view });
    }

    fn revoke_grant(&self, grant_id: &str) -> Result<(), DiagnosticsError> {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if state.grants.remove(grant_id).is_none() {
            return Err(DiagnosticsError::new(CODE_SESSION_NOT_FOUND, "diagnostics grant is not active"));
        }
        Ok(())
    }

    pub(crate) fn clear_real_grants(&self) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        state.grants.clear();
    }

    fn begin_management_operation(&self, action: &str) -> String {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        state.next_management_operation = state.next_management_operation.saturating_add(1);
        let operation_id = format!("diag-control-{}", state.next_management_operation);
        state.last_operation = Some(DiagnosticsOperationView {
            operation_id: operation_id.clone(),
            action: action.to_string(),
            state: "running".into(),
            at_ms: now_ms(),
            error_code: None,
        });
        operation_id
    }

    fn complete_management_operation(&self, operation_id: &str, action: &str, result: Result<(), &DiagnosticsError>) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        state.last_operation = Some(DiagnosticsOperationView {
            operation_id: operation_id.to_string(),
            action: action.to_string(),
            state: if result.is_ok() { "completed" } else { "failed" }.into(),
            at_ms: now_ms(),
            error_code: result.err().map(|error| error.code.to_string()),
        });
    }

    fn new_session_id() -> String {
        format!("diag-session-{:032x}", rand::rng().random::<u128>())
    }

    pub(crate) fn create_session_for_panel(
        &self,
        target_device: String,
        origin: String,
        scopes: Vec<String>,
        ttl_ms: i64,
    ) -> Result<SessionView, DiagnosticsError> {
        self.create_session(SessionRequest {
            session_id: Self::new_session_id(),
            target_device,
            origin,
            scopes,
            issued_at_ms: now_ms(),
            expires_at_ms: now_ms().saturating_add(ttl_ms),
        })
    }

    pub(crate) fn revoke_session(&self, session_id: &str) -> Result<(), DiagnosticsError> {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(session) = state.sessions.get_mut(session_id) else {
            return Err(DiagnosticsError::new(CODE_SESSION_NOT_FOUND, "diagnostics session is not active"));
        };
        session.revoked = Some(CODE_SESSION_REVOKED);
        Ok(())
    }

    pub(crate) fn create_session(&self, request: SessionRequest) -> Result<SessionView, DiagnosticsError> {
        let now = now_ms();
        if !valid_identifier(&request.session_id)
            || !valid_identifier(&request.target_device)
            || !valid_identifier(&request.origin)
        {
            return Err(DiagnosticsError::new(CODE_INVALID_PARAMS, "session identity is invalid"));
        }
        if request.issued_at_ms > now.saturating_add(30_000)
            || request.expires_at_ms <= now
            || request.expires_at_ms <= request.issued_at_ms
            || request.expires_at_ms.saturating_sub(request.issued_at_ms) > MAX_SESSION_TTL_MS
        {
            return Err(DiagnosticsError::new(CODE_SESSION_EXPIRED, "session lifetime is invalid"));
        }
        let scopes = parse_scopes(&request.scopes)?;
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if !state.enabled {
            return Err(DiagnosticsError::new(CODE_DIAGNOSTICS_DISABLED, "remote diagnostics is disabled"));
        }
        state.sessions.retain(|_, session| session.revoked.is_none() && now < session.view.expires_at_ms);
        if state.sessions.len() >= MAX_OPERATION_HISTORY {
            return Err(DiagnosticsError::new(CODE_HANDLER_FAILED, "diagnostics session capacity reached"));
        }
        if state.sessions.contains_key(&request.session_id) {
            return Err(DiagnosticsError::new(CODE_INVALID_PARAMS, "session already exists"));
        }
        let view = SessionView {
            session_id: request.session_id.clone(),
            target_device: request.target_device,
            origin: request.origin,
            scopes: request.scopes,
            issued_at_ms: request.issued_at_ms,
            expires_at_ms: request.expires_at_ms,
        };
        state.sessions.insert(request.session_id, Session { view: view.clone(), scopes, revoked: None });
        logging::diagnostic_event(
            "diagnostics.session.created",
            Some("ok"),
            Some(&view.scopes.join(",")),
            Some(&view.origin),
            None,
            None,
            None,
            None,
        );
        Ok(view)
    }

    pub(crate) fn revoke_all(&self) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        for session in state.sessions.values_mut() {
            session.revoked = Some(CODE_SESSION_REVOKED);
        }
        state.grants.clear();
    }

    pub(crate) fn revoke_target(&self, target_device: &str) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        for session in state.sessions.values_mut().filter(|session| session.view.target_device == target_device) {
            session.revoked = Some(CODE_SESSION_REVOKED);
        }
        state.grants.retain(|_, grant| grant.view.target_device != target_device);
    }

    fn authorize(&self, request: &ToolRequest, scope: Scope) -> Result<Session, DiagnosticsError> {
        let now = now_ms();
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let denied = |error: DiagnosticsError| {
            logging::diagnostic_event(
                "diagnostics.request.denied",
                Some(if error.code == CODE_SESSION_EXPIRED { "expired" } else if error.code == CODE_SESSION_REVOKED { "revoked" } else { "denied" }),
                Some(scope.name()),
                Some(&request.origin),
                Some(&request.request_id),
                Some(error.code),
                None,
                Some(&request.tool),
            );
            error
        };
        if !state.enabled {
            return Err(denied(DiagnosticsError::new(CODE_DIAGNOSTICS_DISABLED, "remote diagnostics is disabled")));
        }
        let Some(session) = state.sessions.get_mut(&request.session_id) else {
            return Err(denied(DiagnosticsError::new(CODE_SESSION_NOT_FOUND, "diagnostics session is not active")));
        };
        if let Some(reason) = session.revoked {
            let code = if reason == CODE_DIAGNOSTICS_DISABLED { CODE_DIAGNOSTICS_DISABLED } else { reason };
            return Err(denied(DiagnosticsError::new(code, "diagnostics session is not active")));
        }
        if now >= session.view.expires_at_ms {
            session.revoked = Some(CODE_SESSION_EXPIRED);
            return Err(denied(DiagnosticsError::new(CODE_SESSION_EXPIRED, "diagnostics session has expired")));
        }
        if session.view.origin != request.origin {
            return Err(denied(DiagnosticsError::new(CODE_ORIGIN_MISMATCH, "diagnostics origin does not match the session")));
        }
        if request.target_device.as_deref().is_some_and(|target| target != session.view.target_device) {
            return Err(denied(DiagnosticsError::new(CODE_TARGET_MISMATCH, "diagnostics target does not match the session")));
        }
        if !session.scopes.contains(scope.name()) {
            return Err(denied(DiagnosticsError::new(CODE_SCOPE_DENIED, "diagnostics scope is not granted")));
        }
        Ok(session.clone())
    }


    fn begin_operation(&self, session: &Session) -> String {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        state.next_operation = state.next_operation.saturating_add(1);
        let id = format!("diag-op-{}", state.next_operation);
        state.operations.insert(id.clone(), OperationView {
            operation_id: id.clone(),
            session_id: session.view.session_id.clone(),
            status: "running",
            result: None,
            error_code: None,
        });
        while state.operations.len() > MAX_OPERATION_HISTORY {
            let Some(first) = state.operations.keys().next().cloned() else { break };
            state.operations.remove(&first);
        }
        id
    }

    fn complete_operation(&self, operation_id: &str, result: Result<Value, DiagnosticsError>) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(operation) = state.operations.get_mut(operation_id) else { return };
        match result {
            Ok(value) => {
                operation.status = "completed";
                operation.result = Some(value);
            }
            Err(error) => {
                operation.status = "failed";
                operation.error_code = Some(error.code);
            }
        }
    }

    fn operation(&self, operation_id: &str, session_id: &str) -> Result<Value, DiagnosticsError> {
        let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(operation) = state.operations.get(operation_id) else {
            return Err(DiagnosticsError::new(CODE_OPERATION_NOT_FOUND, "diagnostics operation is not retained"));
        };
        if operation.session_id != session_id {
            return Err(DiagnosticsError::new(CODE_OPERATION_NOT_FOUND, "diagnostics operation is not retained"));
        }
        serde_json::to_value(operation).map_err(|_| DiagnosticsError::new(CODE_HANDLER_FAILED, "diagnostics operation could not be encoded"))
    }
}

impl Drop for DiagnosticsState {
    fn drop(&mut self) {
        if let Ok(slot) = self.local_http.get_mut() {
            if let Some(mut server) = slot.take() {
                server.stop.store(true, Ordering::Release);
                if let Some(join) = server.join.take() { let _ = join.join(); }
            }
        }
    }
}

struct LocalHttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
    session_id: Option<String>,
    authorization: Option<String>,
}

fn run_local_mcp_http(listener: TcpListener, app: AppHandle, stop: Arc<AtomicBool>) {
    let mut handlers: Vec<JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::Acquire) {
        let mut active_handlers = Vec::with_capacity(handlers.len());
        for handler in handlers.drain(..) {
            if handler.is_finished() { let _ = handler.join(); } else { active_handlers.push(handler); }
        }
        handlers = active_handlers;
        match listener.accept() {
            Ok((stream, _peer)) if handlers.len() >= LOCAL_MCP_MAX_CONNECTIONS => {
                let mut stream = stream;
                let _ = stream.set_write_timeout(Some(LOCAL_MCP_REQUEST_TIMEOUT));
                let body = serde_json::to_vec(&json_rpc_error(Value::Null, -32000, "local MCP connection limit reached")).unwrap_or_default();
                let _ = write_local_http_response(&mut stream, 429, &body, None);
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            Ok((mut stream, _peer)) => {
                let app = app.clone();
                let handler = thread::Builder::new().name("tokenme-mcp-http-request".into()).spawn(move || {
                    let _ = stream.set_read_timeout(Some(LOCAL_MCP_REQUEST_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(LOCAL_MCP_REQUEST_TIMEOUT));
                    if let Err(error) = handle_local_mcp_http(&mut stream, &app) {
                        logging::diagnostic_event(
                            "diagnostics.request.denied",
                            Some("denied"),
                            Some("diagnostics.read"),
                            Some("mcp-http"),
                            None,
                            Some("invalid_request"),
                            None,
                            Some("http"),
                        );
                        let body = serde_json::to_vec(&json_rpc_error(Value::Null, -32600, &error)).unwrap_or_else(|_| b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32600,\"message\":\"invalid request\"}}".to_vec());
                        let _ = write_local_http_response(&mut stream, 400, &body, None);
                    }
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                });
                match handler {
                    Ok(handler) => handlers.push(handler),
                    Err(error) => {
                        logging::error(&format!("tokenme mcp http: request worker unavailable: {error}"));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                eprintln!("tokenme mcp http: listener stopped: {error}");
                break;
            }
        }
    }
    for handler in handlers { let _ = handler.join(); }
}

fn handle_local_mcp_http(stream: &mut TcpStream, app: &AppHandle) -> Result<(), String> {
    let request = read_local_http_request(stream)?;
    if request.method == "OPTIONS" {
        audit_local_http(None, "options", None, "ok", None);
        return write_local_http_response(stream, 204, &[], None);
    }
    if request.method != "POST" || request.path != LOCAL_MCP_PATH {
        audit_local_http(None, "http", None, "denied", Some("endpoint_not_found"));
        return write_local_http_response(stream, 404, br#"{"error":"MCP endpoint not found"}"#, None);
    }
    let state = app.state::<DiagnosticsState>();
    let access = match state.local_http_authorized(request.authorization.as_deref()) {
        Ok(access) => access,
        Err(error_code) => {
            audit_local_http(None, "http", None, "denied", Some(error_code));
            return write_local_http_response(stream, 401, br#"{"error":"MCP bearer authorization required"}"#, None);
        }
    };
    let value: Value = serde_json::from_slice(&request.body).map_err(|_| "invalid JSON-RPC request".to_string())?;
    let object = value.as_object().ok_or_else(|| "invalid JSON-RPC request".to_string())?;
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    let audit_id = json_rpc_id_string(&id);
    let method = object.get("method").and_then(Value::as_str).ok_or_else(|| "invalid JSON-RPC method".to_string())?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return write_local_http_response(stream, 200, &serde_json::to_vec(&json_rpc_error(id, -32600, "JSON-RPC version must be 2.0")).unwrap_or_default(), request.session_id.as_deref());
    }
    if method != "notifications/initialized" && !matches!(object.get("id"), Some(Value::String(_) | Value::Number(_))) {
        return write_local_http_response(stream, 200, &serde_json::to_vec(&json_rpc_error(Value::Null, -32600, "JSON-RPC request id is required")).unwrap_or_default(), request.session_id.as_deref());
    }
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    let mut response_session = request.session_id.clone();
    let response = match method {
        "initialize" => {
            if request.session_id.is_some() {
                Err("initialize must not include Mcp-Session-Id".to_string())
            } else {
                let session = state.create_session_for_panel(
                    "local-http".into(),
                    access.local_origin().into(),
                    access.local_scopes(),
                    state.local_http_session_ttl(access),
                ).map_err(|error| error.to_string())?;
                response_session = Some(session.session_id.clone());
                Ok(json_rpc_result(id.clone(), json!({
                    "protocolVersion": usage_device_sync::DIAGNOSTICS_PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "tokenme-diagnostics", "version": env!("CARGO_PKG_VERSION") },
                    "sessionId": session.session_id,
                })))
            }
        }
        "notifications/initialized" => {
            if request.session_id.as_deref().is_some_and(|session_id| state.local_http_session_active(session_id, access)) {
                audit_local_http(Some(access), method, audit_id.as_deref(), "ok", None);
                return write_local_http_response(stream, 202, &[], request.session_id.as_deref());
            }
            audit_local_http(Some(access), method, audit_id.as_deref(), "denied", Some(CODE_SESSION_NOT_FOUND));
            return write_local_http_response(stream, 401, br#"{"error":"Mcp-Session-Id is required; initialize first"}"#, None);
        }
        "ping" => {
            if !request.session_id.as_deref().is_some_and(|session_id| state.local_http_session_active(session_id, access)) {
                Err("Mcp-Session-Id is required; initialize first".to_string())
            } else {
                Ok(json_rpc_result(id.clone(), json!({})))
            }
        }
        "tools/list" => {
            if !request.session_id.as_deref().is_some_and(|session_id| state.local_http_session_active(session_id, access)) { Err("Mcp-Session-Id is required; initialize first".to_string()) }
            else { Ok(json_rpc_result(id.clone(), json!({ "tools": local_tool_registry(access) }))) }
        }
        "tools/call" => {
            let session_id = request.session_id.clone().ok_or_else(|| "Mcp-Session-Id is required; initialize first".to_string())?;
            let params = params.as_object().ok_or_else(|| "tools/call params must be an object".to_string())?;
            let tool = params.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()).ok_or_else(|| "tools/call requires a tool name".to_string())?;
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let request_id = object.get("id").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("mcp-http-{:032x}", rand::rng().random::<u128>()));
            let tool_request = ToolRequest {
                session_id,
                request_id,
                origin: access.local_origin().into(),
                target_device: None,
                tool: tool.into(),
                params: arguments,
            };
            let result = tauri::async_runtime::block_on(call_tool(app.clone(), tool_request)).map_err(|error| error.to_string())?;
            Ok(json_rpc_result(id.clone(), local_mcp_tool_result(result.result)?))
        }
        _ => Err(format!("unsupported MCP method: {method}")),
    };
    match response {
        Ok(response) => {
            audit_local_http(Some(access), method, audit_id.as_deref(), "ok", None);
            write_local_http_response(stream, 200, &serde_json::to_vec(&response).map_err(|_| "MCP response could not be encoded".to_string())?, response_session.as_deref())
        }
        Err(error) => {
            audit_local_http(Some(access), method, audit_id.as_deref(), "error", Some("mcp_request_failed"));
            write_local_http_response(stream, 200, &serde_json::to_vec(&json_rpc_error(id.clone(), -32602, &error)).unwrap_or_default(), response_session.as_deref())
        }
    }
}

fn json_rpc_id_string(id: &Value) -> Option<String> {
    match id {
        Value::String(value) => Some(value.chars().take(128).collect()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn local_tool_registry(access: Scope) -> Vec<usage_device_sync::DiagnosticsToolDefinition> {
    usage_device_sync::diagnostics_tool_registry()
        .into_iter()
        .filter(|tool| tool_scope(&tool.name).is_ok_and(|scope| access == Scope::Control || scope == Scope::Read))
        .collect()
}

fn audit_local_http(access: Option<Scope>, action: &str, request_id: Option<&str>, outcome: &str, error_code: Option<&str>) {
    logging::diagnostic_event(
        "diagnostics.operation.completed",
        Some(outcome),
        access.map(Scope::name),
        access.map(Scope::local_origin),
        request_id,
        error_code,
        None,
        Some(action),
    );
}

/// Convert the private host image marker into the standard MCP image result.
/// Local HTTP and HTTPS MCP must expose the same wire shape.
fn local_mcp_tool_result(value: Value) -> Result<Value, String> {
    let Some(marker) = value.get("_mcp_image").and_then(Value::as_object) else {
        let text = serde_json::to_string(&value).map_err(|_| "MCP tool result could not be encoded".to_string())?;
        return Ok(json!({ "content": [{ "type": "text", "text": text }], "structuredContent": value }));
    };
    let mime_type = marker.get("mime_type").and_then(Value::as_str).ok_or_else(|| "MCP image MIME type is invalid".to_string())?.to_owned();
    if !matches!(mime_type.as_str(), "image/png" | "image/jpeg" | "image/bmp") {
        return Err("MCP image MIME type is invalid".into());
    }
    let encoded = marker.get("data_base64").and_then(Value::as_str).ok_or_else(|| "MCP image data is invalid".to_string())?.to_owned();
    let max_encoded = 4 * ((usage_device_sync::MAX_DIAGNOSTICS_IMAGE_BYTES + 2) / 3);
    if encoded.len() > max_encoded {
        return Err("MCP image is too large".into());
    }
    let data = base64::engine::general_purpose::STANDARD.decode(&encoded).map_err(|_| "MCP image data is invalid".to_string())?;
    if data.is_empty() || data.len() > usage_device_sync::MAX_DIAGNOSTICS_IMAGE_BYTES {
        return Err("MCP image is too large".into());
    }
    let mut structured = value;
    structured.as_object_mut().ok_or_else(|| "MCP tool result is invalid".to_string())?.remove("_mcp_image");
    Ok(json!({
        "content": [{ "type": "image", "data": encoded, "mimeType": mime_type }],
        "structuredContent": structured,
    }))
}

fn read_local_http_request(stream: &mut TcpStream) -> Result<LocalHttpRequest, String> {
    let mut bytes = Vec::with_capacity(4096);
    let header_end = loop {
        if bytes.len() > LOCAL_MCP_MAX_HEADER_BYTES { return Err("MCP HTTP headers exceed the size limit".into()); }
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).map_err(|error| format!("MCP HTTP read failed: {error}"))?;
        if read == 0 { return Err("MCP HTTP request ended before headers".into()); }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") { break offset + 4; }
    };
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut parsed = httparse::Request::new(&mut headers);
    let status = parsed.parse(&bytes[..header_end]).map_err(|_| "invalid MCP HTTP headers".to_string())?;
    let httparse::Status::Complete(_) = status else { return Err("incomplete MCP HTTP headers".into()); };
    let method = parsed.method.ok_or_else(|| "MCP HTTP method is missing".to_string())?.to_string();
    let path = parsed.path.ok_or_else(|| "MCP HTTP path is missing".to_string())?.to_string();
    let mut content_length = 0usize;
    let mut session_id = None;
    let mut authorization = None;
    for header in parsed.headers.iter() {
        if header.name.eq_ignore_ascii_case("content-length") {
            content_length = std::str::from_utf8(header.value).ok().and_then(|value| value.trim().parse().ok()).ok_or_else(|| "invalid MCP HTTP content length".to_string())?;
        } else if header.name.eq_ignore_ascii_case("mcp-session-id") {
            session_id = Some(std::str::from_utf8(header.value).map_err(|_| "invalid MCP session header".to_string())?.trim().to_string());
        } else if header.name.eq_ignore_ascii_case("authorization") {
            if authorization.is_some() { return Err("duplicate MCP authorization header".into()); }
            let value = std::str::from_utf8(header.value).map_err(|_| "invalid MCP authorization header".to_string())?.trim();
            if value.len() > LOCAL_MCP_MAX_AUTH_BYTES { return Err("MCP authorization header exceeds the size limit".into()); }
            authorization = Some(value.to_owned());
        }
    }
    if content_length > LOCAL_MCP_MAX_BODY_BYTES { return Err("MCP HTTP body exceeds the size limit".into()); }
    let mut body = bytes[header_end..].to_vec();
    if body.len() > content_length { body.truncate(content_length); }
    while body.len() < content_length {
        let remaining = content_length - body.len();
        let mut chunk = vec![0u8; remaining.min(16 * 1024)];
        let read = stream.read(&mut chunk).map_err(|error| format!("MCP HTTP body read failed: {error}"))?;
        if read == 0 { return Err("MCP HTTP body ended early".into()); }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(LocalHttpRequest { method, path, body, session_id, authorization })
}

fn write_local_http_response(stream: &mut TcpStream, status: u16, body: &[u8], session_id: Option<&str>) -> Result<(), String> {
    let reason = match status { 200 => "OK", 202 => "Accepted", 204 => "No Content", 400 => "Bad Request", 401 => "Unauthorized", 404 => "Not Found", _ => "Error" };
    let content_type = if body.is_empty() { "text/plain; charset=utf-8" } else { "application/json" };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    if let Some(session_id) = session_id { head.push_str(&format!("Mcp-Session-Id: {session_id}\r\n")); }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).map_err(|error| format!("MCP HTTP response failed: {error}"))?;
    stream.write_all(body).map_err(|error| format!("MCP HTTP response failed: {error}"))?;
    stream.flush().map_err(|error| format!("MCP HTTP response failed: {error}"))
}

fn json_rpc_result(id: Value, result: Value) -> Value { json!({ "jsonrpc": "2.0", "id": id, "result": result }) }
fn json_rpc_error(id: Value, code: i32, message: &str) -> Value { json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }) }

#[derive(Debug, Clone, Deserialize)]
pub struct ToolRequest {
    pub session_id: String,
    pub request_id: String,
    pub origin: String,
    #[serde(default)]
    pub target_device: Option<String>,
    pub tool: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolResponse {
    pub operation_id: String,
    pub result: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub scope: &'static str,
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    [
        ("device.get_status", "Return safe device and sync status", "diagnostics.read"),
        ("device.get_build", "Return version, build ID, and running binary hash", "diagnostics.read"),
        ("logs.tail", "Read a bounded tail of a fixed log", "diagnostics.read"),
        ("logs.search", "Search a fixed log with bounded output", "diagnostics.read"),
        ("sync.get_status", "Return redacted sync status", "diagnostics.read"),
        ("sync.get_requests", "Return pairing request summaries", "diagnostics.read"),
        ("sync.get_trace", "Return recent persistent diagnostics events", "diagnostics.read"),
        ("diagnostics.run_check", "Run fixed listener, TLS, database, and IPC checks", "diagnostics.read"),
        ("diagnostics.get_operation", "Read a retained operation result", "diagnostics.read"),
        ("app.get_panel_state", "Return safe panel visibility state", "diagnostics.read"),
        ("ui.capture_panel", "Capture only the visible TokenMe panel", "diagnostics.read"),
        ("sync.start_primary", "Start the existing primary sync service", "sync.control"),
        ("sync.pair", "Pair this device from a complete tokenme invite after TLS pin validation", "sync.control"),
        ("sync.stop_primary", "Stop the existing primary sync service", "sync.control"),
        ("sync.discover", "Run one existing local discovery action", "sync.control"),
        ("sync.approve_pairing", "Approve one existing pairing request", "sync.control"),
        ("sync.reject_pairing", "Reject one existing pairing request", "sync.control"),
        ("sync.clear_expired_request", "Clear one expired pairing request", "sync.control"),
        ("sync.pause", "Pause one paired device", "sync.control"),
        ("sync.resume", "Resume one paired device", "sync.control"),
        ("sync.revoke_device", "Revoke one paired device", "sync.control"),
        ("sync.refresh", "Refresh existing sync status", "sync.control"),
        ("ui.open_sync_sheet", "Open the existing sync sheet", "sync.control"),
        ("ui.focus_device", "Focus a device in the existing sync sheet", "sync.control"),
        ("ui.click_panel", "Click one bounded coordinate inside the TokenMe panel", "sync.control"),
    ]
    .into_iter()
    .map(|(name, description, scope)| ToolDefinition { name, description, scope })
    .collect()
}

fn tool_scope(tool: &str) -> Result<Scope, DiagnosticsError> {
    let definition = tool_definitions().into_iter().find(|definition| definition.name == tool)
        .ok_or_else(|| DiagnosticsError::new(CODE_UNKNOWN_TOOL, "diagnostics tool is not registered"))?;
    tool_definition_scope(definition.scope)
}

fn tool_definition_scope(scope: &str) -> Result<Scope, DiagnosticsError> {
    match scope {
        "diagnostics.read" => Ok(Scope::Read),
        "sync.control" => Ok(Scope::Control),
        _ => Err(DiagnosticsError::new(CODE_UNKNOWN_TOOL, "diagnostics tool scope is not registered")),
    }
}

/// Protocol adapters call this function after their transport and grant
/// verifier have authenticated a request. No protocol request reaches a raw
/// Tauri command without this session gate.
pub async fn call_tool(app: AppHandle, request: ToolRequest) -> Result<ToolResponse, DiagnosticsError> {
    let scope = tool_scope(&request.tool)?;
    if !valid_identifier(&request.request_id) || !valid_identifier(&request.origin) {
        return Err(DiagnosticsError::new(CODE_INVALID_PARAMS, "request identity is invalid"));
    }
    let params_size = serde_json::to_vec(&request.params).map_err(|_| DiagnosticsError::new(CODE_INVALID_PARAMS, "request parameters are invalid"))?.len();
    if params_size > MAX_PARAMS_BYTES {
        return Err(DiagnosticsError::new(CODE_INVALID_PARAMS, "request parameters are too large"));
    }
    let diagnostics = app.state::<DiagnosticsState>();
    let session = diagnostics.authorize(&request, scope)?;
    let operation_id = diagnostics.begin_operation(&session);
    let result = dispatch(&app, &diagnostics, &request, &session, &operation_id).await;
    diagnostics.complete_operation(&operation_id, result.clone());
    logging::diagnostic_event(
        "diagnostics.operation.completed",
        Some(if result.is_ok() { "ok" } else { "error" }),
        Some(scope.name()),
        Some(&session.view.origin),
        Some(&request.request_id),
        result.as_ref().err().map(|error| error.code),
        Some(&operation_id),
        Some(&request.tool),
    );
    result.map(|result| ToolResponse { operation_id, result })
}

async fn dispatch(
    app: &AppHandle,
    diagnostics: &DiagnosticsState,
    request: &ToolRequest,
    session: &Session,
    operation_id: &str,
) -> Result<Value, DiagnosticsError> {
    match request.tool.as_str() {
        "device.get_status" => {
            let sync = sync_status(app).await.ok();
            Ok(json!({
                "device_name": "TokenMe",
                "system": std::env::consts::OS,
                "version": env!("CARGO_PKG_VERSION"),
                "build_id": env!("TOKENME_BUILD_ID"),
                "process_running": true,
                "sync": sync.as_ref().map(safe_sync_view),
            }))
        }
        "device.get_build" => current_build(),
        "logs.tail" => {
            let params = object(&request.params)?;
            if params.keys().any(|key| !matches!(key.as_str(), "log" | "lines")) { return Err(invalid_params()); }
            let log = string(params, "log")?;
            let lines = number(params, "lines", 50, 1, usage_device_sync::MAX_DIAGNOSTICS_LOG_LINES as u64)? as usize;
            let lines = logging::diagnostic_log_tail(log, lines, None).map_err(|_| handler_error())?;
            Ok(json!({ "log": log, "lines": lines }))
        }
        "logs.search" => {
            let params = object(&request.params)?;
            if params.keys().any(|key| !matches!(key.as_str(), "log" | "query" | "since_ms" | "limit")) { return Err(invalid_params()); }
            let log = string(params, "log")?;
            let query = string(params, "query")?;
            let since_ms = optional_number(params, "since_ms")?;
            let limit = number(params, "limit", 50, 1, usage_device_sync::MAX_DIAGNOSTICS_LOG_SEARCH_RESULTS as u64)? as usize;
            let lines = logging::diagnostic_log_search(log, query, since_ms, limit).map_err(|_| handler_error())?;
            Ok(json!({ "log": log, "lines": lines }))
        }
        "sync.get_status" => Ok(safe_sync_view(&sync_status(app).await?)),
        "sync.get_requests" => {
            let status = sync_status(app).await?;
            Ok(json!({ "requests": status.pairing_requests.iter().map(|request| json!({
                "request_id": request.request_id,
                "device_id": request.device_id,
                "origin": request.origin,
                "display_name": request.display_name,
                "os": request.os,
                "status": request.status,
            })).collect::<Vec<_>>() }))
        }
        "sync.get_trace" => {
            let params = object_or_empty(&request.params)?;
            if params.keys().any(|key| key != "limit") { return Err(invalid_params()); }
            let limit = number(params, "limit", 50, 1, usage_device_sync::MAX_DIAGNOSTICS_TRACE_RESULTS as u64)? as usize;
            let lines = logging::diagnostic_log_tail("diagnostics", limit, None).map_err(|_| handler_error())?;
            Ok(json!({ "events": lines }))
        }
        "diagnostics.run_check" => run_checks(app, &request.params).await,
        "diagnostics.get_operation" => {
            let params = object(&request.params)?;
            if params.keys().any(|key| key != "operation_id") { return Err(invalid_params()); }
            let operation_id = string(params, "operation_id")?;
            diagnostics.operation(operation_id, &session.view.session_id)
        }
        "app.get_panel_state" => Ok(json!({
            "panel_open": !panel::is_panel_hidden(),
            "current_page": Value::Null,
            "current_page_known": false,
            "background_tasks": [],
        })),
        "ui.capture_panel" => {
            if !object_or_empty(&request.params)?.is_empty() { return Err(invalid_params()); }
            if !panel_is_visible(app) {
                return Err(DiagnosticsError::new(CODE_TOOL_UNAVAILABLE, "TokenMe panel is not visible; call ui.open_sync_sheet first"));
            }
            let image = panel::capture_panel(app).map_err(|_| DiagnosticsError::new(CODE_TOOL_UNAVAILABLE, "TokenMe panel screenshot is unavailable; check platform capture permission"))?;
            if image.data.len() > usage_device_sync::MAX_DIAGNOSTICS_IMAGE_BYTES { return Err(DiagnosticsError::new(CODE_INVALID_PARAMS, "TokenMe panel screenshot is too large")); }
            Ok(json!({
                "_mcp_image": {
                    "mime_type": image.mime_type,
                    "data_base64": base64::engine::general_purpose::STANDARD.encode(&image.data),
                },
                "width": image.width,
                "height": image.height,
                "scope": "tokenme-panel",
            }))
        }
        "sync.start_primary" => {
            control_sync(app, primary_start_request(&request.params)?).await
        }
        "sync.pair" => {
            let params = object(&request.params)?;
            if params.keys().any(|key| !matches!(key.as_str(), "invite" | "origin" | "display_name" | "os")) { return Err(invalid_params()); }
            let invite = string(params, "invite")?;
            if !invite.starts_with("tokenme://device-pair?") || invite.len() > usage_device_sync::MAX_DIAGNOSTICS_STRING_BYTES {
                return Err(invalid_params());
            }
            control_sync(app, SyncManagementRequest::Pair { invite: invite.to_owned(), origin: optional_string(params, "origin")?, display_name: optional_string(params, "display_name")?, os: optional_string(params, "os")? }).await
        }
        "sync.stop_primary" => {
            let status = sync_status(app).await?;
            if status.role != "primary" {
                return Err(DiagnosticsError::new(CODE_HANDLER_FAILED, "primary sync service is not active"));
            }
            let result = control_sync(app, SyncManagementRequest::Stop).await;
            diagnostics.revoke_all();
            result
        }
        "sync.discover" => control_sync(app, SyncManagementRequest::Discover).await,
        "sync.approve_pairing" => pairing_action(app, &request.params, true, false).await,
        "sync.reject_pairing" => pairing_action(app, &request.params, false, false).await,
        "sync.clear_expired_request" => pairing_action(app, &request.params, false, true).await,
        "sync.pause" => device_action(app, &request.params, true).await,
        "sync.resume" => device_action(app, &request.params, false).await,
        "sync.revoke_device" => {
            let device_id = required_identifier(&request.params, "device_id")?;
            let result = control_sync(app, SyncManagementRequest::Revoke { device_id: device_id.to_string() }).await;
            if result.is_ok() {
                diagnostics.revoke_target(device_id);
            }
            result
        }
        "sync.refresh" => control_sync(app, SyncManagementRequest::Status).await,
        "ui.open_sync_sheet" => {
            panel::show(app, None);
            wait_for_panel_visible(app).await?;
            app.emit("sync-open-sheet", ()).map_err(|_| handler_error())?;
            Ok(json!({ "opened": true }))
        }
        "ui.focus_device" => {
            let device_id = required_identifier(&request.params, "device_id")?;
            panel::show(app, None);
            wait_for_panel_visible(app).await?;
            app.emit("sync-focus-device", device_id).map_err(|_| handler_error())?;
            Ok(json!({ "opened": true, "device_id": device_id }))
        }
        "ui.click_panel" => {
            let params = object(&request.params)?;
            if params.keys().any(|key| key != "x" && key != "y") { return Err(invalid_params()); }
            let x = params.get("x").and_then(Value::as_f64).ok_or_else(invalid_params)?;
            let y = params.get("y").and_then(Value::as_f64).ok_or_else(invalid_params)?;
            if !panel_is_visible(app) {
                return Err(DiagnosticsError::new(CODE_TOOL_UNAVAILABLE, "TokenMe panel is not visible; call ui.open_sync_sheet first"));
            }
            panel::click_panel(app, x, y).map_err(|_| DiagnosticsError::new(CODE_TOOL_UNAVAILABLE, "TokenMe panel click is unavailable; check platform input permission"))?;
            Ok(json!({ "accepted": true, "x": x, "y": y, "scope": "tokenme-panel" }))
        }
        _ => Err(DiagnosticsError::new(CODE_UNKNOWN_TOOL, "diagnostics tool is not registered")),
    }
    .map(|value| {
        let _ = operation_id;
        value
    })
}

fn current_build() -> Result<Value, DiagnosticsError> {
    let path = std::env::current_exe().map_err(|_| handler_error())?;
    let mut file = File::open(&path).map_err(|_| handler_error())?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|_| handler_error())?;
        if read == 0 { break; }
        digest.update(&buffer[..read]);
    }
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build_id": env!("TOKENME_BUILD_ID"),
        "binary_name": path.file_name().and_then(|name| name.to_str()).unwrap_or("tokenme"),
        "binary_sha256": format!("{:x}", digest.finalize()),
    }))
}

async fn sync_status(app: &AppHandle) -> Result<SyncDevicesView, DiagnosticsError> {
    let control = app.state::<SyncControl>();
    commands::sync_devices(app.clone(), Some(SyncManagementRequest::Status), control)
        .await
        .map_err(|_| handler_error())
}

#[derive(Debug)]
struct SyncProbeResults {
    listener: Result<(), String>,
    tls: Result<(), String>,
    database: Result<(), String>,
}

fn parse_certificate_pin(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut pin = [0u8; 32];
    for (index, slot) in pin.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(pin)
}

fn sync_database_probe(path: &Path) -> Result<(), String> {
    let snapshot = usage_device_sync::SyncStore::read_status(path).map_err(|error| format!("sync database status failed: {error}"))?;
    let Some(primary_id) = snapshot.primary_id else {
        return Err("sync database has no configured primary".into());
    };
    let store = usage_device_sync::SyncStore::open(path.to_path_buf(), primary_id)
        .map_err(|error| format!("sync database open failed: {error}"))?;
    store.outbox_summary().map(|_| ()).map_err(|error| format!("sync database outbox check failed: {error}"))
}

fn default_sync_database_probe() -> Result<(), String> {
    let path = usage_index::Index::default_path().ok_or_else(|| "sync database path is unavailable".to_string())?;
    sync_database_probe(&path)
}

fn sync_probe_results(status: &SyncDevicesView, database: Result<(), String>) -> SyncProbeResults {
    if !status.service_running || !matches!(status.role.as_str(), "primary" | "secondary") {
        return SyncProbeResults {
            listener: Err("sync service is not running".into()),
            tls: Err("sync service is not running".into()),
            database,
        };
    }
    let Some(endpoint) = status.endpoint.clone() else {
        return SyncProbeResults {
            listener: Err("sync endpoint is unavailable".into()),
            tls: Err("sync endpoint is unavailable".into()),
            database: database.clone(),
        };
    };
    let Some(pin_text) = status.certificate_pin.as_deref() else {
        return SyncProbeResults {
            listener: Err("sync certificate pin is unavailable".into()),
            tls: Err("sync certificate pin is unavailable".into()),
            database: database.clone(),
        };
    };
    let Some(pin) = parse_certificate_pin(pin_text) else {
        return SyncProbeResults {
            listener: Err("sync certificate pin is invalid".into()),
            tls: Err("sync certificate pin is invalid".into()),
            database: database.clone(),
        };
    };
    // Keep the host runner bounded below the protocol's five-second operation
    // deadline.  A connect/TLS black hole must finish before the remote
    // diagnostics worker returns its timeout result, otherwise the detached
    // Tauri blocking task could outlive the operation and make the final
    // status ambiguous.
    let mut config = usage_device_sync::HttpsTransportConfig::pinned_hash(endpoint, "localhost", pin);
    config.connect_timeout = Duration::from_secs(4);
    config.request_timeout = Duration::from_secs(4);
    let transport = match usage_device_sync::Tls13HttpsTransport::new(config) {
        Ok(transport) => transport,
        Err(error) => {
            let detail = error.to_string();
            return SyncProbeResults {
                listener: Err(detail.clone()),
                tls: Err(detail),
                database,
            };
        }
    };
    let listener = transport.probe_tcp().map_err(|error| format!("TCP listener probe failed: {error}"));
    let tls = transport.probe_tls().map_err(|error| format!("TLS 1.3 probe failed: {error}"));
    SyncProbeResults { listener, tls, database }
}

fn primary_start_request(params: &Value) -> Result<SyncManagementRequest, DiagnosticsError> {
    commands::diagnostics_primary_start_request(params).map_err(|_| invalid_params())
}

async fn control_sync(app: &AppHandle, request: SyncManagementRequest) -> Result<Value, DiagnosticsError> {
    let control = app.state::<SyncControl>();
    let status = commands::sync_devices(app.clone(), Some(request), control)
        .await
        .map_err(|_| handler_error())?;
    Ok(safe_sync_view(&status))
}

async fn pairing_action(app: &AppHandle, params: &Value, approve: bool, expired: bool) -> Result<Value, DiagnosticsError> {
    let request_id = required_identifier(params, "request_id")?;
    if expired {
        let status = sync_status(app).await?;
        if !status.pairing_requests.iter().any(|request| request.request_id == request_id && request.status == "expired") {
            return Err(DiagnosticsError::new(CODE_INVALID_PARAMS, "pairing request is not expired"));
        }
    }
    control_sync(app, if approve {
        SyncManagementRequest::Approve { request_id: request_id.to_string() }
    } else {
        SyncManagementRequest::Reject { request_id: request_id.to_string() }
    }).await
}

async fn device_action(app: &AppHandle, params: &Value, pause: bool) -> Result<Value, DiagnosticsError> {
    let device_id = required_identifier(params, "device_id")?;
    control_sync(app, if pause {
        SyncManagementRequest::Pause { device_id: device_id.to_string() }
    } else {
        SyncManagementRequest::Resume { device_id: device_id.to_string() }
    }).await
}

async fn run_checks(app: &AppHandle, params: &Value) -> Result<Value, DiagnosticsError> {
    let params = object_or_empty(params)?;
    if params.keys().any(|key| key != "checks") { return Err(invalid_params()); }
    let checks = match params.get("checks") {
        None => vec!["listener", "tls", "pin", "database", "ipc"],
        Some(Value::Array(values)) => values.iter().map(|value| value.as_str().ok_or_else(|| invalid_params())).collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(invalid_params()),
    };
    if checks.is_empty() || checks.len() > usage_device_sync::MAX_DIAGNOSTICS_CHECKS || checks.iter().any(|check| !matches!(*check, "listener" | "tls" | "pin" | "database" | "ipc")) {
        return Err(invalid_params());
    }
    let sync = if checks.iter().any(|check| matches!(*check, "listener" | "tls" | "pin" | "database")) {
        Some(sync_status(app).await?)
    } else {
        None
    };
    let probes = if checks.iter().any(|check| matches!(*check, "listener" | "tls" | "database")) {
        if let Some(status) = sync.clone() {
            Some(tauri::async_runtime::spawn_blocking(move || {
                let database = default_sync_database_probe().map_err(|error| format!("database probe failed: {error}"));
                sync_probe_results(&status, database)
            }).await.map_err(|_| handler_error())?)
        } else {
            None
        }
    } else {
        None
    };
    let results = checks.into_iter().map(|check| {
        let (ok, detail) = match check {
            "listener" => match probes.as_ref().map(|probes| &probes.listener) {
                Some(Ok(())) => (true, "TCP listener probe succeeded".into()),
                Some(Err(detail)) => (false, detail.clone()),
                None => (false, "listener probe unavailable".into()),
            },
            "tls" => match probes.as_ref().map(|probes| &probes.tls) {
                Some(Ok(())) => (true, "TLS 1.3 handshake and certificate pin probe succeeded".into()),
                Some(Err(detail)) => (false, detail.clone()),
                None => (false, "TLS probe unavailable".into()),
            },
            "pin" => {
                let valid = sync.as_ref().and_then(|status| status.certificate_pin.as_deref()).and_then(parse_certificate_pin).is_some();
                (valid, if valid { "certificate pin shape valid" } else { "certificate pin unavailable or invalid" }.into())
            }
            "database" => match probes.as_ref().map(|probes| &probes.database) {
                Some(Ok(())) => (true, "sync database and outbox probe succeeded".into()),
                Some(Err(detail)) => (false, detail.clone()),
                None => (false, "database probe unavailable".into()),
            },
            "ipc" => {
                let sync_state = app.try_state::<SyncControl>().is_some();
                let diagnostics_state = app.try_state::<DiagnosticsState>().is_some();
                let ok = sync_state && diagnostics_state;
                let detail = match (sync_state, diagnostics_state) {
                    (true, true) => "Tauri SyncControl and DiagnosticsState are registered".into(),
                    (false, true) => "Tauri SyncControl state is unavailable".into(),
                    (true, false) => "Tauri DiagnosticsState is unavailable".into(),
                    (false, false) => "Tauri SyncControl and DiagnosticsState are unavailable".into(),
                };
                (ok, detail)
            }
            _ => (false, "check unavailable".into()),
        };
        json!({ "check": check, "ok": ok, "detail": detail })
    }).collect::<Vec<_>>();
    Ok(json!({ "checks": results }))
}

/// Synchronous bridge used by the HTTPS diagnostics authority.  The protocol
/// worker invokes this on its own bounded runner thread; the bridge enters the
/// Tauri runtime only long enough to execute the same checks used by local MCP.
pub(crate) fn run_remote_checks(
    app: AppHandle,
    checks: Vec<String>,
) -> Result<BTreeMap<String, usage_device_sync::DiagnosticsCheckResult>, usage_device_sync::DiagnosticsError> {
    let value = tauri::async_runtime::block_on(run_checks(&app, &json!({ "checks": checks })))
        .map_err(|_| usage_device_sync::DiagnosticsError::Internal)?;
    let values = value.get("checks").and_then(Value::as_array).ok_or(usage_device_sync::DiagnosticsError::Internal)?;
    values.iter().map(|value| {
        let check = value.get("check").and_then(Value::as_str).ok_or(usage_device_sync::DiagnosticsError::Internal)?;
        let ok = value.get("ok").and_then(Value::as_bool).ok_or(usage_device_sync::DiagnosticsError::Internal)?;
        let detail = value.get("detail").and_then(Value::as_str).map(str::to_owned);
        let status = if ok {
            "ok"
        } else if detail.as_deref().is_some_and(|detail| detail.contains("unavailable")) {
            "unavailable"
        } else {
            "error"
        };
        Ok((check.to_owned(), usage_device_sync::DiagnosticsCheckResult { status: status.into(), detail }))
    }).collect()
}

fn safe_sync_view(status: &SyncDevicesView) -> Value {
    json!({
        "primary_id": status.primary_id,
        "role": status.role,
        "service_running": status.service_running,
        "service_health": status.service_health,
        "endpoint": status.endpoint,
        "stream_id": status.stream_id,
        "data_epoch": status.data_epoch,
        "pending": status.pending,
        "in_flight": status.in_flight,
        "pending_bytes": status.pending_bytes,
        "has_error": status.last_error.is_some(),
        "has_discovery_error": status.discovery_error.is_some(),
        "last_connected_at_ms": status.last_connected_at_ms,
        "operation_state": status.operation_state,
        "last_checked_at_ms": status.last_checked_at_ms,
        "devices": status.devices.iter().map(|device| json!({
            "device_id": device.device_id,
            "origin": device.origin,
            "display_name": device.display_name,
            "os": device.os,
            "role": device.role,
            "status": device.status,
            "data_epoch": device.data_epoch,
            "active_stream_id": device.active_stream_id,
            "last_generation": device.last_generation,
            "last_merged_at_ms": device.last_merged_at_ms,
            "snapshot_complete": device.snapshot_complete,
        })).collect::<Vec<_>>(),
    })
}

fn parse_scopes(scopes: &[String]) -> Result<BTreeSet<String>, DiagnosticsError> {
    let scopes = scopes.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if scopes.is_empty() || scopes.iter().any(|scope| !matches!(*scope, "diagnostics.read" | "sync.control")) {
        return Err(invalid_params());
    }
    Ok(scopes.into_iter().map(str::to_string).collect())
}

fn parse_grant_scopes(scopes: &[String]) -> Result<Vec<usage_device_sync::DiagnosticsScope>, DiagnosticsError> {
    let mut parsed = scopes.iter().map(|scope| match scope.as_str() {
        "diagnostics.read" => Ok(usage_device_sync::DiagnosticsScope::Read),
        "sync.control" => Ok(usage_device_sync::DiagnosticsScope::SyncControl),
        _ => Err(invalid_params()),
    }).collect::<Result<Vec<_>, _>>()?;
    parsed.sort_unstable();
    parsed.dedup();
    if parsed.is_empty() { return Err(invalid_params()); }
    Ok(parsed)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.chars().all(|character| !character.is_control() && character != '/' && character != '\\')
}

fn object(value: &Value) -> Result<&serde_json::Map<String, Value>, DiagnosticsError> {
    value.as_object().ok_or_else(invalid_params)
}

fn object_or_empty(value: &Value) -> Result<&serde_json::Map<String, Value>, DiagnosticsError> {
    if value.is_null() { return Ok(EMPTY_PARAMS.get_or_init(serde_json::Map::new)); }
    object(value)
}

static EMPTY_PARAMS: std::sync::OnceLock<serde_json::Map<String, Value>> = std::sync::OnceLock::new();

pub(crate) fn panel_is_visible(app: &AppHandle) -> bool {
    app.get_webview_window(panel::LABEL).is_some_and(|window| {
        window.is_visible().unwrap_or(false) && !panel::is_panel_hidden()
    })
}

pub(crate) async fn wait_for_panel_visible(app: &AppHandle) -> Result<(), DiagnosticsError> {
    let deadline = Instant::now() + Duration::from_millis(800);
    while !panel_is_visible(app) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if panel_is_visible(app) {
        Ok(())
    } else {
        Err(DiagnosticsError::new(CODE_TOOL_UNAVAILABLE, "TokenMe panel did not become visible"))
    }
}

fn string<'a>(params: &'a serde_json::Map<String, Value>, name: &str) -> Result<&'a str, DiagnosticsError> {
    params.get(name).and_then(Value::as_str).filter(|value| !value.is_empty()).ok_or_else(invalid_params)
}

fn optional_string<'a>(params: &'a serde_json::Map<String, Value>, name: &str) -> Result<Option<String>, DiagnosticsError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(value.as_str().filter(|value| valid_identifier(value)).ok_or_else(invalid_params)?.to_string())),
    }
}

fn optional_bool(params: &serde_json::Map<String, Value>, name: &str) -> Result<Option<bool>, DiagnosticsError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_bool().map(Some).ok_or_else(invalid_params),
    }
}

fn optional_number(params: &serde_json::Map<String, Value>, name: &str) -> Result<Option<i64>, DiagnosticsError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_i64().filter(|value| *value >= 0).map(Some).ok_or_else(invalid_params),
    }
}

fn number(params: &serde_json::Map<String, Value>, name: &str, default: u64, min: u64, max: u64) -> Result<u64, DiagnosticsError> {
    let value = params.get(name).and_then(Value::as_u64).unwrap_or(default);
    (min..=max).contains(&value).then_some(value).ok_or_else(invalid_params)
}

fn required_identifier<'a>(params: &'a Value, name: &str) -> Result<&'a str, DiagnosticsError> {
    let value = string(object(params)?, name)?;
    valid_identifier(value).then_some(value).ok_or_else(invalid_params)
}

fn invalid_params() -> DiagnosticsError { DiagnosticsError::new(CODE_INVALID_PARAMS, "diagnostics parameters are invalid") }
fn handler_error() -> DiagnosticsError { DiagnosticsError::new(CODE_HANDLER_FAILED, "diagnostics handler failed") }
fn primary_unavailable() -> DiagnosticsError { DiagnosticsError::new(CODE_PRIMARY_UNAVAILABLE, "the primary HTTPS service is not running") }
fn grant_failed() -> DiagnosticsError { DiagnosticsError::new(CODE_GRANT_FAILED, "the primary rejected the diagnostics grant") }

#[tauri::command]
pub fn set_diagnostics_enabled(app: AppHandle, enabled: bool, state: State<'_, DiagnosticsState>) -> Result<DiagnosticsStatus, String> {
    configure_local_mcp(&app, &state, enabled, None).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn get_diagnostics_status(state: State<'_, DiagnosticsState>) -> DiagnosticsStatus {
    state.status()
}

#[tauri::command]
pub fn revoke_diagnostics_sessions(state: State<'_, DiagnosticsState>) -> DiagnosticsStatus {
    state.revoke_all();
    state.status()
}

fn diagnostics_action_name(request: &DiagnosticsManagementRequest) -> &'static str {
    match request {
        DiagnosticsManagementRequest::Status => "status",
        DiagnosticsManagementRequest::Configure { .. } => "configure",
        DiagnosticsManagementRequest::ConfigureLanControl { .. } => "configure_lan_control",
        DiagnosticsManagementRequest::CreateGrant { .. } => "create_grant",
        DiagnosticsManagementRequest::RevokeGrant { .. } => "revoke_grant",
        DiagnosticsManagementRequest::RevokeAll => "revoke_all",
    }
}

/// The panel-facing diagnostics contract.  Domain failures are returned as a
/// response with the operation id retained, so the UI can show the exact
/// failure and offer a retry without losing the audit trail.
#[tauri::command]
pub fn diagnostics_session(
    app: AppHandle,
    request: DiagnosticsManagementRequest,
    state: State<'_, DiagnosticsState>,
    control: State<'_, SyncControl>,
) -> Result<DiagnosticsResponse, String> {
    let action = diagnostics_action_name(&request);
    if matches!(request, DiagnosticsManagementRequest::Status) {
        // Grants are held by the in-process HTTPS authority.  If the primary
        // service was stopped or crashed, its authority is gone; discard the
        // panel mirror before returning status so expired credentials cannot
        // remain visible as ready to export.
        if commands::primary_diagnostics_context(&control).is_err() {
            state.clear_real_grants();
        }
        return Ok(DiagnosticsResponse { snapshot: state.snapshot(), config: None, error: None });
    }
    let operation_id = state.begin_management_operation(action);
    let outcome = match request {
        DiagnosticsManagementRequest::Configure { enabled, ttl_ms } => {
            configure_local_mcp(&app, &state, enabled, ttl_ms).map(|_| None::<DiagnosticsConfig>)
        }
        DiagnosticsManagementRequest::ConfigureLanControl { enabled } => state.configure_lan_control(enabled).map(|_| None::<DiagnosticsConfig>),
        DiagnosticsManagementRequest::CreateGrant { debugger_device_id, scopes, ttl_ms } => {
            if ttl_ms <= 0 || ttl_ms > usage_device_sync::MAX_DIAGNOSTICS_GRANT_TTL_MS {
                Err(DiagnosticsError::new(CODE_SESSION_EXPIRED, "session lifetime is invalid"))
            } else if !state.status().enabled {
                Err(DiagnosticsError::new(CODE_DIAGNOSTICS_DISABLED, "remote diagnostics is disabled"))
            } else {
                let context = commands::primary_diagnostics_context(&control).map_err(|_| primary_unavailable());
                context.and_then(|context| {
                    let parsed_scopes = parse_grant_scopes(&scopes)?;
                    let grant = context.handle.issue_diagnostics_grant(
                        &debugger_device_id,
                        &parsed_scopes,
                        now_ms(),
                        Duration::from_millis(ttl_ms as u64),
                    ).map_err(|_| grant_failed())?;
                    let config = DiagnosticsConfig {
                        schema: "tokenme-diagnostics-grant/v1".into(),
                        endpoint: context.endpoint,
                        server_name: "localhost".into(),
                        certificate_pin: context.certificate_pin,
                        certificate_der_b64: base64::engine::general_purpose::STANDARD.encode(&context.certificate_der),
                        grant,
                    };
                    state.record_grant(config.clone());
                    Ok(Some(config))
                })
            }
        }
        DiagnosticsManagementRequest::RevokeGrant { grant_id } => {
            let context = commands::primary_diagnostics_context(&control).map_err(|_| primary_unavailable());
            context.and_then(|context| {
                context.handle.revoke_diagnostics_grant(&grant_id).map_err(|_| grant_failed())?;
                state.revoke_grant(&grant_id).map(|_| None::<DiagnosticsConfig>)
            })
        }
        DiagnosticsManagementRequest::RevokeAll => {
            if let Ok(context) = commands::primary_diagnostics_context(&control) { context.handle.revoke_all_diagnostics(); }
            state.revoke_all();
            Ok(None::<DiagnosticsConfig>)
        }
        DiagnosticsManagementRequest::Status => unreachable!(),
    };
    let error = outcome.as_ref().err().map(|error| DiagnosticsErrorView { code: error.code.to_string(), message: error.to_string() });
    state.complete_management_operation(&operation_id, action, outcome.as_ref().map(|_| ()).map_err(|error| error));
    let config = outcome.as_ref().ok().and_then(|config| config.clone());
    Ok(DiagnosticsResponse { snapshot: state.snapshot(), config, error })
}

fn now_ms() -> i64 { usage_core::report::now_ms() }

fn configure_local_mcp(app: &AppHandle, state: &DiagnosticsState, enabled: bool, ttl_ms: Option<i64>) -> Result<DiagnosticsStatus, DiagnosticsError> {
    if enabled {
        state.set_local_token_ttl(ttl_ms);
        if state.local_http_ttl_changed() {
            // A lifetime change re-mints: the old listener keeps the old clock.
            state.stop_local_http();
        }
        state.start_local_http(app).map_err(|_| DiagnosticsError::new(CODE_HANDLER_FAILED, "local MCP HTTP service could not start"))?;
        state.set_enabled(true);
    } else {
        if let Some(control) = app.try_state::<SyncControl>() {
            if let Ok(context) = commands::primary_diagnostics_context(&control) {
                context.handle.revoke_all_diagnostics();
            }
        }
        state.revoke_all();
        state.set_enabled(false);
        state.stop_local_http();
    }
    Ok(state.status())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> DiagnosticsState {
        let state = DiagnosticsState::default();
        state.set_enabled(true);
        state
    }

    fn session(expires_at_ms: i64) -> SessionRequest {
        SessionRequest {
            session_id: "session-1".into(),
            target_device: "device-1".into(),
            origin: "debugger-1".into(),
            scopes: vec!["diagnostics.read".into(), "sync.control".into()],
            issued_at_ms: now_ms().saturating_sub(1),
            expires_at_ms,
        }
    }

    #[test]
    fn diagnostics_are_disabled_until_explicitly_enabled() {
        let state = DiagnosticsState::default();
        let error = state.create_session(session(now_ms() + 1_000)).unwrap_err();
        assert_eq!(error.code, CODE_DIAGNOSTICS_DISABLED);
    }

    #[test]
    fn expiry_and_revoke_are_stable_fail_closed_codes() {
        let state = state();
        let mut expired = session(now_ms() + 1_000);
        expired.expires_at_ms = now_ms().saturating_sub(1);
        // Expired grants never become sessions, so use a valid session and
        // exercise the expiry gate with a request after its deadline.
        let request = session(now_ms() + 1_000);
        state.create_session(request.clone()).unwrap();
        let tool = ToolRequest {
            session_id: request.session_id.clone(),
            request_id: "request-1".into(),
            origin: request.origin.clone(),
            target_device: Some(request.target_device.clone()),
            tool: "device.get_status".into(),
            params: Value::Null,
        };
        {
            let mut inner = state.inner.lock().unwrap();
            inner.sessions.get_mut(&request.session_id).unwrap().view.expires_at_ms = now_ms().saturating_sub(1);
        }
        assert_eq!(state.authorize(&tool, Scope::Read).unwrap_err().code, CODE_SESSION_EXPIRED);
        state.revoke_all();
        assert_eq!(state.authorize(&tool, Scope::Read).unwrap_err().code, CODE_SESSION_REVOKED);
        let _ = expired;
    }

    #[test]
    fn tool_catalog_has_no_execution_or_path_surface() {
        let names = tool_definitions().into_iter().map(|tool| tool.name).collect::<Vec<_>>();
        assert!(!names.iter().any(|name| name.contains("shell") || name.contains("file")));
    }

    #[test]
    fn scopes_are_independent() {
        let state = state();
        let mut request = session(now_ms() + 1_000);
        request.scopes = vec!["diagnostics.read".into()];
        state.create_session(request.clone()).unwrap();
        let tool = ToolRequest { session_id: request.session_id, request_id: "request-1".into(), origin: request.origin, target_device: None, tool: "sync.start_primary".into(), params: Value::Null };
        assert_eq!(state.authorize(&tool, Scope::Control).unwrap_err().code, CODE_SCOPE_DENIED);
    }

    #[test]
    fn panel_session_lifecycle_exports_and_revokes_bounded_config() {
        let state = state();
        let session = state.create_session_for_panel(
            "target-device".into(),
            "debugger-device".into(),
            vec!["diagnostics.read".into()],
            15 * 60 * 1000,
        ).unwrap();
        assert!(session.session_id.starts_with("diag-session-"));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.sessions, vec![session.clone()]);
        let json = serde_json::to_string(&serde_json::json!({ "schema": "tokenme-diagnostics-local/v1", "session": session.clone() })).unwrap();
        assert!(json.contains("tokenme-diagnostics-local/v1"));
        assert!(!json.contains("password"));
        state.revoke_session(&session.session_id).unwrap();
        assert!(state.snapshot().sessions.is_empty());
    }

    #[test]
    fn local_mcp_image_marker_becomes_standard_image_content() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"image-bytes");
        let result = local_mcp_tool_result(json!({
            "_mcp_image": { "mime_type": "image/png", "data_base64": encoded },
            "width": 2,
            "height": 3,
        })).unwrap();
        assert_eq!(result["content"][0]["type"], "image");
        assert_eq!(result["content"][0]["mimeType"], "image/png");
        assert_eq!(result["structuredContent"]["width"], 2);
        assert!(result["structuredContent"].get("_mcp_image").is_none());
    }

    #[test]
    fn local_mcp_bearer_is_high_entropy_read_only_and_revoked_on_disable() {
        let state = state();
        let token = new_local_mcp_token();
        assert!(token.starts_with("tm_"));
        assert!(token.len() >= 40);
        assert_ne!(token, new_local_mcp_token());
        *state.local_http.lock().unwrap() = Some(LocalMcpHttpServer {
            endpoint: "http://192.0.2.1:47822/mcp".into(),
            read_token: token.clone(),
            expires_at_ms: now_ms() + LOCAL_MCP_TOKEN_TTL_MS,
            ttl_ms: LOCAL_MCP_TOKEN_TTL_MS,
            stop: Arc::new(AtomicBool::new(false)),
            control_enabled: false,
            join: None,
        });
        assert!(state.local_http_authorized(Some(&format!("Bearer {token}"))).is_ok());
        assert!(state.local_http_authorized(None).is_err());
        assert!(state.local_http_authorized(Some("Bearer wrong")).is_err());
        state.set_enabled(false);
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))).unwrap_err(), "mcp_disabled");
        state.set_enabled(true);
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))).unwrap_err(), "bearer_invalid");
    }

    #[test]
    fn local_mcp_bearer_expires_and_constant_time_compare_does_not_accept_prefixes() {
        assert!(constant_time_equal(b"same-token", b"same-token"));
        assert!(!constant_time_equal(b"same-token", b"same-token-extra"));
        let state = state();
        let token = new_local_mcp_token();
        *state.local_http.lock().unwrap() = Some(LocalMcpHttpServer {
            endpoint: "http://192.0.2.1:47822/mcp".into(),
            read_token: token.clone(),
            expires_at_ms: now_ms() - 1,
            ttl_ms: LOCAL_MCP_TOKEN_TTL_MS,
            stop: Arc::new(AtomicBool::new(false)),
            control_enabled: false,
            join: None,
        });
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))).unwrap_err(), "bearer_expired");
    }

    #[test]
    fn local_mcp_bearer_lifetime_is_fixed_at_seven_days() {
        let state = state();
        assert_eq!(state.local_token_ttl(), 7 * 24 * 60 * 60 * 1000);
        state.set_local_token_ttl(Some(1));
        assert_eq!(state.local_token_ttl(), 7 * 24 * 60 * 60 * 1000);
        state.set_local_token_ttl(Some(i64::MAX));
        assert_eq!(state.local_token_ttl(), 7 * 24 * 60 * 60 * 1000);
    }

    #[test]
    fn local_mcp_session_can_use_the_full_bearer_lifetime() {
        let state = lan_state();
        let ttl = state.local_http_session_ttl(Scope::Read);
        assert!(ttl > MAX_SESSION_TTL_MS - 1_000);
        assert!(state.create_session_for_panel(
            "local-http".into(), Scope::Read.local_origin().into(), Scope::Read.local_scopes(), ttl,
        ).is_ok());
    }

    #[test]
    fn unknown_tool_definition_scope_is_denied() {
        assert_eq!(tool_definition_scope("diagnostics.read"), Ok(Scope::Read));
        assert_eq!(tool_definition_scope("sync.control"), Ok(Scope::Control));
        assert_eq!(tool_definition_scope("sync.admin").unwrap_err().code, CODE_UNKNOWN_TOOL);
        assert_eq!(tool_definition_scope("").unwrap_err().code, CODE_UNKNOWN_TOOL);
    }

    #[test]
    fn local_mcp_tool_list_is_read_only() {
        let tools = local_tool_registry(Scope::Read);
        assert!(!tools.is_empty());
        assert!(tools.iter().all(|tool| tool_scope(&tool.name) == Ok(Scope::Read)));
        assert!(!tools.iter().any(|tool| tool.name == "sync.start_primary" || tool.name == "ui.click_panel"));
    }

    fn lan_state() -> DiagnosticsState {
        let state = state();
        *state.local_http.lock().unwrap() = Some(LocalMcpHttpServer {
            endpoint: "http://192.0.2.1:47822/mcp".into(), read_token: new_local_mcp_token(),
            expires_at_ms: now_ms() + LOCAL_MCP_TOKEN_TTL_MS, ttl_ms: LOCAL_MCP_TOKEN_TTL_MS, control_enabled: false,
            stop: Arc::new(AtomicBool::new(false)), join: None,
        });
        state
    }

    #[test]
    fn lan_control_switch_upgrades_the_single_credential_in_place() {
        let state = lan_state();
        let (_, token, expiry) = state.local_http_client_config().unwrap();
        assert!(!state.snapshot().lan_control_enabled);
        assert!(expiry <= now_ms() + LOCAL_MCP_TOKEN_TTL_MS);
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))), Ok(Scope::Read));
        state.configure_lan_control(true).unwrap();
        // 同一个 token：模式切换不换票，权限随开关移动
        assert_eq!(state.local_http_client_config().unwrap().1, token);
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))), Ok(Scope::Control));
        let read_session = state.create_session_for_panel("local-http".into(), Scope::Read.local_origin().into(), Scope::Read.local_scopes(), 1_000).unwrap();
        let control_session = state.create_session_for_panel("local-http".into(), Scope::Control.local_origin().into(), Scope::Control.local_scopes(), 1_000).unwrap();
        assert!(!state.local_http_session_active(&control_session.session_id, Scope::Read));
        assert!(!state.local_http_session_active(&read_session.session_id, Scope::Control));
        let request = ToolRequest { session_id: read_session.session_id, request_id: "bootstrap-1".into(), origin: Scope::Read.local_origin().into(), target_device: None, tool: "sync.start_primary".into(), params: json!({}) };
        assert_eq!(state.authorize(&request, tool_scope(&request.tool).unwrap()).unwrap_err().code, CODE_SCOPE_DENIED);
        let mut request = ToolRequest { session_id: control_session.session_id, origin: Scope::Control.local_origin().into(), ..request };
        assert!(state.authorize(&request, tool_scope(&request.tool).unwrap()).is_ok());
        request.origin = Scope::Read.local_origin().into();
        assert_eq!(state.authorize(&request, Scope::Control).unwrap_err().code, CODE_ORIGIN_MISMATCH);
        assert!(local_tool_registry(Scope::Control).iter().any(|tool| tool.name == "sync.start_primary"));
        assert_eq!(tool_scope("ui.open_sync_sheet"), Ok(Scope::Control));
    }

    #[test]
    fn lan_control_expiry_disable_and_reenable_revoke_sessions() {
        let state = lan_state();
        state.configure_lan_control(true).unwrap();
        let token = state.local_http_client_config().unwrap().1;
        let session = state.create_session_for_panel("local-http".into(), Scope::Control.local_origin().into(), Scope::Control.local_scopes(), 1_000).unwrap();
        state.local_http.lock().unwrap().as_mut().unwrap().expires_at_ms = now_ms() - 1;
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))), Err("bearer_expired"));
        assert!(!state.snapshot().lan_control_enabled);
        assert_eq!(state.local_http_session_ttl(Scope::Control), 0);
        state.configure_lan_control(true).unwrap();
        let replacement = state.local_http_client_config().unwrap().1;
        assert_ne!(token, replacement);
        assert!(!state.local_http_session_active(&session.session_id, Scope::Control));
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {replacement}"))), Ok(Scope::Control));
        state.configure_lan_control(false).unwrap();
        // off = read-only: 同一个 token 降级继续可用，不换票
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {replacement}"))), Ok(Scope::Read));
        state.set_enabled(false);
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {replacement}"))), Err("mcp_disabled"));
        let disabled = DiagnosticsState::default();
        assert_eq!(disabled.configure_lan_control(true).unwrap_err().code, CODE_DIAGNOSTICS_DISABLED);
    }

    #[test]
    fn lan_control_expiry_disable_and_reenable_revoke_old_tokens_and_sessions() {
        let state = lan_state();
        state.configure_lan_control(true).unwrap();
        let token = state.local_http_client_config().unwrap().1;
        let session = state.create_session_for_panel("local-http".into(), Scope::Control.local_origin().into(), Scope::Control.local_scopes(), 1_000).unwrap();
        state.local_http.lock().unwrap().as_mut().unwrap().expires_at_ms = now_ms() - 1;
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))), Err("bearer_expired"));
        assert!(!state.snapshot().lan_control_enabled);
        assert_eq!(state.local_http_session_ttl(Scope::Control), 0);
        state.configure_lan_control(true).unwrap();
        let replacement = state.local_http_client_config().unwrap().1;
        assert_ne!(token, replacement);
        assert!(!state.local_http_session_active(&session.session_id, Scope::Control));
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))), Err("bearer_invalid"));
        state.configure_lan_control(false).unwrap();
        // off = read-only: 同一个 token 降级继续可用，不换票
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {replacement}"))), Ok(Scope::Read));
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))), Err("bearer_invalid"), "the expired token stays dead");
        state.set_enabled(false);
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {replacement}"))), Err("mcp_disabled"));
        let disabled = DiagnosticsState::default();
        assert_eq!(disabled.configure_lan_control(true).unwrap_err().code, CODE_DIAGNOSTICS_DISABLED);
    }

    #[test]
    fn lan_bootstrap_survives_runtime_session_revocation_and_dispatches_start_parameters() {
        let state = lan_state();
        state.configure_lan_control(true).unwrap();
        let token = state.local_http_client_config().unwrap().1;
        let session = state.create_session_for_panel("local-http".into(), Scope::Control.local_origin().into(), Scope::Control.local_scopes(), 1_000).unwrap();
        // Stopping the primary revokes runtime sessions; the independent bootstrap stays usable.
        state.revoke_all();
        assert!(!state.local_http_session_active(&session.session_id, Scope::Control));
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))), Ok(Scope::Control));
        let restarted = state.create_session_for_panel("local-http".into(), Scope::Control.local_origin().into(), Scope::Control.local_scopes(), 1_000).unwrap();
        let request = ToolRequest { session_id: restarted.session_id, request_id: "restart-1".into(), origin: Scope::Control.local_origin().into(), target_device: None, tool: "sync.start_primary".into(), params: json!({"bind":"0.0.0.0:47821", "advertise_host":"192.0.2.1", "allow_lan":true}) };
        assert!(state.authorize(&request, tool_scope(&request.tool).unwrap()).is_ok());
        assert!(matches!(primary_start_request(&request.params).unwrap(), SyncManagementRequest::StartPrimary { bind: Some(bind), advertise_host: Some(host), allow_lan: true } if bind == "0.0.0.0:47821" && host == "192.0.2.1"));
        assert!(matches!(primary_start_request(&json!({})).unwrap(), SyncManagementRequest::StartPrimary { allow_lan: true, .. }));
        assert!(matches!(primary_start_request(&json!({"bind":"127.0.0.1:47821"})).unwrap(), SyncManagementRequest::StartPrimary { allow_lan: false, .. }));
        assert!(matches!(primary_start_request(&json!({"allow_lan":false})).unwrap(), SyncManagementRequest::StartPrimary { allow_lan: false, .. }));
        assert!(matches!(primary_start_request(&json!({"bind":"127.0.0.1:47821", "allow_lan":true})).unwrap(), SyncManagementRequest::StartPrimary { allow_lan: true, .. }));
        assert!(primary_start_request(&json!({"allow_lan":"true"})).is_err());
        assert!(primary_start_request(&json!({"unknown":true})).is_err());
        state.configure_lan_control(false).unwrap();
        // off = read-only: 同一个 token 降级继续可用，不换票
        assert_eq!(state.local_http_authorized(Some(&format!("Bearer {token}"))), Ok(Scope::Read));
    }

    #[test]
    fn stopped_sync_service_cannot_report_listener_or_tls_ok() {
        let status = SyncDevicesView {
            primary_id: Some("primary-1".into()),
            role: "primary".into(),
            service_running: false,
            service_health: Some("stopped".into()),
            endpoint: Some("https://127.0.0.1:47821".into()),
            certificate_pin: Some("a".repeat(64)),
            stream_id: None,
            data_epoch: None,
            pending: 0,
            in_flight: 0,
            pending_bytes: 0,
            last_error: None,
            discovery_error: None,
            last_connected_at_ms: None,
            operation_state: "offline".into(),
            last_checked_at_ms: None,
            invite_url: None,
            invite_expires_at_ms: None,
            discovered_primaries: Vec::new(),
            devices: Vec::new(),
            pairing_requests: Vec::new(),
        };
        let probes = sync_probe_results(&status, Err("database probe not run in this unit test".into()));
        assert!(probes.listener.is_err());
        assert!(probes.tls.is_err());
        assert_eq!(probes.listener.unwrap_err(), "sync service is not running");
        assert_eq!(probes.tls.unwrap_err(), "sync service is not running");
    }

    #[test]
    fn database_probe_reports_unconfigured_without_creating_a_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.db");
        let error = sync_database_probe(&path).unwrap_err();
        assert!(error.contains("no configured primary"));
        assert!(!path.exists());
    }
}

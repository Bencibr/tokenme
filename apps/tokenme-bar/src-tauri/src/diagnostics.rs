//! The Tauri-side diagnostics boundary.
//!
//! Transport, grant signing, and MCP JSON-RPC framing live outside this app
//! slice. This module owns the local fail-closed session gate and translates
//! the fixed protocol tool names into existing Tauri handlers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::sync::Mutex;

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

const MAX_SESSION_TTL_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_PARAMS_BYTES: usize = 32 * 1024;
const MAX_OPERATION_HISTORY: usize = 64;

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
    sessions: BTreeMap<String, Session>,
    operations: BTreeMap<String, OperationView>,
}

#[derive(Debug, Default)]
pub struct DiagnosticsState {
    inner: Mutex<Inner>,
}

impl DiagnosticsState {
    pub fn status(&self) -> DiagnosticsStatus {
        let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        DiagnosticsStatus {
            enabled: state.enabled,
            active_sessions: state.sessions.values().filter(|session| session.revoked.is_none()).count(),
            retained_operations: state.operations.len(),
        }
    }

    pub fn set_enabled(&self, enabled: bool) -> DiagnosticsStatus {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        state.enabled = enabled;
        if !enabled {
            for session in state.sessions.values_mut() {
                session.revoked = Some(CODE_DIAGNOSTICS_DISABLED);
            }
        }
        self.status_after(state)
    }

    fn status_after(&self, state: std::sync::MutexGuard<'_, Inner>) -> DiagnosticsStatus {
        DiagnosticsStatus {
            enabled: state.enabled,
            active_sessions: state.sessions.values().filter(|session| session.revoked.is_none()).count(),
            retained_operations: state.operations.len(),
        }
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
    }

    pub(crate) fn revoke_target(&self, target_device: &str) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        for session in state.sessions.values_mut().filter(|session| session.view.target_device == target_device) {
            session.revoked = Some(CODE_SESSION_REVOKED);
        }
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

    fn is_active(&self, session_id: &str) -> bool {
        let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        state.enabled && state.sessions.get(session_id).is_some_and(|session| session.revoked.is_none() && now_ms() < session.view.expires_at_ms)
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
        ("sync.start_primary", "Start the existing primary sync service", "sync.control"),
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
    ]
    .into_iter()
    .map(|(name, description, scope)| ToolDefinition { name, description, scope })
    .collect()
}

fn tool_scope(tool: &str) -> Result<Scope, DiagnosticsError> {
    if tool_definitions().iter().any(|definition| definition.name == tool) {
        if tool.starts_with("sync.") && !matches!(tool, "sync.get_status" | "sync.get_requests" | "sync.get_trace") {
            return Ok(Scope::Control);
        }
        return Ok(Scope::Read);
    }
    Err(DiagnosticsError::new(CODE_UNKNOWN_TOOL, "diagnostics tool is not registered"))
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
            let log = string(params, "log")?;
            let lines = number(params, "lines", 50, 1, 200)? as usize;
            let since_ms = optional_number(params, "since_ms")?;
            let lines = logging::diagnostic_log_tail(log, lines, since_ms).map_err(|_| handler_error())?;
            Ok(json!({ "log": log, "lines": lines }))
        }
        "logs.search" => {
            let params = object(&request.params)?;
            let log = string(params, "log")?;
            let query = string(params, "query")?;
            let since_ms = optional_number(params, "since_ms")?;
            let limit = number(params, "limit", 50, 1, 100)? as usize;
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
            let limit = number(params, "limit", 50, 1, 100)? as usize;
            let lines = logging::diagnostic_log_tail("diagnostics", limit, None).map_err(|_| handler_error())?;
            Ok(json!({ "events": lines }))
        }
        "diagnostics.run_check" => run_checks(app, &request.params).await,
        "diagnostics.get_operation" => {
            let params = object(&request.params)?;
            let operation_id = string(params, "operation_id")?;
            diagnostics.operation(operation_id, &session.view.session_id)
        }
        "app.get_panel_state" => Ok(json!({
            "panel_open": !panel::is_panel_hidden(),
            "current_page": Value::Null,
            "current_page_known": false,
            "background_tasks": [],
        })),
        "sync.start_primary" => {
            let params = object_or_empty(&request.params)?;
            let bind = optional_string(params, "bind")?;
            let advertise_host = optional_string(params, "advertise_host")?;
            let allow_lan = optional_bool(params, "allow_lan")?.unwrap_or(false);
            control_sync(app, SyncManagementRequest::StartPrimary { bind, advertise_host, allow_lan }).await
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
            app.emit("sync-open-sheet", ()).map_err(|_| handler_error())?;
            Ok(json!({ "opened": true }))
        }
        "ui.focus_device" => {
            let device_id = required_identifier(&request.params, "device_id")?;
            panel::show(app, None);
            app.emit("sync-focus-device", device_id).map_err(|_| handler_error())?;
            Ok(json!({ "opened": true, "device_id": device_id }))
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
    let checks = match params.get("checks") {
        None => vec!["listener", "tls", "pin", "database", "ipc"],
        Some(Value::Array(values)) => values.iter().map(|value| value.as_str().ok_or_else(|| invalid_params())).collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(invalid_params()),
    };
    if checks.len() > 5 || checks.iter().any(|check| !matches!(*check, "listener" | "tls" | "pin" | "database" | "ipc")) {
        return Err(invalid_params());
    }
    let sync = if checks.iter().any(|check| matches!(*check, "listener" | "tls" | "pin" | "database")) {
        Some(sync_status(app).await?)
    } else {
        None
    };
    let results = checks.into_iter().map(|check| {
        let (ok, detail) = match check {
            "listener" => (sync.as_ref().is_some_and(|status| !status.service_running || status.role == "primary" || status.role == "secondary"), "listener state readable"),
            "tls" => (sync.is_some(), "TLS state readable"),
            "pin" => (sync.as_ref().is_some_and(|status| status.certificate_pin.as_ref().is_none_or(|pin| pin.len() == 64 && pin.bytes().all(|byte| byte.is_ascii_hexdigit()))), "certificate pin shape valid"),
            "database" => (sync.is_some(), "sync database readable"),
            "ipc" => (true, "Tauri adapter reachable"),
            _ => (false, "check unavailable"),
        };
        json!({ "check": check, "ok": ok, "detail": detail })
    }).collect::<Vec<_>>();
    Ok(json!({ "checks": results }))
}

fn safe_sync_view(status: &SyncDevicesView) -> Value {
    json!({
        "primary_id": status.primary_id,
        "role": status.role,
        "service_running": status.service_running,
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

#[tauri::command]
pub fn set_diagnostics_enabled(enabled: bool, state: State<'_, DiagnosticsState>) -> Result<DiagnosticsStatus, String> {
    Ok(state.set_enabled(enabled))
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

fn now_ms() -> i64 { usage_core::report::now_ms() }

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
        assert!(!names.iter().any(|name| name.contains("shell") || name.contains("file") || name.contains("click")));
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
}

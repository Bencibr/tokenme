//! Bounded remote-diagnostics MCP protocol primitives.
//!
//! This module owns the protocol boundary only. Grants and sessions are
//! intentionally in memory: the Tauri owner still has to provide the user
//! consent UI, durable audit sink, log redaction source, and semantic sync/UI
//! handlers before those tools can be enabled. An unimplemented tool returns a
//! typed error; it is never treated as a successful no-op.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::protocol::{hash_bytes, identity_bytes, DeviceIdentity};
use crate::store::{DeviceStatus, SyncError, SyncStore};

pub const DIAGNOSTICS_MCP_PATH: &str = "/diagnostics/mcp";
pub const DIAGNOSTICS_PROTOCOL_VERSION: &str = "2025-06-18";
pub const MAX_DIAGNOSTICS_BODY_BYTES: usize = 256 * 1024;
pub const MAX_DIAGNOSTICS_PARAMS_BYTES: usize = 64 * 1024;
pub const MAX_DIAGNOSTICS_JSON_DEPTH: usize = 12;
pub const MAX_DIAGNOSTICS_STRING_BYTES: usize = 16 * 1024;
pub const MAX_DIAGNOSTICS_ARRAY_ITEMS: usize = 64;
pub const MAX_DIAGNOSTICS_OBJECT_ITEMS: usize = 64;
pub const MAX_DIAGNOSTICS_REQUEST_TTL_MS: i64 = 30_000;
pub const MAX_DIAGNOSTICS_GRANT_TTL_MS: i64 = 60 * 60 * 1000;
pub const MAX_DIAGNOSTICS_SESSION_TTL_MS: i64 = 60 * 60 * 1000;
pub const MAX_DIAGNOSTICS_GRANTS: usize = 64;
pub const MAX_DIAGNOSTICS_SESSIONS: usize = 16;
pub const MAX_DIAGNOSTICS_REPLAY_ENTRIES: usize = 1024;
pub const MAX_DIAGNOSTICS_AUDIT_EVENTS: usize = 256;
pub const MAX_DIAGNOSTICS_CONCURRENT_REQUESTS: usize = 2;
pub const DIAGNOSTICS_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

const GRANT_DOMAIN: &[u8] = b"tokenme-diagnostics/grant/v1";
const REQUEST_DOMAIN: &[u8] = b"tokenme-diagnostics/request/v1";

mod signature_serde {
    use super::*;

    pub fn serialize<S>(value: &[u8; 64], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(value))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 64], D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(value)
            .map_err(D::Error::custom)?;
        bytes.try_into().map_err(|_| D::Error::custom("diagnostics signature must be 64 bytes"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagnosticsScope {
    Read,
    SyncControl,
}

impl DiagnosticsScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "diagnostics.read",
            Self::SyncControl => "sync.control",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "diagnostics.read" => Some(Self::Read),
            "sync.control" => Some(Self::SyncControl),
            _ => None,
        }
    }
}

impl Serialize for DiagnosticsScope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DiagnosticsScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).ok_or_else(|| D::Error::custom("unknown diagnostics scope"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticsErrorCode {
    InvalidRequest,
    InvalidParams,
    Unauthorized,
    GrantExpired,
    SessionExpired,
    SessionRevoked,
    Replay,
    ScopeDenied,
    Limit,
    Timeout,
    Busy,
    UnknownMethod,
    UnknownTool,
    ToolUnavailable,
    Stopped,
    Store,
    Internal,
}

impl DiagnosticsErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidParams => "invalid_params",
            Self::Unauthorized => "unauthorized",
            Self::GrantExpired => "grant_expired",
            Self::SessionExpired => "session_expired",
            Self::SessionRevoked => "session_revoked",
            Self::Replay => "replay_rejected",
            Self::ScopeDenied => "scope_denied",
            Self::Limit => "limit_exceeded",
            Self::Timeout => "timeout",
            Self::Busy => "busy",
            Self::UnknownMethod => "method_not_found",
            Self::UnknownTool => "tool_not_found",
            Self::ToolUnavailable => "tool_unavailable",
            Self::Stopped => "service_stopped",
            Self::Store => "store_unavailable",
            Self::Internal => "internal_error",
        }
    }

    const fn rpc_code(self) -> i32 {
        match self {
            Self::InvalidRequest => -32600,
            Self::InvalidParams => -32602,
            Self::UnknownMethod | Self::UnknownTool => -32601,
            Self::Unauthorized => -32001,
            Self::GrantExpired => -32002,
            Self::SessionExpired | Self::SessionRevoked => -32003,
            Self::Replay => -32004,
            Self::ScopeDenied => -32005,
            Self::Limit => -32006,
            Self::Timeout => -32007,
            Self::Busy => -32008,
            Self::ToolUnavailable => -32009,
            Self::Stopped => -32010,
            Self::Store | Self::Internal => -32099,
        }
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticsError {
    #[error("invalid diagnostics request")]
    InvalidRequest,
    #[error("invalid diagnostics parameters")]
    InvalidParams,
    #[error("diagnostics authentication failed")]
    Unauthorized,
    #[error("diagnostics grant expired")]
    GrantExpired,
    #[error("diagnostics session expired")]
    SessionExpired,
    #[error("diagnostics session revoked")]
    SessionRevoked,
    #[error("diagnostics request replay rejected")]
    Replay,
    #[error("diagnostics scope denied")]
    ScopeDenied,
    #[error("diagnostics request exceeds a configured limit")]
    Limit,
    #[error("diagnostics request timed out")]
    Timeout,
    #[error("diagnostics service is busy")]
    Busy,
    #[error("diagnostics MCP method is not supported")]
    UnknownMethod,
    #[error("diagnostics tool is not registered")]
    UnknownTool,
    #[error("diagnostics tool integration is not available")]
    ToolUnavailable,
    #[error("diagnostics service is stopped")]
    Stopped,
    #[error("diagnostics store is unavailable")]
    Store,
    #[error("diagnostics internal error")]
    Internal,
}

impl DiagnosticsError {
    pub const fn code(self) -> DiagnosticsErrorCode {
        match self {
            Self::InvalidRequest => DiagnosticsErrorCode::InvalidRequest,
            Self::InvalidParams => DiagnosticsErrorCode::InvalidParams,
            Self::Unauthorized => DiagnosticsErrorCode::Unauthorized,
            Self::GrantExpired => DiagnosticsErrorCode::GrantExpired,
            Self::SessionExpired => DiagnosticsErrorCode::SessionExpired,
            Self::SessionRevoked => DiagnosticsErrorCode::SessionRevoked,
            Self::Replay => DiagnosticsErrorCode::Replay,
            Self::ScopeDenied => DiagnosticsErrorCode::ScopeDenied,
            Self::Limit => DiagnosticsErrorCode::Limit,
            Self::Timeout => DiagnosticsErrorCode::Timeout,
            Self::Busy => DiagnosticsErrorCode::Busy,
            Self::UnknownMethod => DiagnosticsErrorCode::UnknownMethod,
            Self::UnknownTool => DiagnosticsErrorCode::UnknownTool,
            Self::ToolUnavailable => DiagnosticsErrorCode::ToolUnavailable,
            Self::Stopped => DiagnosticsErrorCode::Stopped,
            Self::Store => DiagnosticsErrorCode::Store,
            Self::Internal => DiagnosticsErrorCode::Internal,
        }
    }

    pub const fn http_status(self) -> u16 {
        match self {
            Self::InvalidRequest | Self::InvalidParams => 400,
            Self::Unauthorized | Self::GrantExpired | Self::SessionExpired | Self::SessionRevoked => 401,
            Self::ScopeDenied => 403,
            Self::Replay => 409,
            Self::Limit => 413,
            Self::Timeout => 408,
            Self::Busy => 429,
            Self::Stopped | Self::Store | Self::Internal => 503,
            Self::UnknownMethod | Self::UnknownTool | Self::ToolUnavailable => 200,
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid diagnostics request",
            Self::InvalidParams => "invalid diagnostics parameters",
            Self::Unauthorized => "diagnostics authentication failed",
            Self::GrantExpired => "diagnostics grant expired",
            Self::SessionExpired => "diagnostics session expired",
            Self::SessionRevoked => "diagnostics session revoked",
            Self::Replay => "diagnostics request replay rejected",
            Self::ScopeDenied => "diagnostics scope denied",
            Self::Limit => "diagnostics request exceeds a configured limit",
            Self::Timeout => "diagnostics request timed out",
            Self::Busy => "diagnostics service is busy",
            Self::UnknownMethod => "diagnostics MCP method is not supported",
            Self::UnknownTool => "diagnostics tool is not registered",
            Self::ToolUnavailable => "diagnostics tool integration is not available",
            Self::Stopped => "diagnostics service is stopped",
            Self::Store => "diagnostics store is unavailable",
            Self::Internal => "diagnostics internal error",
        }
    }

    pub(crate) fn rpc_response(self, id: Option<Value>) -> DiagnosticsMcpResponse {
        DiagnosticsMcpResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(DiagnosticsRpcError {
                code: self.code().rpc_code(),
                message: self.message().into(),
                data: Some(serde_json::json!({ "errorCode": self.code().as_str() })),
            }),
        }
    }
}

impl From<SyncError> for DiagnosticsError {
    fn from(_: SyncError) -> Self {
        Self::Store
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsGrant {
    pub grant_id: String,
    pub target_device_id: String,
    pub debugger_device_id: String,
    pub debugger_origin: String,
    pub scopes: Vec<DiagnosticsScope>,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    #[serde(with = "signature_serde")]
    pub signature: [u8; 64],
}

impl PartialEq for DiagnosticsGrant {
    fn eq(&self, other: &Self) -> bool {
        self.grant_id == other.grant_id
            && self.target_device_id == other.target_device_id
            && self.debugger_device_id == other.debugger_device_id
            && self.debugger_origin == other.debugger_origin
            && self.scopes == other.scopes
            && self.issued_at_ms == other.issued_at_ms
            && self.expires_at_ms == other.expires_at_ms
            && self.signature == other.signature
    }
}

impl Eq for DiagnosticsGrant {}

impl DiagnosticsGrant {
    fn signing_bytes(&self) -> Vec<u8> {
        let scopes = self.scopes.iter().map(|scope| scope.as_str()).collect::<Vec<_>>().join(",");
        identity_bytes(
            GRANT_DOMAIN,
            &[
                self.grant_id.as_bytes(),
                self.target_device_id.as_bytes(),
                self.debugger_device_id.as_bytes(),
                self.debugger_origin.as_bytes(),
                scopes.as_bytes(),
                &self.issued_at_ms.to_be_bytes(),
                &self.expires_at_ms.to_be_bytes(),
            ],
        )
    }

    fn validate(&self, target_device_id: &str, now_ms: i64) -> Result<(), DiagnosticsError> {
        if self.target_device_id != target_device_id
            || self.grant_id.is_empty()
            || self.grant_id.len() > 128
            || self.debugger_device_id.is_empty()
            || self.debugger_origin.is_empty()
            || self.scopes.is_empty()
            || self.expires_at_ms <= self.issued_at_ms
            || self.expires_at_ms - self.issued_at_ms > MAX_DIAGNOSTICS_GRANT_TTL_MS
            || now_ms >= self.expires_at_ms
        {
            return Err(if now_ms >= self.expires_at_ms { DiagnosticsError::GrantExpired } else { DiagnosticsError::Unauthorized });
        }
        if self.scopes.windows(2).any(|scopes| scopes[0] >= scopes[1]) {
            return Err(DiagnosticsError::Unauthorized);
        }
        Ok(())
    }
}

pub struct DiagnosticsAuthority {
    target_device_id: String,
    signing_key: SigningKey,
}

impl DiagnosticsAuthority {
    pub fn new(target_device_id: impl Into<String>) -> Result<Self, DiagnosticsError> {
        let target_device_id = target_device_id.into();
        if target_device_id.is_empty() || target_device_id.len() > 128 {
            return Err(DiagnosticsError::InvalidRequest);
        }
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|_| DiagnosticsError::Internal)?;
        Ok(Self { target_device_id, signing_key: SigningKey::from_bytes(&secret) })
    }

    pub fn target_device_id(&self) -> &str {
        &self.target_device_id
    }

    fn issue_grant(
        &self,
        debugger_device_id: &str,
        debugger_origin: &str,
        mut scopes: Vec<DiagnosticsScope>,
        issued_at_ms: i64,
        ttl_ms: i64,
    ) -> Result<DiagnosticsGrant, DiagnosticsError> {
        if debugger_device_id.is_empty() || debugger_origin.is_empty() || ttl_ms <= 0 || ttl_ms > MAX_DIAGNOSTICS_GRANT_TTL_MS {
            return Err(DiagnosticsError::InvalidRequest);
        }
        scopes.sort_unstable();
        scopes.dedup();
        if scopes.is_empty() {
            return Err(DiagnosticsError::ScopeDenied);
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| DiagnosticsError::Internal)?;
        let grant_id = format!("dgrant-{}", hex(&random));
        let mut grant = DiagnosticsGrant {
            grant_id,
            target_device_id: self.target_device_id.clone(),
            debugger_device_id: debugger_device_id.into(),
            debugger_origin: debugger_origin.into(),
            scopes,
            issued_at_ms,
            expires_at_ms: issued_at_ms.checked_add(ttl_ms).ok_or(DiagnosticsError::InvalidRequest)?,
            signature: [0; 64],
        };
        grant.signature = self.signing_key.sign(&grant.signing_bytes()).to_bytes();
        Ok(grant)
    }

}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsRequestAuth {
    pub grant_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub debugger_device_id: String,
    pub origin: String,
    pub request_id: String,
    pub nonce: [u8; 16],
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    #[serde(with = "signature_serde")]
    pub signature: [u8; 64],
}

impl DiagnosticsRequestAuth {
    pub fn new_signed(
        grant: &DiagnosticsGrant,
        identity: &DeviceIdentity,
        session_id: Option<String>,
        method: &str,
        params: &Value,
        request_id: impl Into<String>,
        issued_at_ms: i64,
        expires_at_ms: i64,
    ) -> Result<Self, DiagnosticsError> {
        if grant.debugger_device_id != identity.device_id || grant.debugger_origin != identity.origin {
            return Err(DiagnosticsError::Unauthorized);
        }
        let request_id = request_id.into();
        if request_id.is_empty() || request_id.len() > 128 || method.is_empty() {
            return Err(DiagnosticsError::InvalidRequest);
        }
        if expires_at_ms <= issued_at_ms || expires_at_ms - issued_at_ms > MAX_DIAGNOSTICS_REQUEST_TTL_MS {
            return Err(DiagnosticsError::InvalidRequest);
        }
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| DiagnosticsError::Internal)?;
        let mut auth = Self {
            grant_id: grant.grant_id.clone(),
            session_id,
            debugger_device_id: identity.device_id.clone(),
            origin: identity.origin.clone(),
            request_id,
            nonce,
            issued_at_ms,
            expires_at_ms,
            signature: [0; 64],
        };
        auth.signature = identity.signing_key.sign(&auth.signing_bytes(&grant.target_device_id, method, params)).to_bytes();
        Ok(auth)
    }

    fn signing_bytes(&self, target_device_id: &str, method: &str, params: &Value) -> Vec<u8> {
        let params_hash = hash_bytes(&canonical_json(params));
        identity_bytes(
            REQUEST_DOMAIN,
            &[
                self.grant_id.as_bytes(),
                self.session_id.as_deref().unwrap_or_default().as_bytes(),
                self.debugger_device_id.as_bytes(),
                self.origin.as_bytes(),
                self.request_id.as_bytes(),
                &self.nonce,
                &self.issued_at_ms.to_be_bytes(),
                &self.expires_at_ms.to_be_bytes(),
                target_device_id.as_bytes(),
                method.as_bytes(),
                &params_hash,
            ],
        )
    }

    fn verify(&self, target_device_id: &str, method: &str, params: &Value, public_key: &[u8; 32]) -> Result<(), DiagnosticsError> {
        VerifyingKey::from_bytes(public_key)
            .map_err(|_| DiagnosticsError::Unauthorized)?
            .verify(&self.signing_bytes(target_device_id, method, params), &Signature::from_bytes(&self.signature))
            .map_err(|_| DiagnosticsError::Unauthorized)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiagnosticsMcpRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default = "empty_object")]
    pub params: Value,
    pub auth: DiagnosticsRequestAuth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsMcpResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DiagnosticsRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsAuditEvent {
    pub event: String,
    pub request_id: Option<String>,
    pub origin: Option<String>,
    pub scope: Option<DiagnosticsScope>,
    pub result_code: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticsToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolId {
    DeviceGetStatus,
    SyncGetStatus,
    SyncGetRequests,
    Other,
}

#[derive(Debug, Clone, Copy)]
struct ToolSpec {
    id: ToolId,
    name: &'static str,
    scope: DiagnosticsScope,
    description: &'static str,
}

const TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec { id: ToolId::DeviceGetStatus, name: "device.get_status", scope: DiagnosticsScope::Read, description: "Return bounded device and service status." },
    ToolSpec { id: ToolId::Other, name: "device.get_build", scope: DiagnosticsScope::Read, description: "Return a safe build summary when the host integration is connected." },
    ToolSpec { id: ToolId::Other, name: "logs.tail", scope: DiagnosticsScope::Read, description: "Read a bounded, redacted tail from an allow-listed log." },
    ToolSpec { id: ToolId::Other, name: "logs.search", scope: DiagnosticsScope::Read, description: "Search an allow-listed log with bounded redacted output." },
    ToolSpec { id: ToolId::SyncGetStatus, name: "sync.get_status", scope: DiagnosticsScope::Read, description: "Return bounded synchronization status." },
    ToolSpec { id: ToolId::SyncGetRequests, name: "sync.get_requests", scope: DiagnosticsScope::Read, description: "Return bounded pairing-request summaries." },
    ToolSpec { id: ToolId::Other, name: "sync.get_trace", scope: DiagnosticsScope::Read, description: "Return a bounded synchronization trace when the host integration is connected." },
    ToolSpec { id: ToolId::Other, name: "diagnostics.run_check", scope: DiagnosticsScope::Read, description: "Run fixed diagnostics checks when the host integration is connected." },
    ToolSpec { id: ToolId::Other, name: "diagnostics.get_operation", scope: DiagnosticsScope::Read, description: "Read a bounded operation result when the host integration is connected." },
    ToolSpec { id: ToolId::Other, name: "app.get_panel_state", scope: DiagnosticsScope::Read, description: "Return panel state when the host integration is connected." },
    ToolSpec { id: ToolId::Other, name: "sync.start_primary", scope: DiagnosticsScope::SyncControl, description: "Start the primary service through the host state machine." },
    ToolSpec { id: ToolId::Other, name: "sync.stop_primary", scope: DiagnosticsScope::SyncControl, description: "Stop the primary service and revoke diagnostics sessions." },
    ToolSpec { id: ToolId::Other, name: "sync.discover", scope: DiagnosticsScope::SyncControl, description: "Start one bounded LAN discovery operation." },
    ToolSpec { id: ToolId::Other, name: "sync.approve_pairing", scope: DiagnosticsScope::SyncControl, description: "Approve one pairing request through the host handler." },
    ToolSpec { id: ToolId::Other, name: "sync.reject_pairing", scope: DiagnosticsScope::SyncControl, description: "Reject one pairing request through the host handler." },
    ToolSpec { id: ToolId::Other, name: "sync.clear_expired_request", scope: DiagnosticsScope::SyncControl, description: "Clear one expired pairing request through the host handler." },
    ToolSpec { id: ToolId::Other, name: "sync.pause", scope: DiagnosticsScope::SyncControl, description: "Pause network synchronization while retaining local capture." },
    ToolSpec { id: ToolId::Other, name: "sync.resume", scope: DiagnosticsScope::SyncControl, description: "Resume network synchronization through the host handler." },
    ToolSpec { id: ToolId::Other, name: "sync.revoke_device", scope: DiagnosticsScope::SyncControl, description: "Revoke one paired device through the host handler." },
    ToolSpec { id: ToolId::Other, name: "sync.refresh", scope: DiagnosticsScope::SyncControl, description: "Refresh synchronization state through the host handler." },
    ToolSpec { id: ToolId::Other, name: "ui.open_sync_sheet", scope: DiagnosticsScope::SyncControl, description: "Request the semantic sync-sheet UI intent through the host handler." },
    ToolSpec { id: ToolId::Other, name: "ui.focus_device", scope: DiagnosticsScope::SyncControl, description: "Request the semantic device-focus UI intent through the host handler." },
];

pub fn diagnostics_tool_registry() -> Vec<DiagnosticsToolDefinition> {
    TOOL_SPECS
        .iter()
        .map(|spec| DiagnosticsToolDefinition {
            name: spec.name.into(),
            description: spec.description.into(),
            input_schema: tool_schema(spec.name),
        })
        .collect()
}

#[derive(Clone)]
pub struct DiagnosticsService {
    state: Arc<Mutex<DiagnosticsState>>,
}

struct DiagnosticsState {
    authority: DiagnosticsAuthority,
    grants: BTreeMap<String, DiagnosticsGrant>,
    sessions: BTreeMap<String, SessionState>,
    replay: BTreeMap<(String, String), i64>,
    replay_order: VecDeque<(String, String)>,
    audit: VecDeque<DiagnosticsAuditEvent>,
    active_requests: usize,
    stopped: bool,
}

#[derive(Debug, Clone)]
struct SessionState {
    grant_id: String,
    debugger_device_id: String,
    origin: String,
    expires_at_ms: i64,
}

pub struct DiagnosticsRequestPermit {
    state: Arc<Mutex<DiagnosticsState>>,
}

impl Drop for DiagnosticsRequestPermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.active_requests = state.active_requests.saturating_sub(1);
        }
    }
}

impl DiagnosticsService {
    pub fn new(target_device_id: impl Into<String>) -> Result<Self, DiagnosticsError> {
        Ok(Self {
            state: Arc::new(Mutex::new(DiagnosticsState {
                authority: DiagnosticsAuthority::new(target_device_id)?,
                grants: BTreeMap::new(),
                sessions: BTreeMap::new(),
                replay: BTreeMap::new(),
                replay_order: VecDeque::new(),
                audit: VecDeque::new(),
                active_requests: 0,
                stopped: false,
            })),
        })
    }

    pub fn target_device_id(&self) -> Result<String, DiagnosticsError> {
        let state = self.state.lock().map_err(|_| DiagnosticsError::Internal)?;
        Ok(state.authority.target_device_id().into())
    }

    pub fn issue_grant(
        &self,
        store: &SyncStore,
        debugger_device_id: &str,
        scopes: &[DiagnosticsScope],
        issued_at_ms: i64,
        ttl_ms: i64,
    ) -> Result<DiagnosticsGrant, DiagnosticsError> {
        let device = store.device(debugger_device_id)?.ok_or(DiagnosticsError::Unauthorized)?;
        if device.status != DeviceStatus::Paired {
            return Err(if device.status == DeviceStatus::Revoked { DiagnosticsError::SessionRevoked } else { DiagnosticsError::Unauthorized });
        }
        let mut state = self.state.lock().map_err(|_| DiagnosticsError::Internal)?;
        if state.stopped {
            return Err(DiagnosticsError::Stopped);
        }
        if state.grants.len() >= MAX_DIAGNOSTICS_GRANTS {
            return Err(DiagnosticsError::Busy);
        }
        let grant = state.authority.issue_grant(
            &device.device_id,
            &device.origin,
            scopes.to_vec(),
            issued_at_ms,
            ttl_ms,
        )?;
        state.grants.insert(grant.grant_id.clone(), grant.clone());
        Ok(grant)
    }

    pub fn revoke_grant(&self, grant_id: &str) -> Result<(), DiagnosticsError> {
        let mut state = self.state.lock().map_err(|_| DiagnosticsError::Internal)?;
        let Some(grant) = state.grants.remove(grant_id) else {
            return Err(DiagnosticsError::Unauthorized);
        };
        state.sessions.retain(|_, session| session.grant_id != grant.grant_id);
        Ok(())
    }

    pub fn revoke_all(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.grants.clear();
            state.sessions.clear();
        }
    }

    pub fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
            state.grants.clear();
            state.sessions.clear();
        }
    }

    pub fn audit_events(&self) -> Result<Vec<DiagnosticsAuditEvent>, DiagnosticsError> {
        let state = self.state.lock().map_err(|_| DiagnosticsError::Internal)?;
        Ok(state.audit.iter().cloned().collect())
    }

    pub fn enter(&self) -> Result<DiagnosticsRequestPermit, DiagnosticsError> {
        let mut state = self.state.lock().map_err(|_| DiagnosticsError::Internal)?;
        if state.stopped {
            return Err(DiagnosticsError::Stopped);
        }
        if state.active_requests >= MAX_DIAGNOSTICS_CONCURRENT_REQUESTS {
            return Err(DiagnosticsError::Busy);
        }
        state.active_requests += 1;
        Ok(DiagnosticsRequestPermit { state: Arc::clone(&self.state) })
    }

    pub fn handle_json(
        &self,
        store: &SyncStore,
        body: &[u8],
        now_ms: i64,
        deadline: Instant,
    ) -> Result<Option<DiagnosticsMcpResponse>, DiagnosticsError> {
        if body.len() > MAX_DIAGNOSTICS_BODY_BYTES {
            return Err(DiagnosticsError::Limit);
        }
        if Instant::now() >= deadline {
            return Err(DiagnosticsError::Timeout);
        }
        let request: DiagnosticsMcpRequest = serde_json::from_slice(body).map_err(|_| DiagnosticsError::InvalidRequest)?;
        validate_request(&request)?;
        let _permit = self.enter()?;
        self.handle_request(store, &request, now_ms, deadline)
    }

    pub fn handle_request(
        &self,
        store: &SyncStore,
        request: &DiagnosticsMcpRequest,
        now_ms: i64,
        deadline: Instant,
    ) -> Result<Option<DiagnosticsMcpResponse>, DiagnosticsError> {
        let request_id = Some(request.auth.request_id.clone());
        let origin = Some(request.auth.origin.clone());
        let result = self.handle_request_inner(store, request, now_ms, deadline);
        if let Err(error) = result {
            self.record_audit(DiagnosticsAuditEvent {
                event: "diagnostics.request.denied".into(),
                request_id,
                origin,
                scope: None,
                result_code: error.code().as_str().into(),
                at_ms: now_ms,
            });
        }
        result
    }

    fn handle_request_inner(
        &self,
        store: &SyncStore,
        request: &DiagnosticsMcpRequest,
        now_ms: i64,
        deadline: Instant,
    ) -> Result<Option<DiagnosticsMcpResponse>, DiagnosticsError> {
        if Instant::now() >= deadline {
            return Err(DiagnosticsError::Timeout);
        }
        let spec = if request.method == "tools/call" {
            tool_call_spec(&request.params)?.map(|(spec, _)| (spec.name, spec.scope))
        } else {
            None
        };
        let session_id = self.authorize(store, request, now_ms, spec.map(|(_, scope)| scope), deadline)?;
        let id = request.id.clone();
        match request.method.as_str() {
            "initialize" => {
                let session_id = session_id.ok_or(DiagnosticsError::Internal)?;
                let result = serde_json::json!({
                    "protocolVersion": DIAGNOSTICS_PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "tokenme-diagnostics", "version": env!("CARGO_PKG_VERSION") },
                    "sessionId": session_id,
                });
                Ok(Some(success_response(id, result)))
            }
            "notifications/initialized" => Ok(None),
            "tools/list" => Ok(Some(success_response(id, serde_json::json!({ "tools": diagnostics_tool_registry() })))),
            "tools/call" => {
                let (spec, arguments) = tool_call_spec(&request.params)?.ok_or(DiagnosticsError::UnknownTool)?;
                let value = self.call_tool(store, spec, arguments, now_ms, deadline)?;
                self.record_audit(DiagnosticsAuditEvent {
                    event: "diagnostics.operation.completed".into(),
                    request_id: Some(request.auth.request_id.clone()),
                    origin: Some(request.auth.origin.clone()),
                    scope: Some(spec.scope),
                    result_code: "ok".into(),
                    at_ms: now_ms,
                });
                let text = serde_json::to_string(&value).map_err(|_| DiagnosticsError::Internal)?;
                Ok(Some(success_response(
                    id,
                    serde_json::json!({
                        "content": [{ "type": "text", "text": text }],
                        "structuredContent": value,
                    }),
                )))
            }
            _ => Err(DiagnosticsError::UnknownMethod),
        }
    }

    fn authorize(
        &self,
        store: &SyncStore,
        request: &DiagnosticsMcpRequest,
        now_ms: i64,
        required_scope: Option<DiagnosticsScope>,
        deadline: Instant,
    ) -> Result<Option<String>, DiagnosticsError> {
        if Instant::now() >= deadline {
            return Err(DiagnosticsError::Timeout);
        }
        let (grant, target_device_id) = {
            let state = self.state.lock().map_err(|_| DiagnosticsError::Internal)?;
            if state.stopped {
                return Err(DiagnosticsError::Stopped);
            }
            let grant = state.grants.get(&request.auth.grant_id).cloned().ok_or(DiagnosticsError::Unauthorized)?;
            (grant, state.authority.target_device_id().to_string())
        };
        let authority = {
            let state = self.state.lock().map_err(|_| DiagnosticsError::Internal)?;
            state.authority.signing_key.verifying_key()
        };
        grant.validate(&target_device_id, now_ms)?;
        authority.verify(&grant.signing_bytes(), &Signature::from_bytes(&grant.signature)).map_err(|_| DiagnosticsError::Unauthorized)?;
        if grant.debugger_device_id != request.auth.debugger_device_id || grant.debugger_origin != request.auth.origin {
            return Err(DiagnosticsError::Unauthorized);
        }
        if required_scope.is_some_and(|scope| !grant.scopes.contains(&scope)) {
            return Err(DiagnosticsError::ScopeDenied);
        }
        if request.auth.expires_at_ms <= request.auth.issued_at_ms
            || request.auth.expires_at_ms - request.auth.issued_at_ms > MAX_DIAGNOSTICS_REQUEST_TTL_MS
            || request.auth.issued_at_ms <= now_ms.saturating_sub(MAX_DIAGNOSTICS_REQUEST_TTL_MS)
            || request.auth.issued_at_ms > now_ms.saturating_add(MAX_DIAGNOSTICS_REQUEST_TTL_MS)
            || now_ms >= request.auth.expires_at_ms
            || request.auth.expires_at_ms > grant.expires_at_ms
        {
            return Err(DiagnosticsError::Unauthorized);
        }
        let device = store.device(&request.auth.debugger_device_id)?.ok_or(DiagnosticsError::Unauthorized)?;
        if device.origin != request.auth.origin || device.status != DeviceStatus::Paired {
            return Err(if device.status == DeviceStatus::Revoked { DiagnosticsError::SessionRevoked } else { DiagnosticsError::Unauthorized });
        }
        request.auth.verify(&target_device_id, &request.method, &request.params, &device.public_key)?;

        let mut state = self.state.lock().map_err(|_| DiagnosticsError::Internal)?;
        if state.stopped {
            return Err(DiagnosticsError::Stopped);
        }
        state.prune(now_ms);
        if state.grants.get(&grant.grant_id) != Some(&grant) {
            return Err(DiagnosticsError::SessionRevoked);
        }
        let session_id = if request.method == "initialize" {
            if request.auth.session_id.is_some() || state.sessions.len() >= MAX_DIAGNOSTICS_SESSIONS {
                return Err(if state.sessions.len() >= MAX_DIAGNOSTICS_SESSIONS { DiagnosticsError::Busy } else { DiagnosticsError::InvalidRequest });
            }
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|_| DiagnosticsError::Internal)?;
            let session_id = format!("dsess-{}", hex(&random));
            state.sessions.insert(
                session_id.clone(),
                SessionState {
                    grant_id: grant.grant_id.clone(),
                    debugger_device_id: grant.debugger_device_id.clone(),
                    origin: grant.debugger_origin.clone(),
                    expires_at_ms: grant.expires_at_ms.min(now_ms.saturating_add(MAX_DIAGNOSTICS_SESSION_TTL_MS)),
                },
            );
            state.audit.push_back(DiagnosticsAuditEvent {
                event: "diagnostics.session.created".into(),
                request_id: Some(request.auth.request_id.clone()),
                origin: Some(request.auth.origin.clone()),
                scope: required_scope,
                result_code: "ok".into(),
                at_ms: now_ms,
            });
            Some(session_id)
        } else {
            let Some(session_id) = request.auth.session_id.as_ref() else {
                return Err(DiagnosticsError::SessionExpired);
            };
            let Some(session) = state.sessions.get(session_id) else {
                return Err(DiagnosticsError::SessionExpired);
            };
            if session.expires_at_ms <= now_ms {
                return Err(DiagnosticsError::SessionExpired);
            }
            if session.grant_id != grant.grant_id || session.debugger_device_id != request.auth.debugger_device_id || session.origin != request.auth.origin {
                return Err(DiagnosticsError::SessionRevoked);
            }
            Some(session_id.clone())
        };
        let replay_key = (request.auth.debugger_device_id.clone(), request.auth.request_id.clone());
        if state.replay.contains_key(&replay_key) {
            return Err(DiagnosticsError::Replay);
        }
        state.replay.insert(replay_key.clone(), request.auth.expires_at_ms);
        state.replay_order.push_back(replay_key);
        while state.replay_order.len() > MAX_DIAGNOSTICS_REPLAY_ENTRIES {
            if let Some(key) = state.replay_order.pop_front() {
                state.replay.remove(&key);
            }
        }
        Ok(session_id)
    }

    fn call_tool(
        &self,
        store: &SyncStore,
        spec: ToolSpec,
        arguments: Value,
        now_ms: i64,
        deadline: Instant,
    ) -> Result<Value, DiagnosticsError> {
        if Instant::now() >= deadline {
            return Err(DiagnosticsError::Timeout);
        }
        if spec.id == ToolId::Other {
            return Err(DiagnosticsError::ToolUnavailable);
        }
        if !arguments.is_object() {
            return Err(DiagnosticsError::InvalidParams);
        }
        match spec.id {
            ToolId::DeviceGetStatus => {
                if !arguments.as_object().is_some_and(Map::is_empty) {
                    return Err(DiagnosticsError::InvalidParams);
                }
                let devices = store.devices()?.into_iter().map(Into::into).collect::<Vec<crate::store::DeviceStatusView>>();
                Ok(serde_json::json!({
                    "primary_id": store.primary_id(),
                    "role": "primary",
                    "service": "running",
                    "devices": devices,
                }))
            }
            ToolId::SyncGetStatus => {
                if !arguments.as_object().is_some_and(Map::is_empty) {
                    return Err(DiagnosticsError::InvalidParams);
                }
                let config = store.primary_config()?;
                let devices = store.devices()?.into_iter().map(Into::into).collect::<Vec<crate::store::DeviceStatusView>>();
                let outbox = store.outbox_summary()?;
                Ok(serde_json::json!({
                    "primary_id": store.primary_id(),
                    "listener": config.as_ref().map(|value| value.enabled).unwrap_or(false),
                    "allow_lan": config.as_ref().map(|value| value.allow_lan).unwrap_or(false),
                    "devices": devices,
                    "outbox": outbox,
                }))
            }
            ToolId::SyncGetRequests => {
                if !arguments.as_object().is_some_and(Map::is_empty) {
                    return Err(DiagnosticsError::InvalidParams);
                }
                Ok(serde_json::json!({ "requests": store.pairing_requests_at(now_ms)? }))
            }
            ToolId::Other => Err(DiagnosticsError::ToolUnavailable),
        }
    }

    fn record_audit(&self, event: DiagnosticsAuditEvent) {
        if let Ok(mut state) = self.state.lock() {
            push_audit(&mut state.audit, event);
        }
    }
}

impl DiagnosticsState {
    fn prune(&mut self, now_ms: i64) {
        self.replay.retain(|_, expiry| *expiry > now_ms);
        self.replay_order.retain(|key| self.replay.contains_key(key));
        self.sessions.retain(|_, session| session.expires_at_ms > now_ms);
    }
}

fn push_audit(audit: &mut VecDeque<DiagnosticsAuditEvent>, event: DiagnosticsAuditEvent) {
    if audit.len() >= MAX_DIAGNOSTICS_AUDIT_EVENTS {
        audit.pop_front();
    }
    audit.push_back(event);
}

fn validate_request(request: &DiagnosticsMcpRequest) -> Result<(), DiagnosticsError> {
    if request.jsonrpc != "2.0" || request.method.is_empty() || request.method.len() > MAX_DIAGNOSTICS_STRING_BYTES {
        return Err(DiagnosticsError::InvalidRequest);
    }
    if request.method != "notifications/initialized" && !valid_jsonrpc_id(request.id.as_ref()) {
        return Err(DiagnosticsError::InvalidRequest);
    }
    validate_json_limits(&request.params, 0)?;
    if canonical_json(&request.params).len() > MAX_DIAGNOSTICS_PARAMS_BYTES {
        return Err(DiagnosticsError::Limit);
    }
    validate_auth_limits(&request.auth)
}

fn validate_auth_limits(auth: &DiagnosticsRequestAuth) -> Result<(), DiagnosticsError> {
    if auth.grant_id.is_empty()
        || auth.grant_id.len() > MAX_DIAGNOSTICS_STRING_BYTES
        || auth.debugger_device_id.is_empty()
        || auth.debugger_device_id.len() > MAX_DIAGNOSTICS_STRING_BYTES
        || auth.origin.is_empty()
        || auth.origin.len() > MAX_DIAGNOSTICS_STRING_BYTES
        || auth.request_id.is_empty()
        || auth.request_id.len() > MAX_DIAGNOSTICS_STRING_BYTES
        || auth.session_id.as_ref().is_some_and(|id| id.is_empty() || id.len() > MAX_DIAGNOSTICS_STRING_BYTES)
    {
        return Err(DiagnosticsError::Limit);
    }
    Ok(())
}

fn validate_json_limits(value: &Value, depth: usize) -> Result<(), DiagnosticsError> {
    if depth > MAX_DIAGNOSTICS_JSON_DEPTH {
        return Err(DiagnosticsError::Limit);
    }
    match value {
        Value::String(value) if value.len() > MAX_DIAGNOSTICS_STRING_BYTES => Err(DiagnosticsError::Limit),
        Value::Array(values) => {
            if values.len() > MAX_DIAGNOSTICS_ARRAY_ITEMS {
                return Err(DiagnosticsError::Limit);
            }
            values.iter().try_for_each(|value| validate_json_limits(value, depth + 1))
        }
        Value::Object(values) => {
            if values.len() > MAX_DIAGNOSTICS_OBJECT_ITEMS {
                return Err(DiagnosticsError::Limit);
            }
            values.iter().try_for_each(|(key, value)| {
                if key.len() > MAX_DIAGNOSTICS_STRING_BYTES {
                    return Err(DiagnosticsError::Limit);
                }
                validate_json_limits(value, depth + 1)
            })
        }
        _ => Ok(()),
    }
}

fn valid_jsonrpc_id(id: Option<&Value>) -> bool {
    matches!(id, Some(Value::String(_) | Value::Number(_)))
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn success_response(id: Option<Value>, result: Value) -> DiagnosticsMcpResponse {
    DiagnosticsMcpResponse { jsonrpc: "2.0".into(), id, result: Some(result), error: None }
}

fn tool_call_spec(params: &Value) -> Result<Option<(ToolSpec, Value)>, DiagnosticsError> {
    let Some(params) = params.as_object() else {
        return Err(DiagnosticsError::InvalidParams);
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err(DiagnosticsError::InvalidParams);
    };
    let spec = TOOL_SPECS.iter().copied().find(|spec| spec.name == name);
    let Some(spec) = spec else {
        return Ok(None);
    };
    let arguments = params.get("arguments").cloned().unwrap_or_else(empty_object);
    validate_json_limits(&arguments, 0)?;
    Ok(Some((spec, arguments)))
}

fn tool_schema(name: &str) -> Value {
    let mut schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {},
    });
    if matches!(name, "logs.tail" | "logs.search") {
        schema["properties"] = serde_json::json!({
            "log": { "type": "string", "enum": ["panel", "sync"] },
            "lines": { "type": "integer", "minimum": 1, "maximum": 200 },
            "since_ms": { "type": "integer" },
            "query": { "type": "string", "maxLength": MAX_DIAGNOSTICS_STRING_BYTES },
            "limit": { "type": "integer", "minimum": 1, "maximum": 200 },
        });
    }
    if name == "diagnostics.run_check" {
        schema["properties"] = serde_json::json!({
            "checks": { "type": "array", "maxItems": 16, "items": { "type": "string", "enum": ["listener", "tls", "pin", "database", "ipc"] } },
        });
        schema["required"] = serde_json::json!(["checks"]);
    }
    if name == "sync.get_trace" {
        schema["properties"] = serde_json::json!({ "limit": { "type": "integer", "minimum": 1, "maximum": 200 } });
    }
    schema
}

fn canonical_json(value: &Value) -> Vec<u8> {
    let mut output = Vec::new();
    write_canonical_json(value, &mut output);
    output
}

fn write_canonical_json(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => output.extend_from_slice(serde_json::to_string(value).expect("JSON strings are serializable").as_bytes()),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output);
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(serde_json::to_string(key).expect("JSON keys are serializable").as_bytes());
                output.push(b':');
                write_canonical_json(&values[key], output);
            }
            output.push(b'}');
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PairingRequest;
    use crate::store::SyncStore;
    use serde_json::json;

    fn clock() -> i64 {
        1_000_000
    }

    fn paired_store() -> (SyncStore, DeviceIdentity) {
        let mut store = SyncStore::open_in_memory("primary-diagnostics").unwrap();
        let identity = DeviceIdentity::create("debugger", "Debugger").unwrap();
        let invite = store.create_pairing_invite("https://localhost:9443", "11".repeat(32), i64::MAX).unwrap();
        let request = PairingRequest::new(&invite, &identity, "linux").unwrap();
        let state = store.receive_pairing_request(&request, clock()).unwrap();
        store.approve_pairing(&state.request_id, clock() + 1).unwrap();
        (store, identity)
    }

    fn request(
        grant: &DiagnosticsGrant,
        identity: &DeviceIdentity,
        session_id: Option<String>,
        method: &str,
        params: Value,
        request_id: &str,
    ) -> DiagnosticsMcpRequest {
        DiagnosticsMcpRequest {
            jsonrpc: "2.0".into(),
            id: if method == "notifications/initialized" { None } else { Some(json!(request_id)) },
            auth: DiagnosticsRequestAuth::new_signed(
                grant,
                identity,
                session_id,
                method,
                &params,
                request_id,
                clock(),
                clock() + 1_000,
            )
            .unwrap(),
            method: method.into(),
            params,
        }
    }

    #[test]
    fn mcp_initialize_list_and_store_backed_call_use_a_fixed_registry() {
        let (store, identity) = paired_store();
        let service = DiagnosticsService::new(store.primary_id()).unwrap();
        let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
        let init = service.handle_request(&store, &request(&grant, &identity, None, "initialize", json!({}), "r-init"), clock(), Instant::now() + Duration::from_secs(1)).unwrap().unwrap();
        let session = init.result.as_ref().unwrap()["sessionId"].as_str().unwrap().to_string();
        let listed = service.handle_request(&store, &request(&grant, &identity, Some(session.clone()), "tools/list", json!({}), "r-list"), clock(), Instant::now() + Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(listed.result.as_ref().unwrap()["tools"].as_array().unwrap().len(), TOOL_SPECS.len());
        let status = service.handle_request(&store, &request(&grant, &identity, Some(session), "tools/call", json!({"name":"device.get_status"}), "r-call"), clock(), Instant::now() + Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(status.result.as_ref().unwrap()["structuredContent"]["primary_id"], "primary-diagnostics");
        assert!(diagnostics_tool_registry().iter().all(|tool| !tool.name.contains("shell") && !tool.name.contains("screenshot")));
    }

    #[test]
    fn replay_expiry_revoke_and_scope_boundaries_fail_closed() {
        let (store, identity) = paired_store();
        let service = DiagnosticsService::new(store.primary_id()).unwrap();
        let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
        let init = request(&grant, &identity, None, "initialize", json!({}), "same");
        let first = service.handle_request(&store, &init, clock(), Instant::now() + Duration::from_secs(1)).unwrap();
        assert!(first.is_some());
        assert_eq!(service.handle_request(&store, &init, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Replay);
        let session = first.unwrap().result.unwrap()["sessionId"].as_str().unwrap().to_string();
        let control = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::SyncControl], clock(), 10_000).unwrap();
        let control_init = service.handle_request(&store, &request(&control, &identity, None, "initialize", json!({}), "control-init"), clock(), Instant::now() + Duration::from_secs(1)).unwrap().unwrap();
        let control_session = control_init.result.unwrap()["sessionId"].as_str().unwrap().to_string();
        let denied = service.handle_request(&store, &request(&control, &identity, Some(control_session), "tools/call", json!({"name":"device.get_status"}), "control-call"), clock(), Instant::now() + Duration::from_secs(1)).unwrap_err();
        assert_eq!(denied, DiagnosticsError::ScopeDenied);
        service.revoke_grant(&grant.grant_id).unwrap();
        let revoked = service.handle_request(&store, &request(&grant, &identity, Some(session), "tools/list", json!({}), "revoked"), clock(), Instant::now() + Duration::from_secs(1)).unwrap_err();
        assert_eq!(revoked, DiagnosticsError::Unauthorized);
    }

    #[test]
    fn request_limits_and_timeout_are_typed_boundaries() {
        let (store, identity) = paired_store();
        let service = DiagnosticsService::new(store.primary_id()).unwrap();
        let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
        let deep = (0..=MAX_DIAGNOSTICS_JSON_DEPTH + 1).fold(json!({}), |value, _| json!({"nested": value}));
        let deep_request = request(&grant, &identity, None, "initialize", deep, "deep");
        assert_eq!(service.handle_json(&store, &serde_json::to_vec(&deep_request).unwrap(), clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Limit);
        let expired_deadline = Instant::now() - Duration::from_millis(1);
        let timeout_request = request(&grant, &identity, None, "initialize", json!({}), "timeout");
        assert_eq!(service.handle_request(&store, &timeout_request, clock(), expired_deadline).unwrap_err(), DiagnosticsError::Timeout);
    }
}

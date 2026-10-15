//! Acceptance probes for the bounded diagnostics MCP protocol.
//!
//! These tests use the public protocol surface and deliberately keep the
//! remaining host-integration gaps ignored instead of treating placeholders as
//! successful remote diagnostics.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use usage_device_sync::{
    DeviceIdentity, DiagnosticsError, DiagnosticsMcpRequest, DiagnosticsRequestAuth,
    DiagnosticsScope, DiagnosticsService, PairingRequest, SyncStore, MAX_DIAGNOSTICS_BODY_BYTES,
};

fn clock() -> i64 { 1_000_000 }

fn paired_store() -> (SyncStore, DeviceIdentity) {
    let mut store = SyncStore::open_in_memory("primary-diagnostics-acceptance").unwrap();
    let identity = DeviceIdentity::create("debugger-acceptance", "Debugger acceptance").unwrap();
    let invite = store.create_pairing_invite("https://localhost:9443", "11".repeat(32), i64::MAX).unwrap();
    let request = PairingRequest::new(&invite, &identity, "linux").unwrap();
    let state = store.receive_pairing_request(&request, clock()).unwrap();
    store.approve_pairing(&state.request_id, clock() + 1).unwrap();
    (store, identity)
}

fn request(
    grant: &usage_device_sync::DiagnosticsGrant,
    identity: &DeviceIdentity,
    session_id: Option<String>,
    method: &str,
    params: Value,
    request_id: &str,
    issued_at_ms: i64,
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
            issued_at_ms,
            issued_at_ms + 1_000,
        ).unwrap(),
        method: method.into(),
        params,
    }
}

fn initialize(
    service: &DiagnosticsService,
    store: &SyncStore,
    grant: &usage_device_sync::DiagnosticsGrant,
    identity: &DeviceIdentity,
    request_id: &str,
) -> String {
    service
        .handle_request(store, &request(grant, identity, None, "initialize", json!({}), request_id, clock()), clock(), Instant::now() + Duration::from_secs(1))
        .unwrap()
        .unwrap()
        .result
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn default_off_and_unauthorized_access_is_rejected() {
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    let auth = DiagnosticsRequestAuth {
        grant_id: "missing-grant".into(),
        session_id: None,
        debugger_device_id: identity.device_id.clone(),
        origin: identity.origin.clone(),
        request_id: "unauthorized-1".into(),
        nonce: [0; 16],
        issued_at_ms: clock(),
        expires_at_ms: clock() + 1_000,
        signature: [0; 64],
    };
    let request = DiagnosticsMcpRequest { jsonrpc: "2.0".into(), id: Some(json!("unauthorized-1")), method: "initialize".into(), params: json!({}), auth };
    assert_eq!(service.handle_request(&store, &request, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Unauthorized);
}

#[test]
fn paired_device_without_diagnostics_scope_is_rejected() {
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
    let session = initialize(&service, &store, &grant, &identity, "scope-init");
    let request = request(&grant, &identity, Some(session), "tools/call", json!({"name":"sync.start_primary","arguments":{}}), "scope-call", clock());
    assert_eq!(service.handle_request(&store, &request, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::ScopeDenied);
}

#[test]
fn expired_and_revoked_grants_are_rejected_immediately() {
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10).unwrap();
    let expired = request(&grant, &identity, None, "initialize", json!({}), "expired", clock() + 11);
    assert_eq!(service.handle_request(&store, &expired, clock() + 11, Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::GrantExpired);
    service.revoke_all();
    let revoked = request(&grant, &identity, None, "initialize", json!({}), "revoked", clock());
    assert_eq!(service.handle_request(&store, &revoked, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Unauthorized);
}

#[test]
fn replayed_request_id_is_rejected() {
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
    let request = request(&grant, &identity, None, "initialize", json!({}), "replay", clock());
    service.handle_request(&store, &request, clock(), Instant::now() + Duration::from_secs(1)).unwrap();
    assert_eq!(service.handle_request(&store, &request, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Replay);
}

#[test]
fn unknown_tools_and_insufficient_scopes_are_rejected() {
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
    let session = initialize(&service, &store, &grant, &identity, "tool-init");
    let unknown = request(&grant, &identity, Some(session.clone()), "tools/call", json!({"name":"unknown.tool","arguments":{}}), "unknown", clock());
    assert_eq!(service.handle_request(&store, &unknown, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::UnknownTool);
    let control = request(&grant, &identity, Some(session), "tools/call", json!({"name":"sync.stop_primary","arguments":{}}), "control", clock());
    assert_eq!(service.handle_request(&store, &control, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::ScopeDenied);
}

#[test]
fn output_is_bounded_by_body_and_deadline() {
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
    let request = request(&grant, &identity, None, "initialize", json!({}), "limits", clock());
    let mut body = serde_json::to_vec(&request).unwrap();
    body.resize(MAX_DIAGNOSTICS_BODY_BYTES + 1, b' ');
    assert_eq!(service.handle_json(&store, &body, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Limit);
    assert_eq!(service.handle_request(&store, &request, clock(), Instant::now() - Duration::from_millis(1)).unwrap_err(), DiagnosticsError::Timeout);
}

#[test]
fn stopping_the_primary_invalidates_sessions_and_grants() {
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
    let session = initialize(&service, &store, &grant, &identity, "stop-init");
    service.stop();
    let request = request(&grant, &identity, Some(session), "tools/list", json!({}), "after-stop", clock());
    assert_eq!(service.handle_request(&store, &request, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Stopped);
}

#[test]
fn host_integrations_remain_explicitly_unverified() {
    // The protocol must expose an explicit failure for tools that still need
    // the Tauri host bridge; this prevents a silent successful no-op.
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
    let session = initialize(&service, &store, &grant, &identity, "bridge-init");
    let request = request(&grant, &identity, Some(session), "tools/call", json!({"name":"logs.tail","arguments":{"log":"panel","lines":1}}), "bridge-tool", clock());
    assert_eq!(service.handle_request(&store, &request, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::ToolUnavailable);
}

#[test]
#[ignore = "requires the Tauri stdio bridge and response redaction integration"]
fn logs_are_redacted_at_the_response_boundary() {
    panic!("host log bridge is intentionally not part of the protocol crate");
}

#[test]
#[ignore = "requires the Tauri remote UI semantic bridge"]
fn ui_and_mcp_sync_actions_have_semantic_parity() {
    panic!("host UI bridge is intentionally not part of the protocol crate");
}

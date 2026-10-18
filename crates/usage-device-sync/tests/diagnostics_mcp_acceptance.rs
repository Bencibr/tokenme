//! Acceptance probes for the bounded diagnostics MCP protocol.
//!
//! These tests use the public protocol surface. Host-controlled read-only
//! diagnostics are exercised through `DiagnosticsHost`; only the semantic UI
//! bridge remains outside this crate's acceptance scope.

use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use usage_device_sync::{
    DeviceIdentity, DiagnosticsError, DiagnosticsHost, DiagnosticsMcpRequest, DiagnosticsRequestAuth,
    DiagnosticsScope, DiagnosticsService, PairingRequest, SyncStore, MAX_DIAGNOSTICS_ARRAY_ITEMS,
    MAX_DIAGNOSTICS_BODY_BYTES, MAX_DIAGNOSTICS_CONCURRENT_REQUESTS, MAX_DIAGNOSTICS_GRANTS,
    MAX_DIAGNOSTICS_JSON_DEPTH, MAX_DIAGNOSTICS_OBJECT_ITEMS, MAX_DIAGNOSTICS_PARAMS_BYTES,
    MAX_DIAGNOSTICS_CHECKS, MAX_DIAGNOSTICS_LOG_LINES, MAX_DIAGNOSTICS_LOG_SEARCH_RESULTS,
    MAX_DIAGNOSTICS_TRACE_RESULTS,
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
fn tool_registry_is_fixed_and_contains_no_execution_surface() {
    let expected = [
        "device.get_status", "device.get_build", "logs.tail", "logs.search", "sync.get_status",
        "sync.get_requests", "sync.get_trace", "diagnostics.run_check", "diagnostics.get_operation",
        "app.get_panel_state", "ui.capture_panel", "sync.start_primary", "sync.pair", "sync.stop_primary", "sync.stop_secondary", "sync.discover",
        "sync.approve_pairing", "sync.reject_pairing", "sync.clear_expired_request", "sync.pause",
        "sync.resume", "sync.revoke_device", "sync.refresh", "ui.open_sync_sheet", "ui.focus_device", "ui.click_panel",
    ];
    let actual = usage_device_sync::diagnostics_tool_registry().into_iter().map(|tool| tool.name).collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert!(actual.iter().all(|name| !name.contains("shell") && !name.contains("exec") && !name.contains("file")));
}

#[test]
fn control_tool_schemas_describe_the_arguments_the_host_accepts() {
    let tools = usage_device_sync::diagnostics_tool_registry();
    let schema = |name: &str| tools.iter().find(|tool| tool.name == name).unwrap().input_schema.clone();
    assert_eq!(schema("sync.start_primary")["properties"]["bind"]["type"], "string");
    assert_eq!(schema("sync.start_primary")["properties"]["advertise_host"]["type"], "string");
    assert_eq!(schema("sync.start_primary")["properties"]["allow_lan"]["type"], "boolean");
    assert_eq!(schema("ui.focus_device")["required"], json!(["device_id"]));
    assert_eq!(schema("sync.revoke_device")["required"], json!(["device_id"]));
    assert_eq!(schema("sync.approve_pairing")["required"], json!(["request_id"]));
    assert_eq!(schema("diagnostics.get_operation")["required"], json!(["operation_id"]));
    assert_eq!(schema("logs.tail")["required"], json!(["log"]));
    assert!(schema("logs.tail")["properties"].get("query").is_none());
    assert_eq!(schema("logs.search")["required"], json!(["log", "query"]));
    assert_eq!(schema("logs.search")["properties"]["limit"]["maximum"], json!(MAX_DIAGNOSTICS_LOG_SEARCH_RESULTS));
    assert_eq!(schema("logs.tail")["properties"]["lines"]["maximum"], json!(MAX_DIAGNOSTICS_LOG_LINES));
    assert_eq!(schema("sync.get_trace")["properties"]["limit"]["maximum"], json!(MAX_DIAGNOSTICS_TRACE_RESULTS));
    assert_eq!(schema("diagnostics.run_check")["properties"]["checks"]["maxItems"], json!(MAX_DIAGNOSTICS_CHECKS));
}

#[test]
fn json_shape_limits_fail_closed_before_tool_dispatch() {
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();

    let oversized = request(&grant, &identity, None, "initialize", json!({ "value": "x".repeat(MAX_DIAGNOSTICS_PARAMS_BYTES) }), "oversized", clock());
    assert_eq!(service.handle_json(&store, &serde_json::to_vec(&oversized).unwrap(), clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Limit);

    let mut nested = Value::Null;
    for _ in 0..=MAX_DIAGNOSTICS_JSON_DEPTH { nested = Value::Array(vec![nested]); }
    let too_deep = request(&grant, &identity, None, "initialize", nested, "too-deep", clock());
    assert_eq!(service.handle_json(&store, &serde_json::to_vec(&too_deep).unwrap(), clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Limit);

    let too_many_items = request(&grant, &identity, None, "initialize", Value::Array((0..=MAX_DIAGNOSTICS_ARRAY_ITEMS).map(|value| json!(value)).collect()), "too-many-items", clock());
    assert_eq!(service.handle_json(&store, &serde_json::to_vec(&too_many_items).unwrap(), clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Limit);

    let mut object = serde_json::Map::new();
    for value in 0..=MAX_DIAGNOSTICS_OBJECT_ITEMS { object.insert(format!("key-{value}"), json!(value)); }
    let too_many_keys = request(&grant, &identity, None, "initialize", Value::Object(object), "too-many-keys", clock());
    assert_eq!(service.handle_json(&store, &serde_json::to_vec(&too_many_keys).unwrap(), clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Limit);
}

#[test]
fn concurrent_request_budget_is_bounded_and_recovers() {
    let service = DiagnosticsService::new("primary-diagnostics-acceptance").unwrap();
    let mut permits = Vec::new();
    for _ in 0..MAX_DIAGNOSTICS_CONCURRENT_REQUESTS { permits.push(service.enter().unwrap()); }
    assert!(matches!(service.enter(), Err(DiagnosticsError::Busy)));
    drop(permits.pop());
    assert!(service.enter().is_ok());
}

#[test]
fn grant_budget_is_bounded() {
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    for _ in 0..MAX_DIAGNOSTICS_GRANTS {
        service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
    }
    assert_eq!(service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap_err(), DiagnosticsError::Busy);
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
    assert_eq!(service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap_err(), DiagnosticsError::Stopped);
}

#[test]
fn a_restart_does_not_accept_grants_from_the_previous_authority() {
    let (store, identity) = paired_store();
    let old_service = DiagnosticsService::new(store.primary_id()).unwrap();
    let old_grant = old_service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
    let restarted_service = DiagnosticsService::new(store.primary_id()).unwrap();
    let old_request = request(&old_grant, &identity, None, "initialize", json!({}), "old-authority", clock());
    assert_eq!(restarted_service.handle_request(&store, &old_request, clock(), Instant::now() + Duration::from_secs(1)).unwrap_err(), DiagnosticsError::Unauthorized);
}

#[test]
fn host_integrations_return_explicit_safe_unconfigured_results() {
    // Built-in diagnostics tools remain callable even before the embedding
    // host registers a log path. They must return a bounded, explicit state,
    // never a successful-looking empty result or a path outside the allowlist.
    let (store, identity) = paired_store();
    let service = DiagnosticsService::new(store.primary_id()).unwrap();
    let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
    let session = initialize(&service, &store, &grant, &identity, "bridge-init");
    let request = request(&grant, &identity, Some(session), "tools/call", json!({"name":"logs.tail","arguments":{"log":"panel","lines":1}}), "bridge-tool", clock());
    let response = service.handle_request(&store, &request, clock(), Instant::now() + Duration::from_secs(1)).unwrap().unwrap();
    let result = response.result.unwrap();
    assert_eq!(result["structuredContent"]["available"], false);
    assert_eq!(result["structuredContent"]["reason"], "not_configured");
}

#[test]
fn logs_are_redacted_at_the_response_boundary() {
    let (store, identity) = paired_store();
    let host = DiagnosticsHost::default();
    let mut log = tempfile::NamedTempFile::new().unwrap();
    writeln!(log, "authorization=Bearer acceptance-secret").unwrap();
    writeln!(log, "{{\"access_token\":\"json-acceptance-secret\"}}").unwrap();
    host.set_log_path("panel", log.path()).unwrap();
    let service = DiagnosticsService::new_with_host(store.primary_id(), host).unwrap();
    let grant = service.issue_grant(&store, &identity.device_id, &[DiagnosticsScope::Read], clock(), 10_000).unwrap();
    let session = initialize(&service, &store, &grant, &identity, "redaction-init");
    let request = request(&grant, &identity, Some(session), "tools/call", json!({"name":"logs.tail","arguments":{"log":"panel","lines":10}}), "redaction-tool", clock());
    let response = service.handle_request(&store, &request, clock(), Instant::now() + Duration::from_secs(1)).unwrap().unwrap();
    let result = response.result.unwrap();
    let lines = result["structuredContent"]["lines"].as_array().unwrap();
    let text = lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
    assert!(text.contains("[REDACTED]"));
    assert!(!text.contains("acceptance-secret"));
    assert!(!text.contains("json-acceptance-secret"));
}

#[test]
#[ignore = "requires the Tauri remote UI semantic bridge"]
fn ui_and_mcp_sync_actions_have_semantic_parity() {
    panic!("host UI bridge is intentionally not part of the protocol crate");
}

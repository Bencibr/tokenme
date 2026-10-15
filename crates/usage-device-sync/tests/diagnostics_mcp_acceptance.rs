//! Acceptance contracts for the diagnostics MCP server.
//!
//! The design documents intentionally say that this API is not implemented.
//! These tests are ignored by default and panic if forced with
//! `cargo test --ignored`; replacing the panic with a real assertion is part
//! of implementing the corresponding contract. Keeping them failing when
//! forced prevents a missing API from being reported as a false pass.

fn not_implemented(contract: &str) -> ! {
    panic!(
        "diagnostics MCP acceptance contract is not implemented: {contract}; see docs/internal/DIAGNOSTICS_MCP_TEST_MATRIX_R1.md"
    );
}

#[test]
#[ignore = "requires the not-yet-implemented diagnostics MCP grant/session API"]
fn default_off_and_unauthorized_access_is_rejected() {
    not_implemented("default-off endpoint and unauthorized request denial");
}

#[test]
#[ignore = "requires the not-yet-implemented diagnostics MCP grant/session API"]
fn paired_device_without_diagnostics_scope_is_rejected() {
    not_implemented("sync pairing must not inherit diagnostics scope");
}

#[test]
#[ignore = "requires the not-yet-implemented diagnostics MCP grant/session API"]
fn expired_and_revoked_grants_are_rejected_immediately() {
    not_implemented("grant expiry and explicit revocation");
}

#[test]
#[ignore = "requires the not-yet-implemented diagnostics MCP request verifier"]
fn replayed_request_id_or_nonce_is_rejected() {
    not_implemented("request timestamp, request ID, nonce, and bounded replay cache");
}

#[test]
#[ignore = "requires the not-yet-implemented diagnostics MCP HTTPS route"]
fn wrong_tls_pin_is_rejected_before_diagnostics_http() {
    not_implemented("TLS 1.3 and certificate-pin boundary for /diagnostics/mcp");
}

#[test]
#[ignore = "requires the not-yet-implemented diagnostics MCP tool registry"]
fn unknown_tools_and_insufficient_scopes_are_rejected() {
    not_implemented("fixed tool allowlist and diagnostics.read/sync.control scope checks");
}

#[test]
#[ignore = "requires the not-yet-implemented diagnostics MCP response pipeline"]
fn logs_are_redacted_at_the_response_boundary() {
    not_implemented("credential, identity path, prompt, source, and provider redaction");
}

#[test]
#[ignore = "requires the not-yet-implemented diagnostics MCP response pipeline"]
fn output_is_bounded_by_lines_bytes_deadline_and_concurrency() {
    not_implemented("bounded logs, JSON body, timeout, and concurrent operations");
}

#[test]
#[ignore = "requires the not-yet-implemented diagnostics MCP operation lifecycle"]
fn stopping_the_primary_invalidates_listener_operations_and_sessions() {
    not_implemented("stop-primary cancellation and immediate diagnostics-session invalidation");
}

#[test]
#[ignore = "requires the not-yet-implemented diagnostics MCP UI bridge"]
fn ui_and_mcp_sync_actions_have_semantic_parity() {
    not_implemented("shared Tauri handler semantics and final operation status");
}

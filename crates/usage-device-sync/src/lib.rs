//! Strict device-to-device sync primitives.
//!
//! This crate deliberately does not auto-detect or alter the legacy JSONL/SSH
//! importer. It owns identity, signed envelopes, bounded packages, pairing,
//! streams, receipts, deterministic merge state, and the bounded HTTPS/TLS 1.3
//! transport used by the primary and secondary runtimes.

mod protocol;
mod diagnostics;
mod lifecycle;
mod runtime;
mod store;
mod transport;

pub use diagnostics::{
    diagnostics_tool_registry, DiagnosticsAuditEvent, DiagnosticsAuthority, DiagnosticsCheckResult,
    DiagnosticsError,
    DiagnosticsErrorCode, DiagnosticsGrant, DiagnosticsMcpRequest, DiagnosticsMcpResponse,
    DiagnosticsControlHandler, DiagnosticsHost, DiagnosticsOperation, DiagnosticsRequestAuth, DiagnosticsScope,
    DiagnosticsCheckRunner, DiagnosticsService, DiagnosticsToolDefinition, DiagnosticsTraceEvent,
    DIAGNOSTICS_MCP_PATH, DIAGNOSTICS_PROTOCOL_VERSION, DIAGNOSTICS_REQUEST_TIMEOUT,
    MAX_DIAGNOSTICS_ARRAY_ITEMS, MAX_DIAGNOSTICS_AUDIT_EVENTS, MAX_DIAGNOSTICS_BODY_BYTES,
    MAX_DIAGNOSTICS_CONCURRENT_REQUESTS, MAX_DIAGNOSTICS_GRANTS, MAX_DIAGNOSTICS_JSON_DEPTH,
    MAX_DIAGNOSTICS_OBJECT_ITEMS, MAX_DIAGNOSTICS_PARAMS_BYTES, MAX_DIAGNOSTICS_REPLAY_ENTRIES,
    MAX_DIAGNOSTICS_SESSIONS, MAX_DIAGNOSTICS_STRING_BYTES, MAX_DIAGNOSTICS_REQUEST_TTL_MS,
    MAX_DIAGNOSTICS_GRANT_TTL_MS, MAX_DIAGNOSTICS_SESSION_TTL_MS, MAX_DIAGNOSTICS_TRACE_EVENTS,
    MAX_DIAGNOSTICS_OPERATIONS, MAX_DIAGNOSTICS_LOG_SCAN_BYTES, MAX_DIAGNOSTICS_OUTPUT_BYTES,
    MAX_DIAGNOSTICS_LOG_LINES, MAX_DIAGNOSTICS_LOG_SEARCH_RESULTS, MAX_DIAGNOSTICS_TRACE_RESULTS,
    MAX_DIAGNOSTICS_CHECKS,
    MAX_DIAGNOSTICS_IMAGE_BYTES, MAX_DIAGNOSTICS_RESPONSE_BYTES,
};
pub use protocol::{
    hash_bytes, identity_bytes, DeviceIdentity, NetworkCall, NetworkCounts, NetworkManifest,
    NetworkPackage, NetworkRow, PairingInvite, PairingRequest, SignedUpload, StreamOpenReceipt,
    AuthenticatedStatus, AuthenticatedStatusRequest, PairingInviteDetails, UploadEnvelope, MAX_COMPRESSED_BYTES, MAX_DECOMPRESSED_BYTES, MAX_LINE_BYTES, MAX_MANIFEST_BYTES,
    MAX_ROWS, MAX_STRING_BYTES, PROTOCOL_ID, SYNC_FORMAT,
};
pub use store::{
    DeviceRecord, DeviceRole, DeviceStatus, DeviceStatusView, DeviceSyncSnapshot, ImportReceipt,
    DirtyJournalEntry, DirtyJournalInput, MergeOutcome, OutboxEntry, OutboxState, OutboxSummary,
    DisconnectNotice, PairingRequestState, PrimarySyncConfig, SecondarySyncConfig, SnapshotCursor, SyncError, SyncResult, SyncStore,
};
pub use lifecycle::deliver_disconnect_notice;
pub use runtime::{export_index_snapshot, export_index_snapshots, export_index_snapshots_from_snapshot, LocalSnapshot, RuntimeRunReport, SyncRuntime, SyncRuntimeConfig};
pub use transport::{
    ensure_server_certificate, load_pem_certificate, load_pem_private_key, ClientTransportCancellation, DeviceSyncTransport, HttpsServerConfig,
    HttpsSyncServer, HttpsSyncServerHandle, HttpsTransportConfig, ParsedEndpoint, Tls13HttpsTransport, TransportError,
};

//! Strict device-to-device sync primitives.
//!
//! This crate deliberately does not auto-detect or alter the legacy JSONL/SSH
//! importer. It owns identity, signed envelopes, bounded packages, pairing,
//! streams, receipts, deterministic merge state, and the bounded HTTPS/TLS 1.3
//! transport used by the primary and secondary runtimes.

mod protocol;
mod runtime;
mod store;
mod transport;

pub use protocol::{
    hash_bytes, identity_bytes, DeviceIdentity, NetworkCall, NetworkCounts, NetworkManifest,
    NetworkPackage, NetworkRow, PairingInvite, PairingRequest, SignedUpload, StreamOpenReceipt,
    AuthenticatedStatus, AuthenticatedStatusRequest, PairingInviteDetails, UploadEnvelope, MAX_COMPRESSED_BYTES, MAX_DECOMPRESSED_BYTES, MAX_LINE_BYTES, MAX_MANIFEST_BYTES,
    MAX_ROWS, MAX_STRING_BYTES, PROTOCOL_ID, SYNC_FORMAT,
};
pub use store::{
    DeviceRecord, DeviceRole, DeviceStatus, DeviceStatusView, DeviceSyncSnapshot, ImportReceipt,
    DirtyJournalEntry, DirtyJournalInput, MergeOutcome, OutboxEntry, OutboxState, OutboxSummary,
    PairingRequestState, PrimarySyncConfig, SecondarySyncConfig, SnapshotCursor, SyncError, SyncResult, SyncStore,
};
pub use runtime::{export_index_snapshot, export_index_snapshots, export_index_snapshots_from_snapshot, LocalSnapshot, RuntimeRunReport, SyncRuntime, SyncRuntimeConfig};
pub use transport::{
    ensure_server_certificate, load_pem_certificate, load_pem_private_key, ClientTransportCancellation, DeviceSyncTransport, HttpsServerConfig,
    HttpsSyncServer, HttpsTransportConfig, ParsedEndpoint, Tls13HttpsTransport, TransportError,
};

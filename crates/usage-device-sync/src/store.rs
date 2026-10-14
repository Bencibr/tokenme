use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use usage_core::{origin_of, origin_ok};
use usage_index::{Index, INDEX_SCHEMA_VERSION};

use crate::protocol::{
    hash_bytes, identity_bytes, network_event_key, public_key_from_bytes, valid_text,
    validate_network_reader, AuthenticatedStatus, AuthenticatedStatusRequest, NetworkManifest,
    NetworkPackage, NetworkRow, PairingInvite,
    PairingRequest, SignedUpload, StreamOpenReceipt, UploadEnvelope,
};

const STATE_VERSION: i64 = 4;
const MAX_PENDING_PAIRING_REQUESTS: i64 = 32;
const MAX_OUTBOX_ENTRIES: i64 = 4_096;
const MAX_OUTBOX_BYTES: i64 = 512 * 1024 * 1024;
const OUTBOX_BACKOFF_MAX_MS: i64 = 15 * 60 * 1000;
const RECEIPT_DOMAIN: &[u8] = b"tokenme-device-sync/receipt/v1";
const TOKENS_SCHEMA: &str = "tokens/v1";
const MAX_TOKEN_CONFLICT_PAIRS_PER_KEY: i64 = 1_024;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("invalid sync state: {0}")]
    Invalid(String),
    #[error("sync protocol error: {0}")]
    Protocol(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("sync limit exceeded: {0}")]
    Limit(String),
    #[error("cryptographic operation failed: {0}")]
    Crypto(String),
    #[error("security policy rejected the operation: {0}")]
    Security(String),
    #[error("unsupported sync feature: {0}")]
    Unsupported(String),
    #[error("row has no stable occurrence identity")]
    UnstableOccurrence,
    #[error("I/O at {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("SQLite: {0}")]
    Sqlite(String),
    #[error("device is not paired: {0}")]
    NotPaired(String),
    #[error("device is revoked: {0}")]
    Revoked(String),
    #[error("stream is closed: {0}")]
    StreamClosed(String),
    #[error("stream data epoch is stale")]
    EpochStale,
    #[error("stream generation gap: expected {expected}, received {received}")]
    GenerationGap { expected: u64, received: u64 },
    #[error("conflicting sync state: {0}")]
    Conflict(String),
    #[error("reconciliation is required before retrying this package")]
    ReconcileRequired,
    #[error("sync state not found: {0}")]
    NotFound(String),
}

pub type SyncResult<T> = std::result::Result<T, SyncError>;

impl From<usage_core::Error> for SyncError {
    fn from(error: usage_core::Error) -> Self {
        match error {
            usage_core::Error::Io { path, source } => Self::Io(path, source),
            usage_core::Error::Sqlite(message) => Self::Sqlite(message),
            usage_core::Error::Sync(message) => Self::Protocol(message),
            other => Self::Sqlite(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    Primary,
    Secondary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    Pending,
    Paired,
    Paused,
    Revoked,
    ReconcileRequired,
}

impl DeviceStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Paired => "paired",
            Self::Paused => "paused",
            Self::Revoked => "revoked",
            Self::ReconcileRequired => "reconcile_required",
        }
    }

    fn parse(value: &str) -> SyncResult<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "paired" => Ok(Self::Paired),
            "paused" => Ok(Self::Paused),
            "revoked" => Ok(Self::Revoked),
            "reconcile_required" => Ok(Self::ReconcileRequired),
            other => Err(SyncError::Unsupported(format!("unknown device status {other:?}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub device_id: String,
    pub origin: String,
    pub display_name: String,
    pub os: String,
    pub public_key: [u8; 32],
    pub role: DeviceRole,
    pub status: DeviceStatus,
    pub data_epoch: u64,
    pub active_stream_id: Option<String>,
    pub last_generation: Option<u64>,
}

/// Device registry fields safe for a status/UI reader. The long-term public
/// key is intentionally omitted even though it is not a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatusView {
    pub device_id: String,
    pub origin: String,
    pub display_name: String,
    pub os: String,
    pub role: DeviceRole,
    pub status: DeviceStatus,
    pub data_epoch: u64,
    pub active_stream_id: Option<String>,
    pub last_generation: Option<u64>,
}

impl From<DeviceRecord> for DeviceStatusView {
    fn from(record: DeviceRecord) -> Self {
        Self {
            device_id: record.device_id,
            origin: record.origin,
            display_name: record.display_name,
            os: record.os,
            role: record.role,
            status: record.status,
            data_epoch: record.data_epoch,
            active_stream_id: record.active_stream_id,
            last_generation: record.last_generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRequestState {
    pub request_id: String,
    pub invite_id: String,
    pub device_id: String,
    pub origin: String,
    pub display_name: String,
    pub os: String,
    pub status: String,
}

/// Read-only status exposed to the panel. The public-key bytes stay inside the
/// sync store; UI callers only need registered device state and requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeviceSyncSnapshot {
    pub primary_id: Option<String>,
    pub devices: Vec<DeviceStatusView>,
    pub pairing_requests: Vec<PairingRequestState>,
}

/// Persisted secondary-side connection and scheduler state. The certificate is
/// public trust material; the identity private key remains in `identity_path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondarySyncConfig {
    pub primary_id: String,
    pub device_id: String,
    pub origin: String,
    pub endpoint: String,
    pub server_name: String,
    pub certificate_der: Vec<u8>,
    pub certificate_pin: [u8; 32],
    pub identity_path: PathBuf,
    pub stream_id: Option<String>,
    pub data_epoch: Option<u64>,
    pub expected_generation: Option<u64>,
    pub enabled: bool,
    pub capture_enabled: bool,
    pub last_error: Option<String>,
    pub last_manifest_hash: Option<[u8; 32]>,
    pub last_gzip_hash: Option<[u8; 32]>,
    pub last_connected_at_ms: Option<i64>,
    pub last_ack_generation: Option<u64>,
    pub last_ack_receipt_hash: Option<[u8; 32]>,
}

impl SecondarySyncConfig {
    fn validate(&self, bound_primary: &str) -> SyncResult<()> {
        if self.primary_id != bound_primary {
            return Err(SyncError::Security("secondary config targets another primary".into()));
        }
        for (label, value) in [
            ("primary_id", self.primary_id.as_str()),
            ("device_id", self.device_id.as_str()),
            ("origin", self.origin.as_str()),
            ("endpoint", self.endpoint.as_str()),
            ("server_name", self.server_name.as_str()),
        ] { validate_identifier(label, value)?; }
        if !origin_ok(&self.origin) { return Err(SyncError::Invalid("secondary origin is not safe".into())); }
        if self.certificate_der.is_empty() || hash_bytes(&self.certificate_der) != self.certificate_pin {
            return Err(SyncError::Security("secondary certificate pin does not match the stored certificate".into()));
        }
        if self.identity_path.as_os_str().is_empty() {
            return Err(SyncError::Invalid("secondary identity path is empty".into()));
        }
        if self.stream_id.is_some() != self.data_epoch.is_some() {
            return Err(SyncError::Invalid("secondary stream and data_epoch must be stored together".into()));
        }
        if self.data_epoch.is_some_and(|epoch| epoch == 0) || self.expected_generation.is_some_and(|generation| generation == 0) {
            return Err(SyncError::Invalid("secondary stream counters start at 1".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeOutcome {
    pub inserted: u64,
    pub updated: u64,
    pub deduped: u64,
}

impl Default for MergeOutcome {
    fn default() -> Self {
        Self { inserted: 0, updated: 0, deduped: 0 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReceipt {
    pub primary_id: String,
    pub device_id: String,
    pub origin: String,
    pub stream_id: String,
    pub data_epoch: u64,
    pub generation: u64,
    pub manifest_hash: [u8; 32],
    pub gzip_hash: [u8; 32],
    pub rows: u64,
    pub inserted: u64,
    pub updated: u64,
    pub deduped: u64,
    pub merged_at_ms: i64,
    pub receipt_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    Pending,
    InFlight,
}

impl OutboxState {
    fn as_str(self) -> &'static str { match self { Self::Pending => "pending", Self::InFlight => "in_flight" } }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub id: i64,
    pub device_id: String,
    pub primary_id: String,
    pub origin: String,
    pub stream_id: String,
    pub data_epoch: u64,
    pub generation: u64,
    pub revision: u64,
    pub manifest_bytes: Vec<u8>,
    pub gzip_sha256: [u8; 32],
    #[serde(with = "crate::protocol::signature_serde")]
    pub signature: [u8; 64],
    pub gzip_bytes: Vec<u8>,
    pub state: OutboxState,
    pub attempts: u64,
    pub next_attempt_ms: i64,
    pub last_error: Option<String>,
    pub snapshot_next_identity: Option<String>,
    pub snapshot_dirty_revision: Option<u64>,
    pub snapshot_complete: Option<bool>,
}

impl OutboxEntry {
    pub fn signed_upload(&self) -> SignedUpload {
        SignedUpload {
            envelope: crate::protocol::UploadEnvelope {
                protocol_id: crate::protocol::PROTOCOL_ID.into(),
                primary_id: self.primary_id.clone(),
                device_id: self.device_id.clone(),
                origin: self.origin.clone(),
                stream_id: self.stream_id.clone(),
                data_epoch: self.data_epoch,
                generation: self.generation,
                manifest_bytes: self.manifest_bytes.clone(),
                gzip_sha256: self.gzip_sha256,
            },
            signature: self.signature,
            gzip_bytes: self.gzip_bytes.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxSummary {
    pub pending: u64,
    pub in_flight: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyJournalEntry {
    pub id: i64,
    pub device_id: String,
    pub origin: String,
    pub identity: String,
    pub kind: String,
    pub row_json: Vec<u8>,
    pub outbox_id: Option<i64>,
    pub revision: u64,
    pub dirty_revision: u64,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct DirtyJournalInput {
    pub identity: String,
    pub revision: u64,
    pub kind: String,
    pub row: NetworkRow,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SnapshotCursor {
    pub last_identity: Option<String>,
    pub last_dirty_revision: u64,
    pub complete: bool,
}

impl ImportReceipt {
    fn new(
        primary_id: &str,
        device_id: &str,
        origin: &str,
        stream_id: &str,
        data_epoch: u64,
        generation: u64,
        manifest_hash: [u8; 32],
        gzip_hash: [u8; 32],
        rows: u64,
        outcome: MergeOutcome,
        merged_at_ms: i64,
    ) -> Self {
        let mut receipt = Self {
            primary_id: primary_id.to_string(),
            device_id: device_id.to_string(),
            origin: origin.to_string(),
            stream_id: stream_id.to_string(),
            data_epoch,
            generation,
            manifest_hash,
            gzip_hash,
            rows,
            inserted: outcome.inserted,
            updated: outcome.updated,
            deduped: outcome.deduped,
            merged_at_ms,
            receipt_hash: [0; 32],
        };
        receipt.receipt_hash = receipt.compute_hash();
        receipt
    }

    fn compute_hash(&self) -> [u8; 32] {
        let u64_bytes = |value: u64| value.to_be_bytes();
        let i64_bytes = self.merged_at_ms.to_be_bytes();
        let fields = [
            self.primary_id.as_bytes(),
            self.device_id.as_bytes(),
            self.stream_id.as_bytes(),
            &self.data_epoch.to_be_bytes(),
            &self.generation.to_be_bytes(),
            &self.manifest_hash,
            &self.gzip_hash,
            &u64_bytes(self.rows),
            &u64_bytes(self.inserted),
            &u64_bytes(self.updated),
            &u64_bytes(self.deduped),
            &i64_bytes,
        ];
        hash_bytes(&identity_bytes(RECEIPT_DOMAIN, &fields))
    }
}

pub struct SyncStore {
    index: Index,
    primary_id: String,
}

impl SyncStore {
    pub fn new(mut index: Index, primary_id: impl Into<String>) -> SyncResult<Self> {
        let primary_id = primary_id.into();
        validate_identifier("primary_id", &primary_id)?;
        index.mark_device_sync_state().map_err(SyncError::from)?;
        // The marker is durable before any sync table/receipt mutation.  A
        // crash during the first migration must make the normal Index opener
        // preserve the database rather than quarantine it as disposable cache.
        migrate(&mut index, &primary_id)?;
        Ok(Self { index, primary_id })
    }

    pub fn open(path: impl Into<PathBuf>, primary_id: impl Into<String>) -> SyncResult<Self> {
        Self::new(Index::open_preserving_corruption(path).map_err(SyncError::from)?, primary_id)
    }

    pub fn open_in_memory(primary_id: impl Into<String>) -> SyncResult<Self> {
        Self::new(Index::open_in_memory().map_err(SyncError::from)?, primary_id)
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn index_mut(&mut self) -> &mut Index {
        &mut self.index
    }

    pub fn primary_id(&self) -> &str {
        &self.primary_id
    }

    /// Reads the additive sync tables without migrating or writing the index.
    /// The panel can therefore inspect state while the engine owns the write
    /// connection, and an older process never creates newer sync tables just
    /// by opening the status bridge.
    pub fn read_status(path: impl AsRef<std::path::Path>) -> SyncResult<DeviceSyncSnapshot> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(DeviceSyncSnapshot::default());
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|error| SyncError::Io(path.to_path_buf(), error))?;
        if metadata.file_type().is_symlink() {
            return Err(SyncError::Security(format!("sync index is a symlink: {}", path.display())));
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sql_err)?;
        let has_state: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'device_sync_meta')",
                [],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        if !has_state {
            return Ok(DeviceSyncSnapshot::default());
        }
        let primary_id = conn
            .query_row("SELECT value FROM device_sync_meta WHERE key = 'primary_id'", [], |row| row.get(0))
            .optional()
            .map_err(sql_err)?;
        let mut ids = conn
            .prepare("SELECT device_id FROM device_registry ORDER BY origin, device_id")
            .map_err(sql_err)?;
        let devices = ids
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql_err)?
            .map(|id| {
                let id = id.map_err(sql_err)?;
                device_tx(&conn, &id)?
                    .ok_or_else(|| SyncError::NotFound("device disappeared".into()))
                    .map(DeviceStatusView::from)
            })
            .collect::<SyncResult<Vec<_>>>()?;
        let mut requests = conn
            .prepare("SELECT request_id, invite_id, device_id, origin, display_name, os,
                             CASE WHEN status = 'pending' AND EXISTS(
                                 SELECT 1 FROM device_pairing_invite i
                                 WHERE i.invite_id = device_pairing_request.invite_id
                                   AND i.expires_at_ms <= CAST(strftime('%s','now') AS INTEGER) * 1000
                             ) THEN 'expired' ELSE status END
                       FROM device_pairing_request ORDER BY created_at_ms, request_id")
            .map_err(sql_err)?;
        let pairing_requests = requests
            .query_map([], |row| {
                Ok(PairingRequestState {
                    request_id: row.get(0)?,
                    invite_id: row.get(1)?,
                    device_id: row.get(2)?,
                    origin: row.get(3)?,
                    display_name: row.get(4)?,
                    os: row.get(5)?,
                    status: row.get(6)?,
                })
            })
            .map_err(sql_err)?
            .map(|row| row.map_err(sql_err))
            .collect::<SyncResult<Vec<_>>>()?;
        Ok(DeviceSyncSnapshot { primary_id, devices, pairing_requests })
    }

    /// Read the persisted secondary configuration without migrating or
    /// acquiring a writer connection. This is safe for the panel's polling
    /// path and lets an older process remain read-only when this table exists.
    pub fn read_secondary_config(path: impl AsRef<std::path::Path>) -> SyncResult<Option<SecondarySyncConfig>> {
        let path = path.as_ref();
        if !path.exists() { return Ok(None); }
        let metadata = std::fs::symlink_metadata(path).map_err(|error| SyncError::Io(path.to_path_buf(), error))?;
        if metadata.file_type().is_symlink() {
            return Err(SyncError::Security(format!("sync index is a symlink: {}", path.display())));
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sql_err)?;
        read_secondary_config_conn(&conn)
    }

    pub fn devices(&self) -> SyncResult<Vec<DeviceRecord>> {
        let mut stmt = self.index.conn().prepare(
            "SELECT device_id FROM device_registry ORDER BY origin, device_id",
        ).map_err(sql_err)?;
        let ids = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(sql_err)?;
        let mut records = Vec::new();
        for id in ids {
            records.push(self.device(&id.map_err(sql_err)?)?.ok_or_else(|| SyncError::NotFound("device disappeared".into()))?);
        }
        Ok(records)
    }

    pub fn device(&self, device_id: &str) -> SyncResult<Option<DeviceRecord>> {
        device_tx(self.index.conn(), device_id)
    }

    pub fn secondary_config(&self) -> SyncResult<Option<SecondarySyncConfig>> {
        read_secondary_config_conn(self.index.conn())
    }

    pub fn secondary_has_history(&self, config: &SecondarySyncConfig) -> SyncResult<bool> {
        if config.last_ack_generation.is_some()
            || config.last_ack_receipt_hash.is_some()
            || config.last_manifest_hash.is_some()
            || config.last_gzip_hash.is_some()
        {
            return Ok(true);
        }
        let has_history: i64 = self.index.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM device_sync_outbox WHERE device_id = ?1)
                 OR EXISTS(SELECT 1 FROM device_sync_journal WHERE device_id = ?1)
                 OR EXISTS(SELECT 1 FROM device_sync_outbox_sequence WHERE device_id = ?1 AND (last_generation > 0 OR last_revision > 0))",
            params![config.device_id],
            |row| row.get(0),
        ).map_err(sql_err)?;
        Ok(has_history != 0)
    }

    pub fn snapshot_cursor(&self, config: &SecondarySyncConfig) -> SyncResult<SnapshotCursor> {
        let Some(stream_id) = config.stream_id.as_deref() else { return Ok(SnapshotCursor::default()); };
        let Some(data_epoch) = config.data_epoch else { return Ok(SnapshotCursor::default()); };
        let row: Option<(Option<String>, i64, i64)> = self.index.conn().query_row(
            "SELECT last_identity, last_dirty_revision, complete
               FROM device_sync_snapshot_cursor
              WHERE device_id = ?1 AND stream_id = ?2 AND data_epoch = ?3",
            params![config.device_id, stream_id, data_epoch as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(sql_err)?;
        let Some((last_identity, dirty_revision, complete)) = row else { return Ok(SnapshotCursor::default()); };
        if dirty_revision < 0 || complete < 0 || complete > 1 {
            return Err(SyncError::Security("snapshot cursor contains invalid state".into()));
        }
        Ok(SnapshotCursor { last_identity, last_dirty_revision: dirty_revision as u64, complete: complete != 0 })
    }

    pub fn save_secondary_config(&mut self, config: &SecondarySyncConfig, now_ms: i64) -> SyncResult<()> {
        config.validate(&self.primary_id)?;
        let config = config.clone();
        self.index.with_network_transaction(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO device_sync_secondary_config(
                    config_key, primary_id, device_id, origin, endpoint, server_name,
                    certificate_der, certificate_pin, identity_path, stream_id, data_epoch,
                    expected_generation, enabled, capture_enabled, last_error, last_manifest_hash, last_gzip_hash,
                    last_connected_at_ms, last_ack_generation, last_ack_receipt_hash, updated_at_ms
                 ) VALUES('secondary', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                params![
                    config.primary_id, config.device_id, config.origin, config.endpoint,
                    config.server_name, config.certificate_der, config.certificate_pin.as_slice(),
                    config.identity_path.to_string_lossy().as_ref(), config.stream_id,
                    config.data_epoch.map(|value| value as i64), config.expected_generation.map(|value| value as i64),
                    if config.enabled { 1_i64 } else { 0_i64 }, if config.capture_enabled { 1_i64 } else { 0_i64 }, config.last_error,
                    config.last_manifest_hash.map(|value| value.to_vec()), config.last_gzip_hash.map(|value| value.to_vec()),
                    config.last_connected_at_ms, config.last_ack_generation.map(|value| value as i64),
                    config.last_ack_receipt_hash.map(|value| value.to_vec()), now_ms,
                ],
            ).map_err(sql_err)?;
            tx.execute(
                "INSERT OR IGNORE INTO meta(key, value) VALUES('device_sync_initial_coverage_lo', ?1)",
                params![now_ms.saturating_sub(30_i64.saturating_mul(86_400_000)) .to_string()],
            ).map_err(sql_err)?;
            Ok::<_, SyncError>(())
        })
    }

    pub fn set_secondary_enabled(&mut self, enabled: bool, now_ms: i64) -> SyncResult<()> {
        self.index.with_network_transaction(|tx| {
            let changed = tx.execute(
                "UPDATE device_sync_secondary_config SET enabled = ?1, updated_at_ms = ?2 WHERE config_key = 'secondary' AND primary_id = ?3",
                params![if enabled { 1_i64 } else { 0_i64 }, now_ms, self.primary_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("secondary sync configuration".into())); }
            Ok(())
        })
    }

    /// Remove the active secondary binding while leaving local usage history and
    /// immutable outbox rows intact for audit/reconciliation.
    pub fn clear_secondary_config(&mut self, now_ms: i64) -> SyncResult<Option<PathBuf>> {
        self.index.with_network_transaction(|tx| {
            let identity_path: Option<String> = tx.query_row(
                "SELECT identity_path FROM device_sync_secondary_config WHERE config_key = 'secondary' AND primary_id = ?1",
                params![self.primary_id], |row| row.get(0),
            ).optional().map_err(sql_err)?;
            tx.execute(
                "DELETE FROM device_sync_secondary_config WHERE config_key = 'secondary' AND primary_id = ?1",
                params![self.primary_id],
            ).map_err(sql_err)?;
            tx.execute(
                "INSERT INTO meta(key, value) VALUES('device_sync_last_unpaired_at_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![now_ms.to_string()],
            ).map_err(sql_err)?;
            Ok(identity_path.map(PathBuf::from))
        })
    }

    /// Network scheduling and local dirty capture are separate switches. A
    /// stopped or primary-paused secondary keeps recording revisions so resume
    /// can upload the changes made while it was offline.
    pub fn set_secondary_capture_enabled(&mut self, enabled: bool, now_ms: i64) -> SyncResult<()> {
        self.index.with_network_transaction(|tx| {
            let changed = tx.execute(
                "UPDATE device_sync_secondary_config SET capture_enabled = ?1, updated_at_ms = ?2 WHERE config_key = 'secondary' AND primary_id = ?3",
                params![if enabled { 1_i64 } else { 0_i64 }, now_ms, self.primary_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("secondary sync configuration".into())); }
            Ok(())
        })
    }

    pub fn set_secondary_last_error(&mut self, error: Option<&str>, now_ms: i64) -> SyncResult<()> {
        let error = error.map(|value| value.chars().take(512).collect::<String>());
        self.index.with_network_transaction(|tx| {
            tx.execute(
                "UPDATE device_sync_secondary_config SET last_error = ?1, updated_at_ms = ?2 WHERE config_key = 'secondary' AND primary_id = ?3",
                params![error, now_ms, self.primary_id],
            ).map_err(sql_err)?;
            Ok(())
        })
    }

    pub fn mark_secondary_connected(&mut self, now_ms: i64) -> SyncResult<()> {
        self.index.with_network_transaction(|tx| {
            tx.execute(
                "UPDATE device_sync_secondary_config
                    SET last_connected_at_ms = ?1, updated_at_ms = ?1
                  WHERE config_key = 'secondary' AND primary_id = ?2",
                params![now_ms, self.primary_id],
            ).map_err(sql_err)?;
            Ok(())
        })
    }

    pub fn update_secondary_stream(&mut self, receipt: &StreamOpenReceipt, now_ms: i64) -> SyncResult<()> {
        if receipt.primary_id != self.primary_id { return Err(SyncError::Auth("stream receipt targets another primary".into())); }
        if receipt.data_epoch == 0 || receipt.expected_generation == 0 { return Err(SyncError::Invalid("stream receipt counters start at 1".into())); }
        self.index.with_network_transaction(|tx| {
            let current: Option<(Option<String>, Option<i64>)> = tx.query_row(
                "SELECT stream_id, data_epoch FROM device_sync_secondary_config WHERE config_key = 'secondary' AND primary_id = ?1 AND device_id = ?2",
                params![self.primary_id, receipt.device_id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(sql_err)?;
            let Some((stream_id, epoch)) = current else { return Err(SyncError::NotFound("secondary sync configuration".into())); };
            let changed = stream_id.as_deref() != Some(receipt.stream_id.as_str()) || epoch != Some(receipt.data_epoch as i64);
            tx.execute(
                "UPDATE device_sync_secondary_config
                 SET stream_id = ?1, data_epoch = ?2, expected_generation = ?3,
                     last_error = NULL,
                     last_manifest_hash = CASE WHEN ?4 THEN NULL ELSE last_manifest_hash END,
                     last_gzip_hash = CASE WHEN ?4 THEN NULL ELSE last_gzip_hash END,
                     updated_at_ms = ?5
                 WHERE config_key = 'secondary' AND primary_id = ?6 AND device_id = ?7",
                params![receipt.stream_id, receipt.data_epoch as i64, receipt.expected_generation as i64, changed, now_ms, self.primary_id, receipt.device_id],
            ).map_err(sql_err)?;
            Ok(())
        })
    }

    pub fn create_pairing_invite(
        &mut self,
        endpoint: impl Into<String>,
        certificate_pin: impl Into<String>,
        expires_at_ms: i64,
    ) -> SyncResult<PairingInvite> {
        let invite = PairingInvite::new(endpoint, certificate_pin, expires_at_ms)?;
        let id = invite.invite_id.clone();
        let endpoint = invite.endpoint.clone();
        let pin = invite.certificate_pin.clone();
        let secret_hash = invite.secret_hash();
        self.index.with_network_transaction(|tx| {
            tx.execute(
                "INSERT INTO device_pairing_invite(invite_id, endpoint, certificate_pin, secret_hash, expires_at_ms, consumed) VALUES(?1, ?2, ?3, ?4, ?5, 0)",
                params![id, endpoint, pin, secret_hash.as_slice(), expires_at_ms],
            ).map_err(sql_err)?;
            Ok::<_, SyncError>(())
        })?;
        Ok(invite)
    }

    pub fn receive_pairing_request(
        &mut self,
        request: &PairingRequest,
        now_ms: i64,
    ) -> SyncResult<PairingRequestState> {
        request.verify()?;
        let expected_id = device_id_from_public_key(&request.public_key);
        if request.device_id != expected_id {
            return Err(SyncError::Auth("pairing device_id does not match public key".into()));
        }
        validate_identifier("request_id", &request.request_id)?;
        validate_identifier("invite_id", &request.invite_id)?;
        validate_identifier("device_id", &request.device_id)?;
        validate_identifier("origin", &request.origin)?;
        if !origin_ok(&request.origin) {
            return Err(SyncError::Invalid("pairing origin is not safe".into()));
        }
        valid_text("display_name", &request.display_name, false)?;
        valid_text("os", &request.os, false)?;
        let request = request.clone();
        self.index.with_network_transaction(|tx| {
            // Expired requests no longer consume the bounded pending queue.
            tx.execute(
                "UPDATE device_pairing_request SET status = 'expired'
                 WHERE status = 'pending' AND invite_id IN
                   (SELECT invite_id FROM device_pairing_invite WHERE expires_at_ms <= ?1)",
                params![now_ms],
            ).map_err(sql_err)?;
            let existing: Option<(String, String, String, String, String, String, String, Vec<u8>, Vec<u8>)> = tx.query_row(
                "SELECT request_id, invite_id, device_id, origin, display_name, os, status, secret_hash, public_key FROM device_pairing_request WHERE request_id = ?1",
                params![request.request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?)),
            ).optional().map_err(sql_err)?;
            if let Some((request_id, invite_id, device_id, origin, display_name, os, status, secret_hash, public_key)) = existing {
                if invite_id != request.invite_id || device_id != request.device_id || origin != request.origin
                    || display_name != request.display_name || os != request.os
                    || secret_hash.as_slice() != hash_bytes(&request.invite_secret).as_slice()
                    || public_key.as_slice() != request.public_key.as_slice()
                {
                    return Err(SyncError::Conflict("pairing request id was reused with different identity".into()));
                }
                return Ok(PairingRequestState { request_id, invite_id, device_id, origin, display_name, os, status });
            }
            let invite: Option<(Vec<u8>, i64, i64, Option<String>)> = tx.query_row(
                "SELECT secret_hash, expires_at_ms, consumed, bound_request_id FROM device_pairing_invite WHERE invite_id = ?1",
                params![request.invite_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional().map_err(sql_err)?;
            let Some((secret_hash, expires_at_ms, consumed, bound_request_id)) = invite else {
                return Err(SyncError::NotFound("pairing invite".into()));
            };
            if now_ms >= expires_at_ms {
                return Err(SyncError::Conflict("pairing invite expired".into()));
            }
            if consumed != 0 {
                return Err(SyncError::Conflict("pairing invite already consumed".into()));
            }
            if secret_hash.as_slice() != hash_bytes(&request.invite_secret).as_slice() {
                return Err(SyncError::Auth("pairing invite secret mismatch".into()));
            }
            if let Some(bound_request_id) = bound_request_id {
                return Err(SyncError::Conflict(format!("pairing invite is already bound to request {bound_request_id}; a new invite is required")));
            }
            let pending: i64 = tx.query_row(
                "SELECT count(*) FROM device_pairing_request WHERE status = 'pending'",
                [],
                |row| row.get(0),
            ).map_err(sql_err)?;
            if pending >= MAX_PENDING_PAIRING_REQUESTS {
                return Err(SyncError::Limit(format!("pending pairing requests exceed {MAX_PENDING_PAIRING_REQUESTS}")));
            }
            let bound = tx.execute(
                "UPDATE device_pairing_invite SET bound_request_id = ?2
                 WHERE invite_id = ?1 AND bound_request_id IS NULL AND consumed = 0 AND expires_at_ms > ?3",
                params![request.invite_id, request.request_id, now_ms],
            ).map_err(sql_err)?;
            if bound != 1 {
                return Err(SyncError::Conflict("pairing invite was concurrently claimed; a new invite is required".into()));
            }
            tx.execute(
                "INSERT INTO device_pairing_request(request_id, invite_id, secret_hash, device_id, origin, display_name, os, public_key, signature, status, created_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10)",
                params![request.request_id, request.invite_id, hash_bytes(&request.invite_secret).as_slice(), request.device_id, request.origin, request.display_name, request.os, request.public_key.as_slice(), request.signature.as_slice(), now_ms],
            ).map_err(sql_err)?;
            Ok(PairingRequestState { request_id: request.request_id, invite_id: request.invite_id, device_id: request.device_id, origin: request.origin, display_name: request.display_name, os: request.os, status: "pending".into() })
        })
    }

    pub fn pairing_requests(&self) -> SyncResult<Vec<PairingRequestState>> {
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| SyncError::Invalid(format!("system clock is before unix epoch: {e}")))?.as_millis().min(i64::MAX as u128) as i64;
        self.pairing_requests_at(now_ms)
    }

    pub fn pairing_requests_at(&self, now_ms: i64) -> SyncResult<Vec<PairingRequestState>> {
        let mut stmt = self.index.conn().prepare(
            "SELECT request_id, invite_id, device_id, origin, display_name, os,
                    CASE WHEN status = 'pending' AND EXISTS(
                        SELECT 1 FROM device_pairing_invite i
                        WHERE i.invite_id = device_pairing_request.invite_id
                                   AND i.expires_at_ms <= ?1
                    ) THEN 'expired' ELSE status END
              FROM device_pairing_request ORDER BY created_at_ms, request_id",
        ).map_err(sql_err)?;
        let rows = stmt.query_map(params![now_ms], |row| Ok(PairingRequestState { request_id: row.get(0)?, invite_id: row.get(1)?, device_id: row.get(2)?, origin: row.get(3)?, display_name: row.get(4)?, os: row.get(5)?, status: row.get(6)? })).map_err(sql_err)?;
        rows.map(|row| row.map_err(sql_err)).collect()
    }

    pub fn approve_pairing(&mut self, request_id: &str, now_ms: i64) -> SyncResult<DeviceRecord> {
        let request_id = request_id.to_string();
        self.index.with_network_transaction(|tx| {
            let request: Option<(String, String, String, String, String, String, Vec<u8>)> = tx.query_row(
                "SELECT invite_id, device_id, origin, display_name, os, status, public_key FROM device_sync_pairing_request_view WHERE request_id = ?1",
                params![request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
            ).optional().map_err(sql_err)?;
            // The view is created by migration; keeping this lookup separate
            // makes the approval transaction easy to audit.
            let Some((invite_id, device_id, origin, display_name, os, status, public_key)) = request else {
                return Err(SyncError::NotFound("pairing request".into()));
            };
            if status == "approved" {
                return device_tx(tx, &device_id)?.ok_or_else(|| SyncError::NotFound("approved device".into()));
            }
            if status != "pending" {
                return Err(SyncError::Conflict(format!("pairing request is {status}")));
            }
            let invite: Option<(i64, i64, Option<String>)> = tx.query_row(
                "SELECT expires_at_ms, consumed, bound_request_id FROM device_pairing_invite WHERE invite_id = ?1",
                params![invite_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional().map_err(sql_err)?;
            let Some((expires_at_ms, consumed, bound_request_id)) = invite else { return Err(SyncError::NotFound("pairing invite".into())); };
            if now_ms >= expires_at_ms || consumed != 0 || bound_request_id.as_deref() != Some(request_id.as_str()) {
                return Err(SyncError::Conflict("pairing invite is no longer approvable".into()));
            }
            let key: [u8; 32] = public_key.as_slice().try_into().map_err(|_| SyncError::Auth("pairing public key length".into()))?;
            if device_id_from_public_key(&key) != device_id {
                return Err(SyncError::Auth("pairing public key identity mismatch".into()));
            }
            if let Some(existing) = device_tx(tx, &device_id)? {
                if existing.public_key != key || existing.origin != origin {
                    return Err(SyncError::Conflict("device is already registered with different identity".into()));
                }
                tx.execute("UPDATE device_registry SET display_name = ?2, os = ?3, status = 'paired' WHERE device_id = ?1", params![device_id, display_name, os]).map_err(sql_err)?;
            } else {
                let origin_owner: Option<String> = tx.query_row("SELECT device_id FROM device_registry WHERE origin = ?1", params![origin], |row| row.get(0)).optional().map_err(sql_err)?;
                if origin_owner.is_some() {
                    return Err(SyncError::Conflict("origin is already registered to another device".into()));
                }
                tx.execute(
                    "INSERT INTO device_registry(device_id, origin, display_name, os, public_key, status, data_epoch, created_at_ms, revoked_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, 'paired', 1, ?6, NULL)",
                    params![device_id, origin, display_name, os, key.as_slice(), now_ms],
                ).map_err(sql_err)?;
            }
            tx.execute("UPDATE device_pairing_request SET status = 'approved' WHERE request_id = ?1", params![request_id]).map_err(sql_err)?;
            tx.execute("UPDATE device_pairing_invite SET consumed = 1 WHERE invite_id = ?1", params![invite_id]).map_err(sql_err)?;
            device_tx(tx, &device_id)?.ok_or_else(|| SyncError::NotFound("registered device".into()))
        })
    }

    pub fn reject_pairing(&mut self, request_id: &str) -> SyncResult<()> {
        self.index.with_network_transaction(|tx| {
            let changed = tx.execute("UPDATE device_pairing_request SET status = 'rejected' WHERE request_id = ?1 AND status = 'pending'", params![request_id]).map_err(sql_err)?;
            if changed == 0 {
                return Err(SyncError::NotFound("pending pairing request".into()));
            }
            Ok(())
        })
    }

    pub fn pause_device(&mut self, device_id: &str) -> SyncResult<()> {
        self.set_device_status(device_id, DeviceStatus::Paused)
    }

    pub fn resume_device(&mut self, device_id: &str) -> SyncResult<()> {
        self.set_device_status(device_id, DeviceStatus::Paired)
    }

    fn set_device_status(&mut self, device_id: &str, status: DeviceStatus) -> SyncResult<()> {
        self.index.with_network_transaction(|tx| {
            let changed = tx.execute("UPDATE device_registry SET status = ?2 WHERE device_id = ?1 AND status != 'revoked'", params![device_id, status.as_str()]).map_err(sql_err)?;
            if changed == 0 {
                return Err(SyncError::NotFound("device".into()));
            }
            Ok(())
        })
    }

    pub fn open_stream(&mut self, device_id: &str, public_key: &[u8; 32], now_ms: i64) -> SyncResult<StreamOpenReceipt> {
        let primary_id = self.primary_id.clone();
        let stream_id = random_id("str-")?;
        let public_key = *public_key;
        let device_id = device_id.to_string();
        self.index.with_network_transaction(|tx| {
            let row: Option<(Vec<u8>, String, i64)> = tx.query_row("SELECT public_key, status, data_epoch FROM device_registry WHERE device_id = ?1", params![device_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional().map_err(sql_err)?;
            let Some((stored_key, status, epoch)) = row else { return Err(SyncError::NotPaired(device_id.clone())); };
            if status == "revoked" { return Err(SyncError::Revoked(device_id.clone())); }
            if status != "paired" { return Err(SyncError::NotPaired(device_id.clone())); }
            if stored_key.as_slice() != public_key.as_slice() { return Err(SyncError::Auth("stream public key mismatch".into())); }
            tx.execute("UPDATE device_stream SET active = 0 WHERE device_id = ?1 AND active = 1", params![device_id]).map_err(sql_err)?;
            tx.execute("INSERT INTO device_stream(device_id, stream_id, data_epoch, expected_generation, last_generation, active, opened_at_ms) VALUES(?1, ?2, ?3, 1, 0, 1, ?4)", params![device_id, stream_id, epoch, now_ms]).map_err(sql_err)?;
            Ok(StreamOpenReceipt { primary_id, device_id, stream_id, data_epoch: epoch as u64, expected_generation: 1 })
        })
    }

    /// Primary-side convenience for an already paired device. The key is read
    /// from the registry inside the same connection; callers cannot substitute
    /// a different public key when issuing a stream receipt.
    pub fn open_stream_for_registered(&mut self, device_id: &str, now_ms: i64) -> SyncResult<StreamOpenReceipt> {
        let key: Vec<u8> = self.index.conn().query_row(
            "SELECT public_key FROM device_registry WHERE device_id = ?1",
            params![device_id],
            |row| row.get(0),
        ).optional().map_err(sql_err)?.ok_or_else(|| SyncError::NotPaired(device_id.into()))?;
        let key: [u8; 32] = key.as_slice().try_into().map_err(|_| SyncError::Security("registered public key length is invalid".into()))?;
        self.open_stream(device_id, &key, now_ms)
    }

    pub fn revoke_device(&mut self, device_id: &str, now_ms: i64) -> SyncResult<u64> {
        self.index.with_network_transaction(|tx| {
            let epoch: Option<i64> = tx.query_row("SELECT data_epoch FROM device_registry WHERE device_id = ?1", params![device_id], |row| row.get(0)).optional().map_err(sql_err)?;
            let Some(epoch) = epoch else { return Err(SyncError::NotFound("device".into())); };
            let new_epoch = epoch.checked_add(1).ok_or_else(|| SyncError::Invalid("data epoch overflow".into()))?;
            tx.execute("UPDATE device_registry SET status = 'revoked', data_epoch = ?2, revoked_at_ms = ?3 WHERE device_id = ?1", params![device_id, new_epoch, now_ms]).map_err(sql_err)?;
            tx.execute("UPDATE device_stream SET active = 0 WHERE device_id = ?1", params![device_id]).map_err(sql_err)?;
            Ok(new_epoch as u64)
        })
    }

    pub fn receipt(&self, device_id: &str, stream_id: &str, generation: u64) -> SyncResult<Option<ImportReceipt>> {
        read_receipt(self.index.conn(), device_id, stream_id, generation)
    }

    pub fn authenticated_status(&self, request: &AuthenticatedStatusRequest, observed_at_ms: i64) -> SyncResult<AuthenticatedStatus> {
        if request.primary_id != self.primary_id {
            return Err(SyncError::Auth("status targets another primary".into()));
        }
        let (public_key, status, epoch, origin): (Vec<u8>, String, i64, String) = self.index.conn().query_row(
            "SELECT public_key, status, data_epoch, origin FROM device_registry WHERE device_id = ?1",
            params![request.device_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional().map_err(sql_err)?.ok_or_else(|| SyncError::NotPaired(request.device_id.clone()))?;
        if origin != request.origin {
            return Err(SyncError::Auth("status origin does not match paired device".into()));
        }
        let public_key = public_key_from_bytes(&public_key)?;
        request.verify(&public_key)?;
        if !request.is_fresh(observed_at_ms) {
            return Err(SyncError::Auth("status request is expired or from the future".into()));
        }
        let stream: Option<(String, i64, i64)> = self.index.conn().query_row(
            "SELECT stream_id, expected_generation, last_generation FROM device_stream WHERE device_id = ?1 AND active = 1",
            params![request.device_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(sql_err)?;
        let highwater: Option<(i64, [u8; 32])> = if let Some(row) = stream.as_ref() {
            let value: Option<(i64, Vec<u8>)> = self.index.conn().query_row(
                "SELECT last_generation, receipt_hash FROM device_highwater WHERE device_id = ?1 AND stream_id = ?2",
                params![request.device_id, row.0],
                |value| Ok((value.get(0)?, value.get(1)?)),
            ).optional().map_err(sql_err)?;
            match value {
                Some((generation, hash)) => Some((generation, blob32(&hash)?)),
                None => None,
            }
        } else {
            None
        };
        let checkpoint_receipt_hash = if let Some(generation) = request.checkpoint_generation {
            let Some(stream) = stream.as_ref() else { return Err(SyncError::ReconcileRequired); };
            let generation = i64::try_from(generation).map_err(|_| SyncError::Invalid("checkpoint generation is too large".into()))?;
            let receipt = read_receipt(self.index.conn(), &request.device_id, &stream.0, generation as u64)?
                .ok_or(SyncError::ReconcileRequired)?;
            Some(receipt.receipt_hash)
        } else {
            None
        };
        let coverage: Option<(i64, i64)> = self.index.conn().query_row(
            "SELECT MIN(ts_ms), MAX(ts_ms) FROM event WHERE source LIKE ?1",
            params![format!("linux:{}:%", origin)],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
        ).optional().map_err(sql_err)?.and_then(|(lo, hi)| match (lo, hi) {
            (Some(lo), Some(hi)) => Some((lo, hi)),
            _ => None,
        });
        Ok(AuthenticatedStatus {
            primary_id: self.primary_id.clone(),
            device_id: request.device_id.clone(),
            origin,
            request_nonce: request.nonce,
            status,
            data_epoch: epoch as u64,
            active_stream_id: stream.as_ref().map(|row| row.0.clone()),
            expected_generation: stream.as_ref().map(|row| row.1 as u64),
            last_generation: stream.map(|row| row.2 as u64),
            highwater_generation: highwater.as_ref().map(|row| row.0 as u64),
            highwater_receipt_hash: highwater.map(|row| row.1),
            checkpoint_generation: request.checkpoint_generation,
            checkpoint_receipt_hash,
            coverage_lo_ms: coverage.map(|range| range.0),
            coverage_hi_ms: coverage.map(|range| range.1),
            observed_at_ms,
        })
    }

    /// Authorize bounded upload metadata before any compressed bytes are
    /// staged. The full import repeats the checks in its transaction because
    /// revocation and stream replacement may race this gate.
    pub fn authorize_upload_metadata(&self, envelope: &UploadEnvelope, signature: &[u8; 64]) -> SyncResult<()> {
        if envelope.primary_id != self.primary_id {
            return Err(SyncError::Auth("upload targets another primary".into()));
        }
        envelope.validate()?;
        let (public_key, status, epoch, origin): (Vec<u8>, String, i64, String) = self.index.conn().query_row(
            "SELECT public_key, status, data_epoch, origin FROM device_registry WHERE device_id = ?1",
            params![envelope.device_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional().map_err(sql_err)?.ok_or_else(|| SyncError::NotPaired(envelope.device_id.clone()))?;
        if status == "revoked" { return Err(SyncError::Revoked(envelope.device_id.clone())); }
        if status != "paired" { return Err(SyncError::NotPaired(envelope.device_id.clone())); }
        if origin != envelope.origin { return Err(SyncError::Auth("upload origin does not match paired device".into())); }
        if epoch as u64 != envelope.data_epoch { return Err(SyncError::EpochStale); }
        let stream: Option<(i64, i64, i64)> = self.index.conn().query_row(
            "SELECT data_epoch, expected_generation, active FROM device_stream WHERE device_id = ?1 AND stream_id = ?2",
            params![envelope.device_id, envelope.stream_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(sql_err)?;
        let Some((stream_epoch, expected_generation, active)) = stream else { return Err(SyncError::StreamClosed(envelope.stream_id.clone())); };
        if stream_epoch as u64 != envelope.data_epoch { return Err(SyncError::EpochStale); }
        if active == 0 {
            let manifest_hash = hash_bytes(&envelope.manifest_bytes);
            let replay_is_valid: Option<bool> = self.index.conn().query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM device_receipt r
                    WHERE r.device_id = ?1 AND r.stream_id = ?2 AND r.generation = ?3
                      AND r.manifest_hash = ?4 AND r.gzip_hash = ?5
                      AND EXISTS(
                          SELECT 1 FROM device_package_marker m
                          JOIN device_receipt original
                            ON original.device_id = m.device_id
                           AND original.stream_id = m.receipt_stream_id
                           AND original.generation = m.receipt_generation
                           AND original.receipt_hash = m.receipt_hash
                          WHERE m.device_id = r.device_id
                            AND m.manifest_hash = r.manifest_hash
                            AND m.gzip_hash = r.gzip_hash
                      )
                )",
                params![envelope.device_id, envelope.stream_id, envelope.generation as i64, manifest_hash.as_slice(), envelope.gzip_sha256.as_slice()],
                |row| row.get(0),
            ).optional().map_err(sql_err)?;
            if replay_is_valid != Some(true) { return Err(SyncError::StreamClosed(envelope.stream_id.clone())); }
        }
        if envelope.generation > expected_generation as u64 {
            return Err(SyncError::GenerationGap { expected: expected_generation as u64, received: envelope.generation });
        }
        let public_key = public_key_from_bytes(&public_key)?;
        envelope.verify_signature(&public_key, signature)
    }

    pub fn recover_stream(&mut self, request: &AuthenticatedStatusRequest, now_ms: i64) -> SyncResult<StreamOpenReceipt> {
        if !request.want_recovery {
            return Err(SyncError::Invalid("recovery request flag is required".into()));
        }
        if request.primary_id != self.primary_id {
            return Err(SyncError::Auth("recovery targets another primary".into()));
        }
        let key: Vec<u8> = self.index.conn().query_row(
            "SELECT public_key FROM device_registry WHERE device_id = ?1 AND origin = ?2",
            params![request.device_id, request.origin],
            |row| row.get(0),
        ).optional().map_err(sql_err)?.ok_or_else(|| SyncError::NotPaired(request.device_id.clone()))?;
        let key: [u8; 32] = key.as_slice().try_into().map_err(|_| SyncError::Security("registered public key length is invalid".into()))?;
        let public_key = public_key_from_bytes(&key)?;
        request.verify(&public_key)?;
        if !request.is_fresh(now_ms) {
            return Err(SyncError::Auth("recovery request is expired or from the future".into()));
        }
        self.index.with_network_transaction(|tx| {
            tx.execute(
                "DELETE FROM device_sync_recovery WHERE expires_at_ms <= ?1",
                params![now_ms],
            ).map_err(sql_err)?;
            let current: Option<(String, i64)> = tx.query_row(
                "SELECT status, data_epoch FROM device_registry WHERE device_id = ?1 AND origin = ?2",
                params![request.device_id, request.origin],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(sql_err)?;
            let Some((status, current_epoch)) = current else { return Err(SyncError::NotPaired(request.device_id.clone())); };
            if status == "revoked" { return Err(SyncError::Revoked(request.device_id.clone())); }
            if status != "paired" { return Err(SyncError::NotPaired(request.device_id.clone())); }
            let existing: Option<(String, i64, i64)> = tx.query_row(
                "SELECT stream_id, data_epoch, expected_generation FROM device_sync_recovery WHERE device_id = ?1 AND nonce = ?2",
                params![request.device_id, request.nonce.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional().map_err(sql_err)?;
            if let Some((stream_id, epoch, expected_generation)) = existing {
                if epoch as u64 != current_epoch as u64 {
                    return Err(SyncError::EpochStale);
                }
                return Ok(StreamOpenReceipt { primary_id: self.primary_id.clone(), device_id: request.device_id.clone(), stream_id, data_epoch: epoch as u64, expected_generation: expected_generation as u64 });
            }
            let active_challenges: i64 = tx.query_row(
                "SELECT count(*) FROM device_sync_recovery WHERE device_id = ?1 AND expires_at_ms > ?2",
                params![request.device_id, now_ms], |row| row.get(0),
            ).map_err(sql_err)?;
            if active_challenges >= 8 { return Err(SyncError::Limit("too many active recovery challenges; retry after expiry".into())); }
            let row: Option<(Vec<u8>, String, i64)> = tx.query_row(
                "SELECT public_key, status, data_epoch FROM device_registry WHERE device_id = ?1",
                params![request.device_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional().map_err(sql_err)?;
            let Some((stored_key, status, epoch)) = row else { return Err(SyncError::NotPaired(request.device_id.clone())); };
            if stored_key.as_slice() != key.as_slice() { return Err(SyncError::Auth("recovery public key mismatch".into())); }
            if status == "revoked" { return Err(SyncError::Revoked(request.device_id.clone())); }
            if status != "paired" { return Err(SyncError::NotPaired(request.device_id.clone())); }
            let stream_id = random_id("str-")?;
            tx.execute("UPDATE device_stream SET active = 0 WHERE device_id = ?1 AND active = 1", params![request.device_id]).map_err(sql_err)?;
            tx.execute("INSERT INTO device_stream(device_id, stream_id, data_epoch, expected_generation, last_generation, active, opened_at_ms) VALUES(?1, ?2, ?3, 1, 0, 1, ?4)", params![request.device_id, stream_id, epoch, now_ms]).map_err(sql_err)?;
            tx.execute("INSERT INTO device_sync_recovery(device_id, nonce, stream_id, data_epoch, expected_generation, issued_at_ms, expires_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![request.device_id, request.nonce.as_slice(), stream_id, epoch, 1, now_ms, now_ms.saturating_add(10 * 60 * 1000)]).map_err(sql_err)?;
            Ok(StreamOpenReceipt { primary_id: self.primary_id.clone(), device_id: request.device_id.clone(), stream_id, data_epoch: epoch as u64, expected_generation: 1 })
        })
    }

    pub fn enqueue_outbox(&mut self, upload: &SignedUpload, now_ms: i64) -> SyncResult<i64> {
        self.enqueue_outbox_with_journal(upload, &[], now_ms)
    }

    pub fn enqueue_outbox_with_journal(&mut self, upload: &SignedUpload, journal: &[DirtyJournalInput], now_ms: i64) -> SyncResult<i64> {
        self.index.with_network_transaction(|tx| {
            enqueue_outbox_tx(tx, &self.primary_id, upload, journal, now_ms)
        })
    }

    /// Build and persist a package with its generation and payload revision
    /// allocated by the same SQLite transaction that writes the outbox and
    /// dirty journal. Callers provide bytes and a signing key, never a stream
    /// generation.
    pub fn enqueue_outbox_auto(
        &mut self,
        device_id: &str,
        origin: &str,
        stream_id: &str,
        data_epoch: u64,
        manifest_bytes: Vec<u8>,
        gzip_bytes: Vec<u8>,
        signing_key: &SigningKey,
        journal: &[DirtyJournalInput],
        now_ms: i64,
    ) -> SyncResult<(i64, SignedUpload)> {
        let device_id = device_id.to_string();
        let origin = origin.to_string();
        let stream_id = stream_id.to_string();
        let primary_id = self.primary_id.clone();
        self.index.with_network_transaction(|tx| {
            let gzip_hash = hash_bytes(&gzip_bytes);
            if let Some(id) = existing_outbox_id(tx, &device_id, &stream_id, data_epoch, &manifest_bytes, &gzip_hash)? {
                let entry = read_outbox(tx, id)?.ok_or_else(|| SyncError::NotFound(format!("outbox {id}")))?;
                return Ok((id, entry.signed_upload()));
            }
            let (last_generation, _) = outbox_sequence(tx, &device_id, &stream_id, data_epoch)?;
            let generation = last_generation.checked_add(1).ok_or_else(|| SyncError::Invalid("outbox generation overflow".into()))?;
            let envelope = crate::protocol::UploadEnvelope {
                protocol_id: crate::protocol::PROTOCOL_ID.into(),
                primary_id: primary_id.clone(),
                device_id: device_id.clone(),
                origin: origin.clone(),
                stream_id: stream_id.clone(),
                data_epoch,
                generation,
                manifest_bytes,
                gzip_sha256: gzip_hash,
            };
            let upload = SignedUpload::sign(envelope, signing_key, gzip_bytes)?;
            let id = enqueue_outbox_tx(tx, &primary_id, &upload, journal, now_ms)?;
            Ok((id, upload))
        })
    }

    /// Enqueue a complete local snapshot only when its immutable bytes differ
    /// from the last successfully persisted snapshot for this stream. The
    /// snapshot fingerprint, generation, payload revision, journal rows and
    /// outbox row commit together, so a crash cannot advance one without the
    /// others.
    pub fn enqueue_snapshot_auto(
        &mut self,
        config: &SecondarySyncConfig,
        package: NetworkPackage,
        journal: &[DirtyJournalInput],
        signing_key: &SigningKey,
        now_ms: i64,
    ) -> SyncResult<Option<(i64, SignedUpload)>> {
        self.enqueue_snapshot_auto_with_cursor(config, package, journal, signing_key, None, now_ms)
    }

    pub fn enqueue_snapshot_page_auto(
        &mut self,
        config: &SecondarySyncConfig,
        package: NetworkPackage,
        journal: &[DirtyJournalInput],
        signing_key: &SigningKey,
        next_identity: Option<&str>,
        next_dirty_revision: u64,
        complete: bool,
        now_ms: i64,
    ) -> SyncResult<Option<(i64, SignedUpload)>> {
        self.enqueue_snapshot_auto_with_cursor(
            config, package, journal, signing_key,
            Some((next_identity, next_dirty_revision, complete)), now_ms,
        )
    }

    fn enqueue_snapshot_auto_with_cursor(
        &mut self,
        config: &SecondarySyncConfig,
        package: NetworkPackage,
        journal: &[DirtyJournalInput],
        signing_key: &SigningKey,
        cursor: Option<(Option<&str>, u64, bool)>,
        now_ms: i64,
    ) -> SyncResult<Option<(i64, SignedUpload)>> {
        config.validate(&self.primary_id)?;
        let stream_id = config.stream_id.clone().ok_or_else(|| SyncError::StreamClosed("secondary has no active stream".into()))?;
        let data_epoch = config.data_epoch.ok_or_else(|| SyncError::EpochStale)?;
        let manifest_hash = hash_bytes(&package.manifest_bytes);
        let gzip_hash = hash_bytes(&package.gzip_bytes);
        let config = config.clone();
        self.index.with_network_transaction(|tx| {
            let previous: Option<(String, i64, Option<Vec<u8>>, Option<Vec<u8>>)> = tx.query_row(
                "SELECT stream_id, data_epoch, last_manifest_hash, last_gzip_hash
                   FROM device_sync_secondary_config
                  WHERE config_key = 'secondary' AND primary_id = ?1 AND device_id = ?2",
                params![config.primary_id, config.device_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional().map_err(sql_err)?;
            if let Some((previous_stream, previous_epoch, Some(previous_manifest), Some(previous_gzip))) = previous {
                if previous_stream == stream_id && previous_epoch as u64 == data_epoch
                    && previous_manifest.as_slice() == manifest_hash.as_slice()
                    && previous_gzip.as_slice() == gzip_hash.as_slice()
                {
                    return Ok(None);
                }
            }
            let envelope = crate::protocol::UploadEnvelope {
                protocol_id: crate::protocol::PROTOCOL_ID.into(),
                primary_id: config.primary_id.clone(), device_id: config.device_id.clone(),
                origin: config.origin.clone(), stream_id, data_epoch,
                generation: outbox_sequence(tx, &config.device_id, config.stream_id.as_deref().unwrap(), data_epoch)?.0.checked_add(1)
                    .ok_or_else(|| SyncError::Invalid("outbox generation overflow".into()))?,
                manifest_bytes: package.manifest_bytes,
                gzip_sha256: gzip_hash,
            };
            let upload = SignedUpload::sign(envelope, signing_key, package.gzip_bytes)?;
            let id = enqueue_outbox_tx_with_cursor(tx, &config.primary_id, &upload, journal, cursor, now_ms)?;
            tx.execute(
                "UPDATE device_sync_secondary_config SET last_manifest_hash = ?1, last_gzip_hash = ?2, last_error = NULL, updated_at_ms = ?3 WHERE config_key = 'secondary' AND primary_id = ?4 AND device_id = ?5",
                params![manifest_hash.as_slice(), gzip_hash.as_slice(), now_ms, config.primary_id, config.device_id],
            ).map_err(sql_err)?;
            Ok(Some((id, upload)))
        })
    }

    pub fn claim_outbox(&mut self, owner: &str, now_ms: i64, limit: usize) -> SyncResult<Vec<OutboxEntry>> {
        let limit = i64::try_from(limit).map_err(|_| SyncError::Limit("outbox claim limit is too large".into()))?;
        self.index.with_network_transaction(|tx| {
            ensure_scheduler_owner(tx, owner, now_ms)?;
            let mut ids = Vec::new();
            let mut stmt = tx.prepare(
                "SELECT o.id FROM device_sync_outbox o
                 WHERE o.state = 'pending' AND o.next_attempt_ms <= ?1
                   AND o.id = (SELECT MIN(first.id) FROM device_sync_outbox first
                               WHERE first.device_id = o.device_id AND first.stream_id = o.stream_id)
                 ORDER BY o.id LIMIT ?2",
            ).map_err(sql_err)?;
            let rows = stmt.query_map(params![now_ms, limit], |row| row.get::<_, i64>(0)).map_err(sql_err)?;
            for id in rows { ids.push(id.map_err(sql_err)?); }
            for id in &ids {
                tx.execute("UPDATE device_sync_outbox SET state = 'in_flight', attempts = attempts + 1 WHERE id = ?1 AND state = 'pending'", params![id]).map_err(sql_err)?;
            }
            ids.into_iter().map(|id| read_outbox(tx, id)?.ok_or_else(|| SyncError::NotFound(format!("outbox {id}")))).collect()
        })
    }

    pub fn retry_outbox(&mut self, owner: &str, id: i64, now_ms: i64, error: &str) -> SyncResult<()> {
        self.index.with_network_transaction(|tx| {
            ensure_scheduler_owner(tx, owner, now_ms)?;
            let attempts: Option<i64> = tx.query_row("SELECT attempts FROM device_sync_outbox WHERE id = ?1", params![id], |row| row.get(0)).optional().map_err(sql_err)?;
            let Some(attempts) = attempts else { return Err(SyncError::NotFound(format!("outbox {id}"))); };
            let shift = attempts.min(20) as u32;
            let delay = (1000_i64.saturating_mul(1_i64 << shift)).min(OUTBOX_BACKOFF_MAX_MS);
            tx.execute("UPDATE device_sync_outbox SET state = 'pending', next_attempt_ms = ?2, last_error = ?3 WHERE id = ?1", params![id, now_ms.saturating_add(delay), error.chars().take(512).collect::<String>()]).map_err(sql_err)?;
            Ok(())
        })
    }

    pub fn ack_outbox(&mut self, owner: &str, id: i64, receipt: &ImportReceipt, now_ms: i64) -> SyncResult<()> {
        self.index.with_network_transaction(|tx| {
            ensure_scheduler_owner(tx, owner, now_ms)?;
            let Some(entry) = read_outbox(tx, id)? else { return Ok(()); };
            let manifest_hash = hash_bytes(&entry.manifest_bytes);
            if receipt.compute_hash() != receipt.receipt_hash
                || entry.primary_id != receipt.primary_id
                || entry.device_id != receipt.device_id
                || entry.origin != receipt.origin
                || entry.stream_id != receipt.stream_id
                || entry.data_epoch != receipt.data_epoch
                || entry.generation != receipt.generation
                || manifest_hash != receipt.manifest_hash
                || entry.gzip_sha256 != receipt.gzip_hash
            {
                return Err(SyncError::Conflict("ACK does not match immutable outbox package".into()));
            }
            let secondary_state: Option<(Option<String>, Option<i64>, Option<i64>, Option<Vec<u8>>)> = tx.query_row(
                "SELECT stream_id, data_epoch, last_ack_generation, last_ack_receipt_hash
                   FROM device_sync_secondary_config
                  WHERE config_key = 'secondary' AND primary_id = ?1 AND device_id = ?2",
                params![entry.primary_id, entry.device_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional().map_err(sql_err)?;
            if let Some((stored_stream, stored_epoch, stored_generation, stored_hash)) = secondary_state {
                if stored_stream.as_deref() != Some(entry.stream_id.as_str()) || stored_epoch != Some(entry.data_epoch as i64) {
                    return Err(SyncError::ReconcileRequired);
                }
                if let Some(previous_generation) = stored_generation {
                    let previous_generation = u64::try_from(previous_generation).map_err(|_| SyncError::Security("stored secondary ACK generation is negative".into()))?;
                    if receipt.generation < previous_generation {
                        return Err(SyncError::Conflict("ACK generation moves backwards".into()));
                    }
                    if receipt.generation == previous_generation {
                        let previous_hash = stored_hash.as_deref().map(blob32).transpose()?.ok_or_else(|| SyncError::Security("stored secondary ACK hash is missing".into()))?;
                        if previous_hash != receipt.receipt_hash {
                            return Err(SyncError::ReconcileRequired);
                        }
                    }
                }
                let changed = tx.execute(
                    "UPDATE device_sync_secondary_config
                        SET last_ack_generation = ?1, last_ack_receipt_hash = ?2, updated_at_ms = ?3
                      WHERE config_key = 'secondary' AND primary_id = ?4 AND device_id = ?5",
                    params![receipt.generation as i64, receipt.receipt_hash.as_slice(), now_ms, entry.primary_id, entry.device_id],
                ).map_err(sql_err)?;
                if changed != 1 { return Err(SyncError::NotFound("secondary sync configuration".into())); }
            }
            if entry.snapshot_next_identity.is_some() || entry.snapshot_dirty_revision.is_some() || entry.snapshot_complete.is_some() {
                let dirty_revision = entry.snapshot_dirty_revision.ok_or_else(|| SyncError::Security("snapshot cursor target is incomplete".into()))?;
                let complete = entry.snapshot_complete.ok_or_else(|| SyncError::Security("snapshot cursor completion is missing".into()))?;
                let current: Option<i64> = tx.query_row(
                    "SELECT last_dirty_revision FROM device_sync_snapshot_cursor
                      WHERE device_id = ?1 AND stream_id = ?2 AND data_epoch = ?3",
                    params![entry.device_id, entry.stream_id, entry.data_epoch as i64],
                    |row| row.get(0),
                ).optional().map_err(sql_err)?;
                if current.is_some_and(|value| value < 0 || value as u64 > dirty_revision) {
                    return Err(SyncError::ReconcileRequired);
                }
                tx.execute(
                    "INSERT INTO device_sync_snapshot_cursor(device_id, stream_id, data_epoch, last_identity, last_dirty_revision, complete, updated_at_ms)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(device_id, stream_id, data_epoch) DO UPDATE SET
                         last_identity = excluded.last_identity,
                         last_dirty_revision = excluded.last_dirty_revision,
                         complete = excluded.complete,
                         updated_at_ms = excluded.updated_at_ms",
                    params![entry.device_id, entry.stream_id, entry.data_epoch as i64, entry.snapshot_next_identity, dirty_revision as i64, if complete { 1_i64 } else { 0_i64 }, now_ms],
                ).map_err(sql_err)?;
            }
            let legacy_json_bytes: i64 = tx.query_row(
                "SELECT COALESCE(sum(length(d.event_json)), 0) FROM sync_dirty d
                   WHERE EXISTS(
                     SELECT 1 FROM device_sync_journal j
                      WHERE j.outbox_id = ?1 AND j.identity = d.identity AND j.dirty_revision >= d.revision
                   )",
                params![id], |row| row.get(0),
            ).map_err(sql_err)?;
            tx.execute(
                "DELETE FROM sync_dirty
                  WHERE EXISTS(
                      SELECT 1 FROM device_sync_journal j
                       WHERE j.outbox_id = ?1
                         AND j.identity = sync_dirty.identity
                         AND j.dirty_revision >= sync_dirty.revision
                  )",
                params![id],
            ).map_err(sql_err)?;
            if legacy_json_bytes > 0 {
                adjust_dirty_json_bytes_tx(tx, -legacy_json_bytes)?;
            }
            tx.execute("DELETE FROM device_sync_journal WHERE outbox_id = ?1", params![id]).map_err(sql_err)?;
            tx.execute("DELETE FROM device_sync_outbox WHERE id = ?1", params![id]).map_err(sql_err)?;
            Ok(())
        })
    }

    pub fn recover_inflight_outbox(&mut self, owner: &str, now_ms: i64) -> SyncResult<u64> {
        self.index.with_network_transaction(|tx| {
            ensure_scheduler_owner(tx, owner, now_ms)?;
            let changed = tx.execute("UPDATE device_sync_outbox SET state = 'pending', next_attempt_ms = ?1, last_error = 'recovered after restart' WHERE state = 'in_flight'", params![now_ms]).map_err(sql_err)?;
            Ok(changed as u64)
        })
    }

    pub fn outbox_summary(&self) -> SyncResult<OutboxSummary> {
        self.index.conn().query_row(
            "SELECT COALESCE(sum(state = 'pending'), 0), COALESCE(sum(state = 'in_flight'), 0),
                    COALESCE((SELECT sum(length(manifest_bytes) + length(gzip_bytes) + length(signature)) FROM device_sync_outbox), 0)
                  + COALESCE((SELECT sum(length(row_json)) FROM device_sync_journal), 0)
                  + COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'device_sync_dirty_json_bytes'), 0)
               FROM device_sync_outbox",
            [],
            |row| Ok(OutboxSummary { pending: row.get::<_, i64>(0)? as u64, in_flight: row.get::<_, i64>(1)? as u64, bytes: row.get::<_, i64>(2)? as u64 }),
        ).map_err(sql_err)
    }

    pub fn acquire_scheduler_lease(&mut self, owner: &str, now_ms: i64, ttl_ms: i64) -> SyncResult<bool> {
        if owner.is_empty() || ttl_ms <= 0 { return Err(SyncError::Invalid("scheduler lease arguments are invalid".into())); }
        self.index.with_network_transaction(|tx| {
            let existing: Option<(String, i64)> = tx.query_row("SELECT owner, expires_at_ms FROM device_sync_scheduler_lease WHERE lease_key = 'outbox'", [], |row| Ok((row.get(0)?, row.get(1)?))).optional().map_err(sql_err)?;
            if let Some((_current, expires)) = existing {
                if expires > now_ms { return Ok(false); }
            }
            tx.execute("INSERT OR REPLACE INTO device_sync_scheduler_lease(lease_key, owner, expires_at_ms) VALUES('outbox', ?1, ?2)", params![owner, now_ms.saturating_add(ttl_ms)]).map_err(sql_err)?;
            Ok(true)
        })
    }

    pub fn renew_scheduler_lease(&mut self, owner: &str, now_ms: i64, ttl_ms: i64) -> SyncResult<bool> {
        if owner.is_empty() || ttl_ms <= 0 { return Err(SyncError::Invalid("scheduler lease arguments are invalid".into())); }
        self.index.with_network_transaction(|tx| {
            let changed = tx.execute(
                "UPDATE device_sync_scheduler_lease SET expires_at_ms = ?3 WHERE lease_key = 'outbox' AND owner = ?1 AND expires_at_ms > ?2",
                params![owner, now_ms, now_ms.saturating_add(ttl_ms)],
            ).map_err(sql_err)?;
            Ok(changed == 1)
        })
    }

    pub fn release_scheduler_lease(&mut self, owner: &str) -> SyncResult<bool> {
        self.index.with_network_transaction(|tx| {
            let changed = tx.execute("DELETE FROM device_sync_scheduler_lease WHERE lease_key = 'outbox' AND owner = ?1", params![owner]).map_err(sql_err)?;
            Ok(changed == 1)
        })
    }

    pub fn import_network_strict(&mut self, upload: &SignedUpload, merged_at_ms: i64) -> SyncResult<ImportReceipt> {
        upload.envelope.validate()?;
        let public_key_bytes: Vec<u8> = self.index.conn().query_row(
            "SELECT public_key FROM device_registry WHERE device_id = ?1",
            params![upload.envelope.device_id],
            |row| row.get(0),
        ).optional().map_err(sql_err)?.ok_or_else(|| SyncError::NotPaired(upload.envelope.device_id.clone()))?;
        let public_key = public_key_from_bytes(&public_key_bytes)?;
        upload.verify(&public_key)?;
        self.import_network_reader_strict(
            &upload.envelope,
            &upload.signature,
            Cursor::new(upload.gzip_bytes.as_slice()),
            upload.gzip_bytes.len(),
            merged_at_ms,
        )
    }

    /// Strict import for a bounded reader, used by the HTTPS staging file.
    /// The compressed bytes are hashed and decompressed once, row by row, in
    /// the same SQLite transaction that mutates events, rollups, receipts and
    /// highwater. No whole gzip or decompressed JSONL clone is made here.
    pub(crate) fn import_network_reader_strict<R: Read>(
        &mut self,
        envelope: &UploadEnvelope,
        signature: &[u8; 64],
        reader: R,
        compressed_len: usize,
        merged_at_ms: i64,
    ) -> SyncResult<ImportReceipt> {
        envelope.validate()?;
        if envelope.primary_id != self.primary_id {
            return Err(SyncError::Auth("upload targets another primary".into()));
        }
        if envelope.primary_id == envelope.device_id {
            return Err(SyncError::Protocol("primary cannot import itself as a secondary".into()));
        }
        let public_key_bytes: Vec<u8> = self.index.conn().query_row(
            "SELECT public_key FROM device_registry WHERE device_id = ?1",
            params![envelope.device_id],
            |row| row.get(0),
        ).optional().map_err(sql_err)?.ok_or_else(|| SyncError::NotPaired(envelope.device_id.clone()))?;
        let public_key = public_key_from_bytes(&public_key_bytes)?;
        envelope.verify_signature(&public_key, signature)?;
        let manifest: NetworkManifest = serde_json::from_slice(&envelope.manifest_bytes)
            .map_err(|e| SyncError::Protocol(format!("invalid manifest: {e}")))?;
        if manifest.origin != envelope.origin || manifest.schema_version != INDEX_SCHEMA_VERSION {
            return Err(SyncError::Unsupported("package origin or index schema is not supported".into()));
        }
        let manifest_hash = hash_bytes(&envelope.manifest_bytes);
        let gzip_hash = envelope.gzip_sha256;
        let primary_id = self.primary_id.clone();
        let device_id = envelope.device_id.clone();
        let stream_id = envelope.stream_id.clone();
        let epoch = envelope.data_epoch;
        let generation = envelope.generation;
        let result = self.index.with_network_transaction(|tx| {
            import_tx_reader(
                tx, &primary_id, &device_id, &stream_id, epoch, generation, &manifest.origin,
                manifest_hash, gzip_hash, &envelope.manifest_bytes, reader, compressed_len, merged_at_ms,
            )
        });
        if matches!(&result, Err(SyncError::ReconcileRequired)) {
            self.index.with_network_transaction(|tx| {
                tx.execute(
                    "UPDATE device_registry SET status = 'reconcile_required' WHERE device_id = ?1 AND status != 'revoked'",
                    params![device_id],
                ).map_err(sql_err)?;
                Ok::<_, SyncError>(())
            })?;
        }
        result
    }

    pub fn purge_receipt(&mut self, device_id: &str, stream_id: &str, generation: u64) -> SyncResult<()> {
        self.index.with_network_transaction(|tx| {
            let receipt = read_receipt(tx, device_id, stream_id, generation)?.ok_or_else(|| SyncError::NotFound("receipt".into()))?;
            tx.execute("INSERT OR REPLACE INTO device_package_tombstone(device_id, manifest_hash, gzip_hash, deleted_at_ms) VALUES(?1, ?2, ?3, strftime('%s','now') * 1000)", params![device_id, receipt.manifest_hash.as_slice(), receipt.gzip_hash.as_slice()]).map_err(sql_err)?;
            tx.execute("DELETE FROM device_package_marker WHERE device_id = ?1 AND manifest_hash = ?2 AND gzip_hash = ?3", params![device_id, receipt.manifest_hash.as_slice(), receipt.gzip_hash.as_slice()]).map_err(sql_err)?;
            tx.execute("DELETE FROM device_receipt WHERE device_id = ?1 AND stream_id = ?2 AND generation = ?3", params![device_id, stream_id, generation as i64]).map_err(sql_err)?;
            Ok(())
        })
    }
}

fn ensure_column(tx: &Transaction<'_>, table: &str, column: &str, definition: &str) -> SyncResult<()> {
    // These names/definitions are internal migration constants, not caller SQL.
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        params![table, column],
        |row| row.get(0),
    ).map_err(sql_err)?;
    if !exists {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
        tx.execute_batch(&sql).map_err(sql_err)?;
    }
    Ok(())
}

fn existing_outbox_id(
    tx: &Transaction<'_>, device_id: &str, stream_id: &str, data_epoch: u64,
    manifest_bytes: &[u8], gzip_hash: &[u8; 32],
) -> SyncResult<Option<i64>> {
    tx.query_row(
        "SELECT id FROM device_sync_outbox
         WHERE device_id = ?1 AND stream_id = ?2 AND data_epoch = ?3
           AND manifest_bytes = ?4 AND gzip_sha256 = ?5
         ORDER BY id LIMIT 1",
        params![device_id, stream_id, data_epoch as i64, manifest_bytes, gzip_hash.as_slice()],
        |row| row.get(0),
    ).optional().map_err(sql_err)
}

fn outbox_sequence(tx: &Transaction<'_>, device_id: &str, stream_id: &str, data_epoch: u64) -> SyncResult<(u64, u64)> {
    let row: Option<(i64, i64)> = tx.query_row(
        "SELECT last_generation, last_revision FROM device_sync_outbox_sequence
         WHERE device_id = ?1 AND stream_id = ?2 AND data_epoch = ?3",
        params![device_id, stream_id, data_epoch as i64],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(sql_err)?;
    if let Some((generation, revision)) = row {
        if generation < 0 || revision < 0 {
            return Err(SyncError::Security("outbox sequence contains a negative counter".into()));
        }
        return Ok((generation as u64, revision as u64));
    }
    let row: (i64, i64) = tx.query_row(
        "SELECT COALESCE(MAX(generation), 0), COALESCE(MAX(payload_revision), 0)
         FROM device_sync_outbox WHERE device_id = ?1 AND stream_id = ?2 AND data_epoch = ?3",
        params![device_id, stream_id, data_epoch as i64],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(sql_err)?;
    if row.0 < 0 || row.1 < 0 {
        return Err(SyncError::Security("outbox sequence contains a negative counter".into()));
    }
    Ok((row.0 as u64, row.1 as u64))
}

fn enqueue_outbox_tx(
    tx: &Transaction<'_>, primary_id: &str, upload: &SignedUpload,
    journal: &[DirtyJournalInput], now_ms: i64,
) -> SyncResult<i64> {
    enqueue_outbox_tx_with_cursor(tx, primary_id, upload, journal, None, now_ms)
}

fn enqueue_outbox_tx_with_cursor(
    tx: &Transaction<'_>, primary_id: &str, upload: &SignedUpload,
    journal: &[DirtyJournalInput], cursor: Option<(Option<&str>, u64, bool)>, now_ms: i64,
) -> SyncResult<i64> {
    let envelope = &upload.envelope;
    if envelope.primary_id != primary_id && !primary_id.is_empty() {
        return Err(SyncError::Auth("outbox package targets another primary".into()));
    }
    if hash_bytes(&upload.gzip_bytes) != envelope.gzip_sha256 {
        return Err(SyncError::Auth("outbox gzip hash mismatch".into()));
    }
    if let Some(id) = existing_outbox_id(tx, &envelope.device_id, &envelope.stream_id, envelope.data_epoch, &envelope.manifest_bytes, &envelope.gzip_sha256)? {
        return Ok(id);
    }
    let (last_generation, last_revision) = outbox_sequence(tx, &envelope.device_id, &envelope.stream_id, envelope.data_epoch)?;
    let expected = last_generation.checked_add(1).ok_or_else(|| SyncError::Invalid("outbox generation overflow".into()))?;
    if envelope.generation != expected {
        return Err(SyncError::GenerationGap { expected, received: envelope.generation });
    }
    let revision = last_revision.checked_add(1).ok_or_else(|| SyncError::Invalid("outbox payload revision overflow".into()))?;
    let mut encoded_journal = Vec::with_capacity(journal.len());
    for entry in journal {
        if entry.identity.is_empty() || entry.revision == 0 {
            return Err(SyncError::Invalid("dirty journal identity and revision are required".into()));
        }
        entry.row.identity_material()?;
        let dirty_revision = i64::try_from(entry.revision).map_err(|_| SyncError::Limit("dirty journal revision is too large".into()))?;
        let row_json = serde_json::to_vec(&entry.row).map_err(|e| SyncError::Protocol(format!("encode dirty journal row: {e}")))?;
        if row_json.len() > usage_index::SYNC_SNAPSHOT_MAX_ROW_BYTES {
            return Err(SyncError::Limit("dirty journal row is too large".into()));
        }
        encoded_journal.push((entry.identity.clone(), entry.kind.clone(), dirty_revision, row_json));
    }
    let count: i64 = tx.query_row("SELECT count(*) FROM device_sync_outbox", [], |row| row.get(0)).map_err(sql_err)?;
    let before_bytes: i64 = tx.query_row(
        "SELECT COALESCE((SELECT sum(length(manifest_bytes) + length(gzip_bytes) + length(signature)) FROM device_sync_outbox), 0)
              + COALESCE((SELECT sum(length(row_json)) FROM device_sync_journal), 0)
              + COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'device_sync_dirty_json_bytes'), 0)",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    if count >= MAX_OUTBOX_ENTRIES {
        return Err(SyncError::Limit("sync outbox quota exceeded; older packages remain pending".into()));
    }
    tx.execute(
        "INSERT INTO device_sync_outbox(device_id, primary_id, origin, stream_id, data_epoch, generation, payload_revision,
                 snapshot_next_identity, snapshot_dirty_revision, snapshot_complete,
                 manifest_bytes, gzip_sha256, signature, gzip_bytes, state, attempts, next_attempt_ms, created_at_ms, last_error)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 'pending', 0, ?15, ?15, NULL)",
        params![
            envelope.device_id, envelope.primary_id, envelope.origin, envelope.stream_id,
            envelope.data_epoch as i64, envelope.generation as i64, revision as i64,
            cursor.as_ref().and_then(|value| value.0),
            cursor.as_ref().map(|value| value.1 as i64),
            cursor.as_ref().map(|value| if value.2 { 1_i64 } else { 0_i64 }),
            envelope.manifest_bytes, envelope.gzip_sha256.as_slice(), upload.signature.as_slice(), upload.gzip_bytes,
            now_ms,
        ],
    ).map_err(sql_err)?;
    let outbox_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT OR REPLACE INTO device_sync_outbox_sequence(device_id, stream_id, data_epoch, last_generation, last_revision) VALUES(?1, ?2, ?3, ?4, ?5)",
        params![envelope.device_id, envelope.stream_id, envelope.data_epoch as i64, envelope.generation as i64, revision as i64],
    ).map_err(sql_err)?;
    for (identity, kind, dirty_revision, row_json) in encoded_journal {
        tx.execute(
            "INSERT INTO device_sync_journal(device_id, origin, identity, kind, row_json, outbox_id, payload_revision, dirty_revision, created_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![envelope.device_id, envelope.origin, identity, kind, row_json, outbox_id, revision as i64, dirty_revision, now_ms],
        ).map_err(sql_err)?;
        // The immutable journal row is now the durable copy for this exact
        // revision. A newer dirty revision is intentionally retained.
        let legacy_json_bytes: i64 = tx.query_row(
            "SELECT COALESCE(sum(length(event_json)), 0) FROM sync_dirty WHERE identity = ?1 AND revision <= ?2",
            params![identity, dirty_revision], |row| row.get(0),
        ).map_err(sql_err)?;
        tx.execute(
            "DELETE FROM sync_dirty WHERE identity = ?1 AND revision <= ?2",
            params![identity, dirty_revision],
        ).map_err(sql_err)?;
        if legacy_json_bytes > 0 {
            adjust_dirty_json_bytes_tx(tx, -legacy_json_bytes)?;
        }
    }
    let after_bytes: i64 = tx.query_row(
        "SELECT COALESCE((SELECT sum(length(manifest_bytes) + length(gzip_bytes) + length(signature)) FROM device_sync_outbox), 0)
              + COALESCE((SELECT sum(length(row_json)) FROM device_sync_journal), 0)
              + COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'device_sync_dirty_json_bytes'), 0)",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    // Check the actual post-mutation SQLite image. If an old backlog is over
    // quota, allow only a transaction that really lowers its physical size so
    // it can drain in bounded steps.
    if after_bytes > MAX_OUTBOX_BYTES && after_bytes >= before_bytes {
        return Err(SyncError::Limit("sync outbox quota exceeded; drain pending data before capturing more".into()));
    }
    Ok(outbox_id)
}

fn ensure_scheduler_owner(tx: &Transaction<'_>, owner: &str, now_ms: i64) -> SyncResult<()> {
    let lease: Option<(String, i64)> = tx.query_row(
        "SELECT owner, expires_at_ms FROM device_sync_scheduler_lease WHERE lease_key = 'outbox'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(sql_err)?;
    if matches!(lease, Some((current, expires)) if current == owner && expires > now_ms) {
        Ok(())
    } else {
        Err(SyncError::Security("scheduler lease is not held by this runtime".into()))
    }
}

fn adjust_dirty_json_bytes_tx(tx: &Transaction<'_>, delta: i64) -> SyncResult<()> {
    let current: i64 = tx.query_row(
        "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'device_sync_dirty_json_bytes'), 0)",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    let next = match current.checked_add(delta).filter(|value| *value >= 0) {
        Some(next) => next,
        None => tx.query_row(
            "SELECT COALESCE(sum(length(event_json)), 0) FROM sync_dirty",
            [], |row| row.get(0),
        ).map_err(sql_err)?,
    };
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('device_sync_dirty_json_bytes', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    ).map_err(sql_err)?;
    Ok(())
}

fn migrate(index: &mut Index, primary_id: &str) -> SyncResult<()> {
    index.with_network_transaction(|tx| {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS device_sync_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS device_registry(
                 device_id TEXT PRIMARY KEY, origin TEXT NOT NULL UNIQUE, display_name TEXT NOT NULL,
                 os TEXT NOT NULL, public_key BLOB NOT NULL, status TEXT NOT NULL,
                 data_epoch INTEGER NOT NULL, created_at_ms INTEGER NOT NULL, revoked_at_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS device_stream(
                 device_id TEXT NOT NULL, stream_id TEXT NOT NULL, data_epoch INTEGER NOT NULL,
                 expected_generation INTEGER NOT NULL, last_generation INTEGER NOT NULL,
                 active INTEGER NOT NULL, opened_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(device_id, stream_id)
             );
             CREATE UNIQUE INDEX IF NOT EXISTS device_stream_one_active ON device_stream(device_id) WHERE active = 1;
             CREATE TABLE IF NOT EXISTS device_receipt(
                 device_id TEXT NOT NULL, stream_id TEXT NOT NULL, generation INTEGER NOT NULL,
                 data_epoch INTEGER NOT NULL, primary_id TEXT NOT NULL, origin TEXT NOT NULL, manifest_hash BLOB NOT NULL,
                 gzip_hash BLOB NOT NULL, rows INTEGER NOT NULL, inserted INTEGER NOT NULL,
                 updated INTEGER NOT NULL, deduped INTEGER NOT NULL, merged_at_ms INTEGER NOT NULL,
                 receipt_hash BLOB NOT NULL, PRIMARY KEY(device_id, stream_id, generation)
             );
             CREATE TABLE IF NOT EXISTS device_package_marker(
                 device_id TEXT NOT NULL, manifest_hash BLOB NOT NULL, gzip_hash BLOB NOT NULL,
                 receipt_stream_id TEXT NOT NULL, receipt_generation INTEGER NOT NULL,
                 receipt_hash BLOB NOT NULL, PRIMARY KEY(device_id, manifest_hash, gzip_hash)
             );
             CREATE TABLE IF NOT EXISTS device_package_tombstone(
                 device_id TEXT NOT NULL, manifest_hash BLOB NOT NULL, gzip_hash BLOB NOT NULL,
                 deleted_at_ms INTEGER NOT NULL, PRIMARY KEY(device_id, manifest_hash, gzip_hash)
             );
             CREATE TABLE IF NOT EXISTS device_highwater(
                 device_id TEXT NOT NULL, stream_id TEXT NOT NULL, data_epoch INTEGER NOT NULL,
                 last_generation INTEGER NOT NULL, receipt_hash BLOB NOT NULL, updated_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(device_id, stream_id)
             );
             CREATE TABLE IF NOT EXISTS device_sync_conflict(
                 device_id TEXT NOT NULL, dedupe_key TEXT NOT NULL, field_schema TEXT NOT NULL,
                 old_hash BLOB NOT NULL, new_hash BLOB NOT NULL, created_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(device_id, dedupe_key, field_schema, old_hash, new_hash)
             );
             CREATE TABLE IF NOT EXISTS device_sync_outbox(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 device_id TEXT NOT NULL, primary_id TEXT NOT NULL, origin TEXT NOT NULL,
                 stream_id TEXT NOT NULL, data_epoch INTEGER NOT NULL, generation INTEGER NOT NULL,
                 payload_revision INTEGER NOT NULL DEFAULT 0,
                 snapshot_next_identity TEXT, snapshot_dirty_revision INTEGER,
                 snapshot_complete INTEGER,
                 manifest_bytes BLOB NOT NULL, gzip_sha256 BLOB NOT NULL, signature BLOB NOT NULL,
                 gzip_bytes BLOB NOT NULL, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
                 next_attempt_ms INTEGER NOT NULL, created_at_ms INTEGER NOT NULL, last_error TEXT,
                 UNIQUE(device_id, stream_id, generation, gzip_sha256)
             );
             CREATE INDEX IF NOT EXISTS device_sync_outbox_fifo ON device_sync_outbox(state, next_attempt_ms, id);
             CREATE TABLE IF NOT EXISTS device_sync_snapshot_cursor(
                 device_id TEXT NOT NULL, stream_id TEXT NOT NULL, data_epoch INTEGER NOT NULL,
                 last_identity TEXT, last_dirty_revision INTEGER NOT NULL DEFAULT 0,
                 complete INTEGER NOT NULL DEFAULT 0, updated_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(device_id, stream_id, data_epoch)
             );
             CREATE TABLE IF NOT EXISTS device_sync_journal(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 device_id TEXT NOT NULL, origin TEXT NOT NULL, identity TEXT NOT NULL,
                 kind TEXT NOT NULL, row_json BLOB NOT NULL, outbox_id INTEGER,
                 payload_revision INTEGER NOT NULL DEFAULT 0,
                 dirty_revision INTEGER NOT NULL DEFAULT 0,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS device_sync_journal_outbox ON device_sync_journal(outbox_id, id);
             CREATE TABLE IF NOT EXISTS device_sync_outbox_sequence(
                 device_id TEXT NOT NULL, stream_id TEXT NOT NULL, data_epoch INTEGER NOT NULL,
                 last_generation INTEGER NOT NULL, last_revision INTEGER NOT NULL,
                 PRIMARY KEY(device_id, stream_id, data_epoch)
             );
             CREATE TABLE IF NOT EXISTS device_sync_scheduler_lease(
                 lease_key TEXT PRIMARY KEY, owner TEXT NOT NULL, expires_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS device_sync_recovery(
                 device_id TEXT NOT NULL, nonce BLOB NOT NULL, stream_id TEXT NOT NULL,
                 data_epoch INTEGER NOT NULL, expected_generation INTEGER NOT NULL,
                 issued_at_ms INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(device_id, nonce)
             );
             CREATE INDEX IF NOT EXISTS device_sync_recovery_expiry
                 ON device_sync_recovery(device_id, expires_at_ms);
             CREATE TABLE IF NOT EXISTS device_sync_secondary_config(
                 config_key TEXT PRIMARY KEY,
                 primary_id TEXT NOT NULL, device_id TEXT NOT NULL, origin TEXT NOT NULL,
                 endpoint TEXT NOT NULL, server_name TEXT NOT NULL,
                 certificate_der BLOB NOT NULL, certificate_pin BLOB NOT NULL,
                 identity_path TEXT NOT NULL, stream_id TEXT, data_epoch INTEGER,
                 expected_generation INTEGER, enabled INTEGER NOT NULL,
                 capture_enabled INTEGER NOT NULL DEFAULT 1,
                 last_error TEXT, last_manifest_hash BLOB, last_gzip_hash BLOB,
                 last_connected_at_ms INTEGER,
                 last_ack_generation INTEGER, last_ack_receipt_hash BLOB,
                 updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS device_pairing_invite(
                 invite_id TEXT PRIMARY KEY, endpoint TEXT NOT NULL, certificate_pin TEXT NOT NULL,
                 secret_hash BLOB NOT NULL, expires_at_ms INTEGER NOT NULL, consumed INTEGER NOT NULL,
                 bound_request_id TEXT
             );
             CREATE TABLE IF NOT EXISTS device_pairing_request(
                 request_id TEXT PRIMARY KEY, invite_id TEXT NOT NULL, secret_hash BLOB NOT NULL,
                 device_id TEXT NOT NULL, origin TEXT NOT NULL, display_name TEXT NOT NULL, os TEXT NOT NULL,
                 public_key BLOB NOT NULL, signature BLOB NOT NULL, status TEXT NOT NULL, created_at_ms INTEGER NOT NULL
             );
             CREATE VIEW IF NOT EXISTS device_sync_pairing_request_view AS
                 SELECT request_id, invite_id, device_id, origin, display_name, os, status, public_key
                 FROM device_pairing_request;",
        ).map_err(sql_err)?;
        ensure_column(tx, "device_sync_outbox", "payload_revision", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(tx, "device_sync_outbox", "snapshot_next_identity", "TEXT")?;
        ensure_column(tx, "device_sync_outbox", "snapshot_dirty_revision", "INTEGER")?;
        ensure_column(tx, "device_sync_outbox", "snapshot_complete", "INTEGER")?;
        ensure_column(tx, "device_sync_journal", "payload_revision", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(tx, "device_sync_journal", "dirty_revision", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(tx, "device_sync_secondary_config", "last_connected_at_ms", "INTEGER")?;
        ensure_column(tx, "device_sync_secondary_config", "last_ack_generation", "INTEGER")?;
        ensure_column(tx, "device_sync_secondary_config", "last_ack_receipt_hash", "BLOB")?;
        ensure_column(tx, "device_sync_secondary_config", "capture_enabled", "INTEGER NOT NULL DEFAULT 1")?;
        ensure_column(tx, "device_receipt", "origin", "TEXT NOT NULL DEFAULT ''")?;
        tx.execute(
            "UPDATE device_receipt SET origin = COALESCE((SELECT origin FROM device_registry WHERE device_registry.device_id = device_receipt.device_id), '') WHERE origin = ''",
            [],
        ).map_err(sql_err)?;
        ensure_column(tx, "device_pairing_invite", "bound_request_id", "TEXT")?;
        tx.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS device_pairing_invite_bound_request
                 ON device_pairing_invite(bound_request_id) WHERE bound_request_id IS NOT NULL;",
        ).map_err(sql_err)?;
        let stored: Option<i64> = tx.query_row("SELECT value FROM device_sync_meta WHERE key = 'migration_version'", [], |row| row.get::<_, String>(0).and_then(|v| v.parse::<i64>().map_err(|_| rusqlite::Error::InvalidQuery))).optional().map_err(sql_err)?;
        if let Some(version) = stored {
            if version > STATE_VERSION {
                return Err(SyncError::Unsupported(format!("device sync migration {version} is newer than {STATE_VERSION}")));
            }
        }
        let existing_primary: Option<String> = tx.query_row("SELECT value FROM device_sync_meta WHERE key = 'primary_id'", [], |row| row.get(0)).optional().map_err(sql_err)?;
        if let Some(existing) = existing_primary {
            if existing != primary_id {
                return Err(SyncError::Security("index is bound to another primary_id".into()));
            }
        } else {
            tx.execute("INSERT INTO device_sync_meta(key, value) VALUES('primary_id', ?1)", params![primary_id]).map_err(sql_err)?;
        }
        tx.execute("INSERT OR REPLACE INTO device_sync_meta(key, value) VALUES('migration_version', ?1)", params![STATE_VERSION.to_string()]).map_err(sql_err)?;
        Ok::<_, SyncError>(())
    })
}

fn validate_identifier(label: &str, value: &str) -> SyncResult<()> {
    if value.is_empty() || value.len() > 16 * 1024 || value.chars().any(char::is_control) {
        return Err(SyncError::Invalid(format!("{label} is empty, too long, or contains control characters")));
    }
    Ok(())
}

fn read_secondary_config_conn(conn: &Connection) -> SyncResult<Option<SecondarySyncConfig>> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'device_sync_secondary_config')",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    if !exists { return Ok(None); }
    let has_last_connected = {
        let mut statement = conn.prepare("PRAGMA table_info(device_sync_secondary_config)").map_err(sql_err)?;
        let names = statement.query_map([], |row| row.get::<_, String>(1)).map_err(sql_err)?;
        names.collect::<Result<Vec<_>, _>>().map_err(sql_err)?.iter().any(|name| name == "last_connected_at_ms")
    };
    let has_last_ack_generation = {
        let mut statement = conn.prepare("PRAGMA table_info(device_sync_secondary_config)").map_err(sql_err)?;
        let names = statement.query_map([], |row| row.get::<_, String>(1)).map_err(sql_err)?;
        names.collect::<Result<Vec<_>, _>>().map_err(sql_err)?.iter().any(|name| name == "last_ack_generation")
    };
    let has_last_ack_receipt_hash = {
        let mut statement = conn.prepare("PRAGMA table_info(device_sync_secondary_config)").map_err(sql_err)?;
        let names = statement.query_map([], |row| row.get::<_, String>(1)).map_err(sql_err)?;
        names.collect::<Result<Vec<_>, _>>().map_err(sql_err)?.iter().any(|name| name == "last_ack_receipt_hash")
    };
    let has_capture_enabled = {
        let mut statement = conn.prepare("PRAGMA table_info(device_sync_secondary_config)").map_err(sql_err)?;
        let names = statement.query_map([], |row| row.get::<_, String>(1)).map_err(sql_err)?;
        names.collect::<Result<Vec<_>, _>>().map_err(sql_err)?.iter().any(|name| name == "capture_enabled")
    };
    let row: Option<(
        String, String, String, String, String, Vec<u8>, Vec<u8>, String,
        Option<String>, Option<i64>, Option<i64>, i64, Option<String>, Option<Vec<u8>>, Option<Vec<u8>>,
    )> = conn.query_row(
        "SELECT primary_id, device_id, origin, endpoint, server_name, certificate_der,
                certificate_pin, identity_path, stream_id, data_epoch, expected_generation,
                enabled, last_error, last_manifest_hash, last_gzip_hash
           FROM device_sync_secondary_config WHERE config_key = 'secondary'",
        [],
        |row| Ok((
            row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
            row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?,
            row.get(10)?, row.get(11)?, row.get(12)?, row.get(13)?, row.get(14)?,
        )),
    ).optional().map_err(sql_err)?;
    let Some((primary_id, device_id, origin, endpoint, server_name, certificate_der, pin, identity_path, stream_id, data_epoch, expected_generation, enabled, last_error, manifest_hash, gzip_hash)) = row else {
        return Ok(None);
    };
    let last_connected_at_ms = if has_last_connected {
        conn.query_row(
            "SELECT last_connected_at_ms FROM device_sync_secondary_config WHERE config_key = 'secondary'",
            [], |row| row.get::<_, Option<i64>>(0),
        ).map_err(sql_err)?
    } else {
        None
    };
    let last_ack_generation = if has_last_ack_generation {
        conn.query_row(
            "SELECT last_ack_generation FROM device_sync_secondary_config WHERE config_key = 'secondary'",
            [], |row| row.get::<_, Option<i64>>(0),
        ).map_err(sql_err)?.map(|value| u64::try_from(value).map_err(|_| SyncError::Security("stored secondary ACK generation is negative".into()))).transpose()?
    } else {
        None
    };
    let last_ack_receipt_hash = if has_last_ack_receipt_hash {
        let value: Option<Vec<u8>> = conn.query_row(
            "SELECT last_ack_receipt_hash FROM device_sync_secondary_config WHERE config_key = 'secondary'",
            [], |row| row.get(0),
        ).map_err(sql_err)?;
        value.map(|bytes| bytes.try_into().map_err(|_| SyncError::Security("stored secondary ACK receipt hash is not 32 bytes".into()))).transpose()?
    } else {
        None
    };
    let capture_enabled = if has_capture_enabled {
        conn.query_row(
            "SELECT capture_enabled FROM device_sync_secondary_config WHERE config_key = 'secondary'",
            [], |row| row.get::<_, i64>(0),
        ).map_err(sql_err)? == 1
    } else {
        // Pre-capture-enabled tables had no way to pause local capture; keep
        // their historical behavior when read without a writer migration.
        true
    };
    let certificate_pin: [u8; 32] = pin.try_into().map_err(|_| SyncError::Security("stored secondary certificate pin is not 32 bytes".into()))?;
    let hash = |value: Option<Vec<u8>>, label: &str| -> SyncResult<Option<[u8; 32]>> {
        value.map(|bytes| bytes.try_into().map_err(|_| SyncError::Security(format!("stored secondary {label} is not 32 bytes")))).transpose()
    };
    let data_epoch = data_epoch.map(|value| u64::try_from(value).map_err(|_| SyncError::Security("stored secondary data_epoch is negative".into()))).transpose()?;
    let expected_generation = expected_generation.map(|value| u64::try_from(value).map_err(|_| SyncError::Security("stored secondary expected_generation is negative".into()))).transpose()?;
    let config = SecondarySyncConfig {
        primary_id, device_id, origin, endpoint, server_name, certificate_der, certificate_pin,
        identity_path: PathBuf::from(identity_path), stream_id, data_epoch, expected_generation,
        enabled: enabled != 0, capture_enabled, last_error, last_manifest_hash: hash(manifest_hash, "manifest hash")?,
        last_gzip_hash: hash(gzip_hash, "gzip hash")?, last_connected_at_ms, last_ack_generation,
        last_ack_receipt_hash,
    };
    config.validate(&config.primary_id)?;
    Ok(Some(config))
}

fn random_id(prefix: &str) -> SyncResult<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| SyncError::Crypto(format!("OS randomness unavailable: {e}")))?;
    let mut out = String::with_capacity(prefix.len() + 32);
    out.push_str(prefix);
    for byte in bytes { out.push_str(&format!("{byte:02x}")); }
    Ok(out)
}

fn device_id_from_public_key(public_key: &[u8; 32]) -> String {
    let digest = hash_bytes(public_key);
    let mut out = String::from("dev-");
    for byte in digest { out.push_str(&format!("{byte:02x}")); }
    out
}

fn sql_err(error: rusqlite::Error) -> SyncError {
    SyncError::Sqlite(error.to_string())
}

fn device_tx(conn: &Connection, device_id: &str) -> SyncResult<Option<DeviceRecord>> {
    let row: Option<(String, String, String, Vec<u8>, String, i64)> = conn.query_row(
        "SELECT origin, display_name, os, public_key, status, data_epoch FROM device_registry WHERE device_id = ?1",
        params![device_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    ).optional().map_err(sql_err)?;
    let Some((origin, display_name, os, public_key, status, data_epoch)) = row else { return Ok(None); };
    let public_key: [u8; 32] = public_key.as_slice().try_into().map_err(|_| SyncError::Security("registered public key length is invalid".into()))?;
    let status = DeviceStatus::parse(&status)?;
    let active: Option<(String, i64)> = conn.query_row(
        "SELECT stream_id, last_generation FROM device_stream WHERE device_id = ?1 AND active = 1",
        params![device_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(sql_err)?;
    Ok(Some(DeviceRecord { device_id: device_id.to_string(), origin, display_name, os, public_key, role: DeviceRole::Secondary, status, data_epoch: data_epoch as u64, active_stream_id: active.as_ref().map(|v| v.0.clone()), last_generation: active.map(|v| v.1 as u64), }))
}

fn read_receipt(conn: &Connection, device_id: &str, stream_id: &str, generation: u64) -> SyncResult<Option<ImportReceipt>> {
    let row: Option<(String, String, String, String, i64, i64, Vec<u8>, Vec<u8>, i64, i64, i64, i64, i64, Vec<u8>)> = conn.query_row(
        "SELECT primary_id, device_id, origin, stream_id, data_epoch, generation, manifest_hash, gzip_hash, rows, inserted, updated, deduped, merged_at_ms, receipt_hash FROM device_receipt WHERE device_id = ?1 AND stream_id = ?2 AND generation = ?3",
        params![device_id, stream_id, generation as i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?, row.get(11)?, row.get(12)?, row.get(13)?)),
    ).optional().map_err(sql_err)?;
    let Some((primary_id, device_id, origin, stream_id, data_epoch, generation, manifest_hash, gzip_hash, rows, inserted, updated, deduped, merged_at_ms, receipt_hash)) = row else { return Ok(None); };
    let receipt = ImportReceipt {
        primary_id,
        device_id,
        origin,
        stream_id,
        data_epoch: data_epoch as u64,
        generation: generation as u64,
        manifest_hash: blob32(&manifest_hash)?,
        gzip_hash: blob32(&gzip_hash)?,
        rows: rows as u64,
        inserted: inserted as u64,
        updated: updated as u64,
        deduped: deduped as u64,
        merged_at_ms,
        receipt_hash: blob32(&receipt_hash)?,
    };
    if receipt.compute_hash() != receipt.receipt_hash {
        return Err(SyncError::Security("stored receipt hash is invalid".into()));
    }
    Ok(Some(receipt))
}

fn read_outbox(conn: &Connection, id: i64) -> SyncResult<Option<OutboxEntry>> {
    let row: Option<(String, String, String, String, i64, i64, i64, Option<String>, Option<i64>, Option<i64>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, String, i64, i64, Option<String>)> = conn.query_row(
        "SELECT device_id, primary_id, origin, stream_id, data_epoch, generation, payload_revision,
                snapshot_next_identity, snapshot_dirty_revision, snapshot_complete,
                manifest_bytes, gzip_sha256, signature, gzip_bytes, state, attempts, next_attempt_ms, last_error
           FROM device_sync_outbox WHERE id = ?1",
        params![id],
        |row| Ok((
            row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?,
            row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?, row.get(11)?, row.get(12)?, row.get(13)?,
            row.get(14)?, row.get(15)?, row.get(16)?, row.get(17)?,
        )),
    ).optional().map_err(sql_err)?;
    let Some((device_id, primary_id, origin, stream_id, data_epoch, generation, revision, snapshot_next_identity, snapshot_dirty_revision, snapshot_complete, manifest_bytes, gzip_sha256, signature, gzip_bytes, state, attempts, next_attempt_ms, last_error)) = row else { return Ok(None); };
    let state = match state.as_str() {
        "pending" => OutboxState::Pending,
        "in_flight" => OutboxState::InFlight,
        other => return Err(SyncError::Security(format!("invalid outbox state {other:?}"))),
    };
    Ok(Some(OutboxEntry {
        id,
        device_id,
        primary_id,
        origin,
        stream_id,
        data_epoch: data_epoch as u64,
        generation: generation as u64,
        revision: revision as u64,
        manifest_bytes,
        gzip_sha256: blob32(&gzip_sha256)?,
        signature: signature.as_slice().try_into().map_err(|_| SyncError::Security("outbox signature has invalid length".into()))?,
        gzip_bytes,
        state,
        attempts: attempts as u64,
        next_attempt_ms,
        last_error,
        snapshot_next_identity,
        snapshot_dirty_revision: snapshot_dirty_revision.map(|value| value as u64),
        snapshot_complete: snapshot_complete.map(|value| value != 0),
    }))
}

fn blob32(value: &[u8]) -> SyncResult<[u8; 32]> {
    value.try_into().map_err(|_| SyncError::Security("stored hash has invalid length".into()))
}

fn import_tx_reader<R: Read>(
    tx: &Transaction<'_>,
    primary_id: &str,
    device_id: &str,
    stream_id: &str,
    data_epoch: u64,
    generation: u64,
    origin: &str,
    manifest_hash: [u8; 32],
    gzip_hash: [u8; 32],
    manifest_bytes: &[u8],
    compressed: R,
    compressed_len: usize,
    merged_at_ms: i64,
) -> SyncResult<ImportReceipt> {
    let registry: Option<(Vec<u8>, String, i64, String)> = tx.query_row(
        "SELECT public_key, status, data_epoch, origin FROM device_registry WHERE device_id = ?1",
        params![device_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).optional().map_err(sql_err)?;
    let Some((_public_key, status, current_epoch, registered_origin)) = registry else { return Err(SyncError::NotPaired(device_id.into())); };
    if status == "revoked" { return Err(SyncError::Revoked(device_id.into())); }
    if status != "paired" { return Err(SyncError::NotPaired(device_id.into())); }
    if registered_origin != origin {
        return Err(SyncError::Auth("upload origin does not match paired device".into()));
    }
    if current_epoch as u64 != data_epoch { return Err(SyncError::EpochStale); }

    let existing_receipt = if let Some(receipt) = read_receipt(tx, device_id, stream_id, generation)? {
        if !receipt_marker_exists(tx, &receipt)? {
            return Err(SyncError::ReconcileRequired);
        }
        if receipt.manifest_hash != manifest_hash || receipt.gzip_hash != gzip_hash {
            return Err(SyncError::Conflict("same stream generation has different package bytes".into()));
        }
        Some(receipt)
    } else { None };
    if package_tombstone_exists(tx, device_id, &manifest_hash, &gzip_hash)? {
        return Err(SyncError::ReconcileRequired);
    }
    if let Some(receipt) = existing_receipt {
        let mut row_count = 0u64;
        validate_network_reader(manifest_bytes, compressed, compressed_len, gzip_hash, |row| {
            row_count = row_count.checked_add(1).ok_or_else(|| SyncError::Limit("row count overflow".into()))?;
            drop(row);
            Ok(())
        })?;
        if row_count != receipt.rows { return Err(SyncError::Protocol("replayed receipt row count changed".into())); }
        return Ok(receipt);
    }

    let stream: Option<(i64, i64, i64)> = tx.query_row(
        "SELECT data_epoch, expected_generation, active FROM device_stream WHERE device_id = ?1 AND stream_id = ?2",
        params![device_id, stream_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(sql_err)?;
    let Some((stream_epoch, expected_generation, active)) = stream else { return Err(SyncError::StreamClosed(stream_id.into())); };
    if stream_epoch as u64 != data_epoch { return Err(SyncError::EpochStale); }
    if active == 0 { return Err(SyncError::StreamClosed(stream_id.into())); }
    if generation != expected_generation as u64 {
        if generation < expected_generation as u64 { return Err(SyncError::ReconcileRequired); }
        return Err(SyncError::GenerationGap { expected: expected_generation as u64, received: generation });
    }

    let marker: Option<(String, i64, Vec<u8>)> = tx.query_row(
        "SELECT receipt_stream_id, receipt_generation, receipt_hash FROM device_package_marker WHERE device_id = ?1 AND manifest_hash = ?2 AND gzip_hash = ?3",
        params![device_id, manifest_hash.as_slice(), gzip_hash.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(sql_err)?;
    let cross_stream_duplicate = if let Some((receipt_stream, receipt_generation, marker_hash)) = marker {
        let original = read_receipt(tx, device_id, &receipt_stream, receipt_generation as u64)?;
        let Some(original) = original else { return Err(SyncError::ReconcileRequired); };
        if original.receipt_hash.as_slice() != marker_hash.as_slice() {
            return Err(SyncError::Security("package marker receipt hash mismatch".into()));
        }
        true
    } else {
        false
    };

    let mut outcome = MergeOutcome::default();
    let mut affected_ts = Vec::new();
    let mut row_count = 0u64;
    let _manifest = validate_network_reader(
        manifest_bytes,
        compressed,
        compressed_len,
        gzip_hash,
        |row| {
            row_count = row_count.checked_add(1).ok_or_else(|| SyncError::Limit("row count overflow".into()))?;
            if cross_stream_duplicate {
                outcome.deduped = outcome.deduped.checked_add(1).ok_or_else(|| SyncError::Limit("dedupe count overflow".into()))?;
            } else {
                let merged = merge_row(tx, device_id, origin, &row, merged_at_ms)?;
                affected_ts.extend(merged.affected_ts);
                match merged.kind {
                    RowKind::Inserted => outcome.inserted = outcome.inserted.checked_add(1).ok_or_else(|| SyncError::Limit("insert count overflow".into()))?,
                    RowKind::Updated => outcome.updated = outcome.updated.checked_add(1).ok_or_else(|| SyncError::Limit("update count overflow".into()))?,
                    RowKind::Deduped => outcome.deduped = outcome.deduped.checked_add(1).ok_or_else(|| SyncError::Limit("dedupe count overflow".into()))?,
                }
            }
            Ok(())
        },
    )?;
    if cross_stream_duplicate {
        outcome.inserted = 0;
        outcome.updated = 0;
    } else {
        usage_index::recompute_network_days(tx, &affected_ts).map_err(SyncError::from)?;
        ensure_network_rollups_finite(tx)?;
    }

    let receipt = ImportReceipt::new(primary_id, device_id, origin, stream_id, data_epoch, generation, manifest_hash, gzip_hash, row_count, outcome, merged_at_ms);
    tx.execute(
        "INSERT INTO device_receipt(device_id, stream_id, generation, data_epoch, primary_id, origin, manifest_hash, gzip_hash, rows, inserted, updated, deduped, merged_at_ms, receipt_hash) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![device_id, stream_id, generation as i64, data_epoch as i64, primary_id, origin, manifest_hash.as_slice(), gzip_hash.as_slice(), receipt.rows as i64, receipt.inserted as i64, receipt.updated as i64, receipt.deduped as i64, merged_at_ms, receipt.receipt_hash.as_slice()],
    ).map_err(sql_err)?;
    if !cross_stream_duplicate {
        tx.execute(
            "INSERT INTO device_package_marker(device_id, manifest_hash, gzip_hash, receipt_stream_id, receipt_generation, receipt_hash) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![device_id, manifest_hash.as_slice(), gzip_hash.as_slice(), stream_id, generation as i64, receipt.receipt_hash.as_slice()],
        ).map_err(sql_err)?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO device_highwater(device_id, stream_id, data_epoch, last_generation, receipt_hash, updated_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![device_id, stream_id, data_epoch as i64, generation as i64, receipt.receipt_hash.as_slice(), merged_at_ms],
    ).map_err(sql_err)?;
    tx.execute("UPDATE device_stream SET expected_generation = ?3, last_generation = ?4 WHERE device_id = ?1 AND stream_id = ?2", params![device_id, stream_id, generation as i64 + 1, generation as i64]).map_err(sql_err)?;
    Ok(receipt)
}

fn ensure_network_rollups_finite(tx: &Transaction<'_>) -> SyncResult<()> {
    let mut stmt = tx.prepare(
        "SELECT in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits FROM event_rollup",
    ).map_err(sql_err)?;
    let rows = stmt.query_map([], |row| {
        Ok([
            row.get::<_, f64>(0)?, row.get::<_, f64>(1)?, row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?, row.get::<_, f64>(4)?, row.get::<_, f64>(5)?,
        ])
    }).map_err(sql_err)?;
    for row in rows {
        if row.map_err(sql_err)?.iter().any(|value| !value.is_finite()) {
            return Err(SyncError::Limit("network rollup sum is not finite".into()));
        }
    }
    Ok(())
}

fn receipt_marker_exists(tx: &Transaction<'_>, receipt: &ImportReceipt) -> SyncResult<bool> {
    let marker: Option<(String, i64, Vec<u8>)> = tx.query_row(
        "SELECT receipt_stream_id, receipt_generation, receipt_hash FROM device_package_marker WHERE device_id = ?1 AND manifest_hash = ?2 AND gzip_hash = ?3",
        params![receipt.device_id, receipt.manifest_hash.as_slice(), receipt.gzip_hash.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(sql_err)?;
    let Some((stream, generation, hash)) = marker else { return Ok(false); };
    if hash.as_slice() != receipt.receipt_hash.as_slice() && stream == receipt.stream_id && generation as u64 == receipt.generation {
        return Err(SyncError::Security("package marker receipt hash mismatch".into()));
    }
    if stream == receipt.stream_id && generation as u64 == receipt.generation {
        return Ok(hash.as_slice() == receipt.receipt_hash.as_slice());
    }
    let Some(original) = read_receipt(tx, &receipt.device_id, &stream, generation as u64)? else {
        return Ok(false);
    };
    if original.receipt_hash.as_slice() != hash.as_slice() {
        return Err(SyncError::Security("package marker receipt hash mismatch".into()));
    }
    Ok(original.manifest_hash == receipt.manifest_hash && original.gzip_hash == receipt.gzip_hash)
}

fn package_tombstone_exists(tx: &Transaction<'_>, device_id: &str, manifest_hash: &[u8; 32], gzip_hash: &[u8; 32]) -> SyncResult<bool> {
    tx.query_row(
        "SELECT 1 FROM device_package_tombstone WHERE device_id = ?1 AND manifest_hash = ?2 AND gzip_hash = ?3",
        params![device_id, manifest_hash.as_slice(), gzip_hash.as_slice()],
        |row| row.get::<_, i64>(0),
    ).optional().map_err(sql_err).map(|v| v.is_some())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind { Inserted, Updated, Deduped }

struct MergedRow {
    kind: RowKind,
    affected_ts: Vec<i64>,
}

struct StoredEvent {
    id: i64,
    tool: String,
    ts_ms: i64,
    session: String,
    project: Option<String>,
    model: Option<String>,
    meter: String,
    counts: [Option<f64>; 6],
    source: String,
}

fn merge_row(tx: &Transaction<'_>, device_id: &str, origin: &str, row: &NetworkRow, merged_at_ms: i64) -> SyncResult<MergedRow> {
    let original = row.identity_material()?;
    let dedupe_key = network_event_key(device_id, original);
    let source = format!("linux:{origin}:{original}");
    let stored: Option<StoredEvent> = tx.query_row(
        "SELECT id, tool, ts_ms, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, source FROM event WHERE dedupe_key = ?1",
        params![dedupe_key],
        |r| Ok(StoredEvent { id: r.get(0)?, tool: r.get(1)?, ts_ms: r.get(2)?, session: r.get(3)?, project: r.get(4)?, model: r.get(5)?, meter: r.get(6)?, counts: [r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?], source: r.get(13)? }),
    ).optional().map_err(sql_err)?;
    let Some(mut old) = stored else {
        tx.execute(
            "INSERT INTO event(tool, ts_ms, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, dedupe_key, source) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![row.tool, row.ts_ms, row.session, row.project, row.model, row.meter, row.counts.in_tok, row.counts.cc_tok, row.counts.cr_tok, row.counts.out_tok, row.counts.reason_tok, row.counts.credits, dedupe_key, source],
        ).map_err(sql_err)?;
        let event_id = tx.last_insert_rowid();
        insert_calls(tx, event_id, &row.calls)?;
        return Ok(MergedRow { kind: RowKind::Inserted, affected_ts: vec![row.ts_ms] });
    };
    if origin_of(&old.source) != origin {
        return Err(SyncError::Conflict("network event key points at another origin".into()));
    }
    if old.meter != row.meter {
        return Err(SyncError::Conflict("event meter changed for a stable identity".into()));
    }
    let old_ts = old.ts_ms;
    let mut changed = false;
    changed |= merge_text(&mut old.tool, &row.tool);
    changed |= merge_text(&mut old.session, &row.session);
    changed |= merge_optional_text(&mut old.project, &row.project);
    changed |= merge_optional_text(&mut old.model, &row.model);
    if row.ts_ms < old.ts_ms { old.ts_ms = row.ts_ms; changed = true; }
    let old_tokens = [old.counts[0], old.counts[1], old.counts[2], old.counts[3], old.counts[4]];
    let new_tokens = [row.counts.in_tok, row.counts.cc_tok, row.counts.cr_tok, row.counts.out_tok, row.counts.reason_tok];
    let (tokens, conflict) = merge_vector(old_tokens, new_tokens);
    if conflict {
        record_conflict(tx, device_id, &dedupe_key, old_tokens, new_tokens, merged_at_ms)?;
    }
    for (slot, value) in old.counts.iter_mut().take(5).zip(tokens) {
        if *slot != value { *slot = value; changed = true; }
    }
    let credits = match (old.counts[5], row.counts.credits) {
        (Some(old), Some(new)) => Some(old.max(new)),
        (Some(old), None) => Some(old),
        (None, value) => value,
    };
    if old.counts[5] != credits { old.counts[5] = credits; changed = true; }
    let call_changed = union_calls(tx, old.id, &row.calls)?;
    if call_changed { changed = true; }
    if changed {
        tx.execute(
            "UPDATE event SET tool = ?2, ts_ms = ?3, session = ?4, project = ?5, model = ?6, in_tok = ?7, cc_tok = ?8, cr_tok = ?9, out_tok = ?10, reason_tok = ?11, credits = ?12 WHERE id = ?1",
            params![old.id, old.tool, old.ts_ms, old.session, old.project, old.model, old.counts[0], old.counts[1], old.counts[2], old.counts[3], old.counts[4], old.counts[5]],
        ).map_err(sql_err)?;
    }
    Ok(MergedRow { kind: if changed { RowKind::Updated } else { RowKind::Deduped }, affected_ts: vec![old_ts, old.ts_ms] })
}

fn insert_calls(tx: &Transaction<'_>, event_id: i64, calls: &[crate::protocol::NetworkCall]) -> SyncResult<()> {
    for call in calls {
        tx.execute("INSERT INTO call(event_id, kind, name) VALUES(?1, ?2, ?3)", params![event_id, call.kind, call.name]).map_err(sql_err)?;
    }
    Ok(())
}

fn union_calls(tx: &Transaction<'_>, event_id: i64, calls: &[crate::protocol::NetworkCall]) -> SyncResult<bool> {
    let mut existing = BTreeMap::<(String, String), usize>::new();
    let mut stmt = tx.prepare("SELECT kind, name, count(*) FROM call WHERE event_id = ?1 GROUP BY kind, name").map_err(sql_err)?;
    let rows = stmt.query_map(params![event_id], |row| {
        Ok(((row.get::<_, String>(0)?, row.get::<_, String>(1)?), row.get::<_, i64>(2)?))
    }).map_err(sql_err)?;
    for row in rows {
        let (key, count) = row.map_err(sql_err)?;
        existing.insert(key, usize::try_from(count).map_err(|_| SyncError::Security("stored call multiplicity is negative".into()))?);
    }
    drop(stmt);
    let mut incoming = BTreeMap::<(String, String), usize>::new();
    for call in calls {
        let key = (call.kind.clone(), call.name.clone());
        let count = incoming.entry(key).or_default();
        *count = count.checked_add(1).ok_or_else(|| SyncError::Limit("call multiplicity overflow".into()))?;
    }
    let mut changed = false;
    for ((kind, name), wanted) in incoming {
        let have = existing.get(&(kind.clone(), name.clone())).copied().unwrap_or(0);
        for _ in have..wanted {
            tx.execute("INSERT INTO call(event_id, kind, name) VALUES(?1, ?2, ?3)", params![event_id, kind, name]).map_err(sql_err)?;
            changed = true;
        }
    }
    Ok(changed)
}

fn merge_text(old: &mut String, incoming: &str) -> bool {
    if incoming < old.as_str() {
        *old = incoming.to_string();
        true
    } else { false }
}

fn merge_optional_text(old: &mut Option<String>, incoming: &Option<String>) -> bool {
    match (old.as_ref(), incoming.as_ref()) {
        (None, Some(value)) => { *old = Some(value.clone()); true }
        (Some(current), Some(value)) if value < current => { *old = Some(value.clone()); true }
        _ => false,
    }
}

fn compare_component(left: &Option<f64>, right: &Option<f64>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(left), Some(right)) => left.total_cmp(right),
    }
}

fn compare_vectors(left: &[Option<f64>; 5], right: &[Option<f64>; 5]) -> Ordering {
    left.iter()
        .zip(right)
        .map(|(left, right)| compare_component(left, right))
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

fn vector_blob(value: &[Option<f64>; 5]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(40);
    for part in value {
        match part {
            Some(value) => bytes.extend_from_slice(&value.to_bits().to_be_bytes()),
            None => bytes.extend_from_slice(&u64::MAX.to_be_bytes()),
        }
    }
    bytes
}

fn hash_vector(value: &[Option<f64>; 5]) -> [u8; 32] {
    hash_bytes(&vector_blob(value))
}

fn vector_complete(value: &[Option<f64>; 5]) -> bool {
    value.iter().all(Option::is_some)
}

fn vector_dominates(left: &[Option<f64>; 5], right: &[Option<f64>; 5]) -> bool {
    let mut strict = false;
    for (left, right) in left.iter().zip(right) {
        match compare_component(left, right) {
            Ordering::Less => return false,
            Ordering::Greater => strict = true,
            Ordering::Equal => {}
        }
    }
    strict
}

fn merge_vector(old: [Option<f64>; 5], incoming: [Option<f64>; 5]) -> ([Option<f64>; 5], bool) {
    if old == incoming {
        return (old, false);
    }
    if vector_dominates(&incoming, &old) {
        return (incoming, false);
    }
    if vector_dominates(&old, &incoming) {
        return (old, false);
    }
    let conflict = vector_complete(&old) && vector_complete(&incoming);
    if compare_vectors(&incoming, &old) == Ordering::Greater {
        (incoming, conflict)
    } else {
        (old, conflict)
    }
}

/// Record only the pair observed by this merge. This is a bounded diagnostic
/// log, not a history of every value ever seen for the event.
fn record_conflict(
    tx: &Transaction<'_>,
    device_id: &str,
    dedupe_key: &str,
    old: [Option<f64>; 5],
    incoming: [Option<f64>; 5],
    created_at_ms: i64,
) -> SyncResult<()> {
    let old_hash = hash_vector(&old);
    let incoming_hash = hash_vector(&incoming);
    let (first_hash, second_hash) = if old_hash <= incoming_hash { (old_hash, incoming_hash) } else { (incoming_hash, old_hash) };
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM device_sync_conflict WHERE device_id = ?1 AND dedupe_key = ?2 AND field_schema = ?3 AND old_hash = ?4 AND new_hash = ?5)",
        params![device_id, dedupe_key, TOKENS_SCHEMA, first_hash.as_slice(), second_hash.as_slice()],
        |row| row.get(0),
    ).map_err(sql_err)?;
    if exists {
        tx.execute(
            "UPDATE device_sync_conflict SET created_at_ms = MIN(created_at_ms, ?6) WHERE device_id = ?1 AND dedupe_key = ?2 AND field_schema = ?3 AND old_hash = ?4 AND new_hash = ?5",
            params![device_id, dedupe_key, TOKENS_SCHEMA, first_hash.as_slice(), second_hash.as_slice(), created_at_ms],
        ).map_err(sql_err)?;
        return Ok(());
    }
    let current_conflicts: i64 = tx.query_row(
        "SELECT count(*) FROM device_sync_conflict WHERE device_id = ?1 AND dedupe_key = ?2 AND field_schema = ?3",
        params![device_id, dedupe_key, TOKENS_SCHEMA],
        |row| row.get(0),
    ).map_err(sql_err)?;
    if current_conflicts >= MAX_TOKEN_CONFLICT_PAIRS_PER_KEY {
        return Err(SyncError::Limit(format!("token conflict pairs exceed {MAX_TOKEN_CONFLICT_PAIRS_PER_KEY} for one event")));
    }
    tx.execute(
        "INSERT INTO device_sync_conflict(device_id, dedupe_key, field_schema, old_hash, new_hash, created_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![device_id, dedupe_key, TOKENS_SCHEMA, first_hash.as_slice(), second_hash.as_slice(), created_at_ms],
    ).map_err(sql_err)?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DeviceIdentity, NetworkCounts, NetworkManifest, NetworkPackage, NetworkRow, NetworkSums, NetworkCall, PROTOCOL_ID, UploadEnvelope};
    use usage_core::{DateFilter, DetectedSource, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile, UsageEvent};

    fn identity(origin: &str) -> DeviceIdentity {
        DeviceIdentity::create(origin, origin).unwrap()
    }

    struct CaptureFixture {
        path: PathBuf,
    }

    impl SourceAdapter for CaptureFixture {
        fn id(&self) -> &'static str { "capture-fixture" }
        fn display_name(&self) -> &'static str { "Capture fixture" }
        fn semantics(&self) -> Semantics { Semantics::TOKENS_PER_CALL_INLINE }
        fn probe(&self) -> Option<DetectedSource> { None }
        fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
            let metadata = std::fs::metadata(&self.path).unwrap();
            vec![SourceFile { path: self.path.clone(), kind: FileKind::Jsonl, size: metadata.len(), mtime_ms: 1 }]
        }
        fn read(&self, file: &SourceFile, cursor: ReadCursor) -> usage_core::Result<ReadOutcome> {
            if cursor.0 != 0 {
                return Ok(ReadOutcome { events: Vec::new(), cursor: ReadCursor(file.size) });
            }
            let mut event = UsageEvent::new(self.id(), 1_789_172_800_000, "capture-session");
            event.dedupe_key = Some("capture-event".into());
            event.counts.input = 1.0;
            event.source = file.key();
            Ok(ReadOutcome { events: vec![event], cursor: ReadCursor(file.size) })
        }
    }

    #[test]
    fn status_bridge_reads_without_migrating_or_exposing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let mut store = SyncStore::open(&path, "primary-1").unwrap();
            let device = identity("linux");
            let invite = store.create_pairing_invite("https://localhost:1", "0000000000000000000000000000000000000000000000000000000000000000", i64::MAX).unwrap();
            let request = PairingRequest::new(&invite, &device, "linux").unwrap();
            let state = store.receive_pairing_request(&request, 1).unwrap();
            store.approve_pairing(&state.request_id, 2).unwrap();
            drop(store);
        }
        let status = SyncStore::read_status(&path).unwrap();
        assert_eq!(status.primary_id.as_deref(), Some("primary-1"));
        assert_eq!(status.devices.len(), 1);
        assert_eq!(status.devices[0].origin, "linux");
        assert_eq!(status.devices[0].status, DeviceStatus::Paired);
        assert!(status.pairing_requests[0].status == "approved");
    }

    #[test]
    fn authenticated_status_accepts_a_paired_zero_event_device() {
        let device = identity("zero-event");
        let (store, _stream) = paired_store_for(&device);
        let request = AuthenticatedStatusRequest::new_at(&device, "primary-1", false, 10).unwrap();
        let status = store.authenticated_status(&request, 20).unwrap();
        assert_eq!(status.status, "paired");
        assert_eq!(status.coverage_lo_ms, None);
        assert_eq!(status.coverage_hi_ms, None);
        assert_eq!(status.highwater_generation, None);
    }

    #[test]
    fn authenticated_status_surfaces_a_corrupt_highwater_hash() {
        let device = identity("corrupt-highwater");
        let (store, stream) = paired_store_for(&device);
        store.index().conn().execute(
            "INSERT INTO device_highwater(device_id, stream_id, data_epoch, last_generation, receipt_hash, updated_at_ms) VALUES(?1, ?2, ?3, 1, ?4, 5)",
            params![device.device_id, stream.stream_id, stream.data_epoch as i64, vec![7u8; 31]],
        ).unwrap();
        let request = AuthenticatedStatusRequest::new_at(&device, "primary-1", false, 10).unwrap();
        assert!(matches!(store.authenticated_status(&request, 20), Err(SyncError::Security(message)) if message.contains("hash")));
    }

    #[test]
    fn authenticated_status_returns_the_requested_checkpoint_receipt_when_highwater_is_newer() {
        let device = identity("checkpoint-status");
        let (mut store, stream) = paired_store_for(&device);
        let first = store.import_network_strict(&upload(&device, &stream, package("checkpoint-status", "checkpoint-1", NetworkCounts::all_zero(), vec![]), 1), 20).unwrap();
        let second = store.import_network_strict(&upload(&device, &stream, package("checkpoint-status", "checkpoint-2", NetworkCounts::all_zero(), vec![]), 2), 21).unwrap();
        let request = AuthenticatedStatusRequest::new_at_with_checkpoint(&device, "primary-1", false, Some(1), 30).unwrap();
        let status = store.authenticated_status(&request, 40).unwrap();
        assert_eq!(status.checkpoint_generation, Some(1));
        assert_eq!(status.checkpoint_receipt_hash, Some(first.receipt_hash));
        assert_eq!(status.highwater_generation, Some(2));
        assert_eq!(status.highwater_receipt_hash, Some(second.receipt_hash));
    }

    #[test]
    fn recovery_checks_current_status_before_nonce_idempotency_and_rejects_expired_replay() {
        let device = identity("recovery-replay");
        let (mut store, _stream) = paired_store_for(&device);
        let request = AuthenticatedStatusRequest::new_at(&device, "primary-1", true, 100).unwrap();
        let first = store.recover_stream(&request, 100).unwrap();
        store.revoke_device(&device.device_id, 101).unwrap();
        assert!(matches!(store.recover_stream(&request, 102), Err(SyncError::Revoked(_))));
        assert_ne!(first.stream_id, store.device(&device.device_id).unwrap().and_then(|d| d.active_stream_id).unwrap_or_default());

        let device = identity("expired-recovery");
        let (mut store, _stream) = paired_store_for(&device);
        let request = AuthenticatedStatusRequest::new_at(&device, "primary-1", true, 200).unwrap();
        let first = store.recover_stream(&request, 200).unwrap();
        let expired_at = 200 + 10 * 60 * 1000 + 1;
        assert!(matches!(store.recover_stream(&request, expired_at), Err(SyncError::Auth(message)) if message.contains("expired")));
        assert_eq!(store.device(&device.device_id).unwrap().unwrap().active_stream_id.as_deref(), Some(first.stream_id.as_str()));
    }

    fn package(origin: &str, key: &str, counts: NetworkCounts, calls: Vec<NetworkCall>) -> NetworkPackage {
        NetworkPackage::from_rows(origin, INDEX_SCHEMA_VERSION, 0, 10_000, &[NetworkRow { key: Some(key.into()), occurrence_id: None, tool: "codex".into(), ts_ms: 10, session: "session".into(), project: Some("project".into()), model: Some("model".into()), meter: "tokens".into(), source: "local.jsonl".into(), counts, calls }], 1).unwrap()
    }

    fn upload(device: &DeviceIdentity, stream: &StreamOpenReceipt, package: NetworkPackage, generation: u64) -> SignedUpload {
        let envelope = UploadEnvelope { protocol_id: PROTOCOL_ID.into(), primary_id: stream.primary_id.clone(), device_id: device.device_id.clone(), origin: device.origin.clone(), stream_id: stream.stream_id.clone(), data_epoch: stream.data_epoch, generation, manifest_bytes: package.manifest_bytes.clone(), gzip_sha256: hash_bytes(&package.gzip_bytes) };
        SignedUpload::sign(envelope, &device.signing_key, package.gzip_bytes).unwrap()
    }

    fn paired_store_for(device: &DeviceIdentity) -> (SyncStore, StreamOpenReceipt) {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let invite = store.create_pairing_invite("https://lan:47821", "1111111111111111111111111111111111111111111111111111111111111111", 10_000).unwrap();
        let request = PairingRequest::new(&invite, device, "linux").unwrap();
        let request = store.receive_pairing_request(&request, 1).unwrap();
        store.approve_pairing(&request.request_id, 2).unwrap();
        let stream = store.open_stream(&device.device_id, &device.public_key(), 3).unwrap();
        (store, stream)
    }

    fn paired_store() -> (SyncStore, DeviceIdentity, StreamOpenReceipt) {
        let device = identity("laptop");
        let (store, stream) = paired_store_for(&device);
        (store, device, stream)
    }

    #[test]
    fn pairing_approval_and_stream_are_stateful() {
        let (store, device, stream) = paired_store();
        let record = store.device(&device.device_id).unwrap().unwrap();
        assert_eq!(record.status, DeviceStatus::Paired);
        assert_eq!(record.active_stream_id.as_deref(), Some(stream.stream_id.as_str()));
        assert_eq!(stream.expected_generation, 1);
    }

    #[test]
    fn exact_replay_returns_original_receipt() {
        let (mut store, device, stream) = paired_store();
        let pkg = package("laptop", "k1", NetworkCounts { in_tok: Some(1.0), cc_tok: Some(2.0), cr_tok: Some(3.0), out_tok: Some(4.0), reason_tok: Some(5.0), credits: Some(1.0) }, vec![]);
        let signed = upload(&device, &stream, pkg, 1);
        let first = store.import_network_strict(&signed, 20).unwrap();
        let replay = store.import_network_strict(&signed, 999).unwrap();
        assert_eq!(first, replay);
        assert_eq!(first.inserted, 1);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM event", [], |r| r.get(0)).unwrap(), 1);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM device_sync_conflict", [], |r| r.get(0)).unwrap(), 0);
    }

    #[test]
    fn exact_receipt_retry_is_allowed_after_stream_is_inactive() {
        let (mut store, device, stream) = paired_store();
        let package = package("laptop", "inactive-retry", NetworkCounts::all_zero(), vec![]);
        let upload = upload(&device, &stream, package, 1);
        store.import_network_strict(&upload, 20).unwrap();
        let _replacement = store.open_stream(&device.device_id, &device.public_key(), 21).unwrap();
        store.authorize_upload_metadata(&upload.envelope, &upload.signature).unwrap();
        let replay = store.import_network_strict(&upload, 22).unwrap();
        assert_eq!(replay.generation, 1);
    }

    #[test]
    fn monotonic_token_growth_of_one_thousand_updates_has_no_conflict() {
        let (mut store, device, stream) = paired_store();
        for generation in 1..=1000 {
            let value = generation as f64;
            let pkg = package("laptop", "k1", NetworkCounts { in_tok: Some(value), cc_tok: Some(0.0), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(value) }, vec![]);
            store.import_network_strict(&upload(&device, &stream, pkg, generation), 20 + generation as i64).unwrap();
        }
        assert_eq!(store.index().conn().query_row::<Option<f64>, _, _>("SELECT in_tok FROM event", [], |r| r.get(0)).unwrap(), Some(1000.0));
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM device_sync_conflict", [], |r| r.get(0)).unwrap(), 0);
    }

    #[test]
    fn upload_origin_must_match_paired_device() {
        let (mut store, device, stream) = paired_store();
        let pkg = package("laptop", "k1", NetworkCounts::all_zero(), vec![]);
        let mut manifest: NetworkManifest = serde_json::from_slice(&pkg.manifest_bytes).unwrap();
        manifest.origin = "another-device".into();
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let signed = SignedUpload::sign(UploadEnvelope {
            protocol_id: PROTOCOL_ID.into(),
            primary_id: stream.primary_id,
            device_id: device.device_id.clone(),
            origin: "another-device".into(),
            stream_id: stream.stream_id,
            data_epoch: stream.data_epoch,
            generation: 1,
            manifest_bytes,
            gzip_sha256: hash_bytes(&pkg.gzip_bytes),
        }, &device.signing_key, pkg.gzip_bytes).unwrap();
        assert!(matches!(store.import_network_strict(&signed, 20), Err(SyncError::Auth(message)) if message.contains("origin")));
    }

    #[test]
    fn generation_gap_and_changed_same_generation_are_rejected() {
        let (mut store, device, stream) = paired_store();
        let pkg = package("laptop", "k1", NetworkCounts::all_zero(), vec![]);
        let gap = upload(&device, &stream, pkg.clone(), 2);
        assert!(matches!(store.import_network_strict(&gap, 20), Err(SyncError::GenerationGap { expected: 1, received: 2 })));
        let first = upload(&device, &stream, pkg, 1);
        store.import_network_strict(&first, 20).unwrap();
        let changed_pkg = package("laptop", "k1", NetworkCounts { in_tok: Some(9.0), ..NetworkCounts::all_zero() }, vec![]);
        let changed = upload(&device, &stream, changed_pkg, 1);
        assert!(matches!(store.import_network_strict(&changed, 21), Err(SyncError::Conflict(_))));
    }

    #[test]
    fn new_stream_same_bytes_gets_new_receipt_without_duplicate_event() {
        let (mut store, device, stream1) = paired_store();
        let pkg = package("laptop", "k1", NetworkCounts::all_zero(), vec![]);
        let first = upload(&device, &stream1, pkg.clone(), 1);
        let r1 = store.import_network_strict(&first, 20).unwrap();
        let stream2 = store.open_stream(&device.device_id, &device.public_key(), 21).unwrap();
        let second = upload(&device, &stream2, pkg, 1);
        let r2 = store.import_network_strict(&second, 22).unwrap();
        let r2_retry = store.import_network_strict(&second, 23).unwrap();
        assert_ne!(r1.receipt_hash, r2.receipt_hash);
        assert_eq!(r2_retry, r2);
        assert_eq!(r2_retry.stream_id, stream2.stream_id);
        assert_eq!(r2_retry.generation, 1);
        assert_eq!(r2.deduped, 1);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM event", [], |r| r.get(0)).unwrap(), 1);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM device_sync_conflict", [], |r| r.get(0)).unwrap(), 0);
    }

    #[test]
    fn network_call_multiplicity_is_max_merged_and_replay_idempotent() {
        let (mut store, device, stream) = paired_store();
        let repeated = vec![
            NetworkCall { kind: "mcp".into(), name: "tools/list".into() },
            NetworkCall { kind: "mcp".into(), name: "tools/list".into() },
        ];
        let first = upload(&device, &stream, package("laptop", "call-count", NetworkCounts::all_zero(), repeated.clone()), 1);
        store.import_network_strict(&first, 20).unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM call", [], |r| r.get(0)).unwrap(), 2);
        store.import_network_strict(&first, 21).unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM call", [], |r| r.get(0)).unwrap(), 2);

        let mut grown = repeated;
        grown.push(NetworkCall { kind: "mcp".into(), name: "tools/list".into() });
        let second = upload(&device, &stream, package("laptop", "call-count", NetworkCounts::all_zero(), grown), 2);
        store.import_network_strict(&second, 22).unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM call", [], |r| r.get(0)).unwrap(), 3);
        store.import_network_strict(&second, 23).unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM call", [], |r| r.get(0)).unwrap(), 3);
    }

    #[test]
    fn receipt_cleanup_requires_reconcile() {
        let (mut store, device, stream) = paired_store();
        let pkg = package("laptop", "k1", NetworkCounts::all_zero(), vec![]);
        let signed = upload(&device, &stream, pkg, 1);
        store.import_network_strict(&signed, 20).unwrap();
        store.purge_receipt(&device.device_id, &stream.stream_id, 1).unwrap();
        assert!(matches!(store.import_network_strict(&signed, 21), Err(SyncError::ReconcileRequired)));
    }

    #[test]
    fn local_rebuild_keeps_network_rows_and_receipts() {
        let (mut store, device, stream) = paired_store();
        let pkg = package("laptop", "k1", NetworkCounts::all_zero(), vec![]);
        let signed = upload(&device, &stream, pkg, 1);
        store.import_network_strict(&signed, 20).unwrap();
        let adapters: Vec<Box<dyn usage_core::SourceAdapter>> = Vec::new();
        store.index_mut().rebuild_with(&adapters, false).unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM event", [], |r| r.get(0)).unwrap(), 1);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM device_receipt", [], |r| r.get(0)).unwrap(), 1);
    }

    #[test]
    fn revoke_and_epoch_change_reject_old_upload() {
        let (mut store, device, stream) = paired_store();
        store.revoke_device(&device.device_id, 20).unwrap();
        let pkg = package("laptop", "k1", NetworkCounts::all_zero(), vec![]);
        let signed = upload(&device, &stream, pkg, 1);
        assert!(matches!(store.import_network_strict(&signed, 21), Err(SyncError::Revoked(_))));
    }

    #[test]
    fn calls_union_and_conflicting_vectors_are_order_independent() {
        let device = identity("laptop");
        let (mut a, stream_a) = paired_store_for(&device);
        let (mut b, stream_b) = paired_store_for(&device);
        let low_first = package("laptop", "k1", NetworkCounts { in_tok: Some(10.0), cc_tok: Some(0.0), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }, vec![NetworkCall { kind: "mcp".into(), name: "z".into() }]);
        let incomparable = package("laptop", "k1", NetworkCounts { in_tok: Some(9.0), cc_tok: Some(1.0), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }, vec![NetworkCall { kind: "mcp".into(), name: "a".into() }]);
        a.import_network_strict(&upload(&device, &stream_a, low_first.clone(), 1), 20).unwrap();
        a.open_stream(&device.device_id, &device.public_key(), 21).unwrap();
        let stream_a2 = a.device(&device.device_id).unwrap().unwrap().active_stream_id.unwrap();
        let stream_a2 = StreamOpenReceipt { primary_id: "primary-1".into(), device_id: device.device_id.clone(), stream_id: stream_a2, data_epoch: 1, expected_generation: 1 };
        a.import_network_strict(&upload(&device, &stream_a2, incomparable.clone(), 1), 22).unwrap();

        b.import_network_strict(&upload(&device, &stream_b, incomparable, 1), 20).unwrap();
        b.open_stream(&device.device_id, &device.public_key(), 21).unwrap();
        let stream_b2 = b.device(&device.device_id).unwrap().unwrap().active_stream_id.unwrap();
        let stream_b2 = StreamOpenReceipt { primary_id: "primary-1".into(), device_id: device.device_id.clone(), stream_id: stream_b2, data_epoch: 1, expected_generation: 1 };
        b.import_network_strict(&upload(&device, &stream_b2, low_first, 1), 22).unwrap();
        let row = |store: &SyncStore| store.index().conn().query_row::<(Option<f64>, Option<f64>), _, _>("SELECT in_tok, cc_tok FROM event", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(row(&a), row(&b));
        assert_eq!(a.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM call", [], |r| r.get(0)).unwrap(), 2);
        assert_eq!(b.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM call", [], |r| r.get(0)).unwrap(), 2);
    }

    #[test]
    fn identical_vector_call_only_revision_does_not_record_self_conflict() {
        let (mut store, device, stream) = paired_store();
        let counts = NetworkCounts { in_tok: Some(4.0), cc_tok: Some(2.0), cr_tok: Some(1.0), out_tok: Some(3.0), reason_tok: Some(0.0), credits: Some(1.0) };
        let first = package("laptop", "same-vector", counts.clone(), vec![NetworkCall { kind: "mcp".into(), name: "first".into() }]);
        let second = package("laptop", "same-vector", counts, vec![NetworkCall { kind: "mcp".into(), name: "second".into() }]);
        store.import_network_strict(&upload(&device, &stream, first, 1), 20).unwrap();
        store.import_network_strict(&upload(&device, &stream, second, 2), 21).unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM device_sync_conflict", [], |r| r.get(0)).unwrap(), 0);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM call", [], |r| r.get(0)).unwrap(), 2);
    }

    #[test]
    fn three_vectors_converge_without_claiming_history_log_order() {
        let snapshot = |order: &[usize]| {
            let device = identity("laptop");
            let (mut store, stream) = paired_store_for(&device);
            let packages = vec![
                package("laptop", "k1", NetworkCounts { in_tok: Some(2.0), cc_tok: Some(2.0), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }, vec![NetworkCall { kind: "mcp".into(), name: "z".into() }]),
                package("laptop", "k1", NetworkCounts { in_tok: Some(1.0), cc_tok: Some(1.0), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }, vec![NetworkCall { kind: "mcp".into(), name: "a".into() }]),
                package("laptop", "k1", NetworkCounts { in_tok: Some(0.0), cc_tok: Some(3.0), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }, vec![NetworkCall { kind: "mcp".into(), name: "m".into() }]),
            ];
            for (generation, index) in order.iter().enumerate() {
                store.import_network_strict(&upload(&device, &stream, packages[*index].clone(), generation as u64 + 1), 20 + generation as i64).unwrap();
            }
            let vector = store.index().conn().query_row::<Vec<Option<f64>>, _, _>(
                "SELECT in_tok, cc_tok, cr_tok, out_tok, reason_tok FROM event",
                [],
                |row| Ok(vec![row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?]),
            ).unwrap();
            let mut conflict_stmt = store.index().conn().prepare(
                "SELECT old_hash, new_hash FROM device_sync_conflict WHERE field_schema = 'tokens/v1' ORDER BY old_hash, new_hash",
            ).unwrap();
            let conflicts: Vec<(Vec<u8>, Vec<u8>)> = conflict_stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(|row| row.unwrap())
                .collect();
            let mut call_stmt = store.index().conn().prepare("SELECT kind, name FROM call ORDER BY kind, name").unwrap();
            let calls: Vec<(String, String)> = call_stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(|row| row.unwrap())
                .collect();
            (vector, conflicts, calls)
        };

        let baseline = snapshot(&[0, 1, 2]);
        for order in [[0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let state = snapshot(&order);
            assert_eq!(state.0, baseline.0, "arrival order {order:?} changed final vector");
            assert_eq!(state.2, baseline.2, "arrival order {order:?} changed calls union");
        }
        assert_eq!(baseline.0, vec![Some(2.0), Some(2.0), Some(0.0), Some(0.0), Some(0.0)]);
        assert!(baseline.1.len() <= 2, "diagnostic log contains only observed merge pairs");
        assert_eq!(baseline.2, vec![("mcp".into(), "a".into()), ("mcp".into(), "m".into()), ("mcp".into(), "z".into())]);
    }

    #[test]
    fn incomparable_revision_history_hits_a_transactional_resource_limit() {
        let (mut store, device, stream) = paired_store();
        let mut rejected = false;
        let total = MAX_TOKEN_CONFLICT_PAIRS_PER_KEY as f64 + 2.0;
        for generation in 1..=(MAX_TOKEN_CONFLICT_PAIRS_PER_KEY as u64 + 2) {
            let n = generation as f64;
            let pkg = package("laptop", "k1", NetworkCounts { in_tok: Some(n), cc_tok: Some(total - n), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }, vec![]);
            match store.import_network_strict(&upload(&device, &stream, pkg, generation), 20 + generation as i64) {
                Ok(_) => {}
                Err(SyncError::Limit(message)) => {
                    assert!(message.contains("conflict pairs"));
                    rejected = true;
                    break;
                }
                Err(error) => panic!("unexpected revision error: {error:?}"),
            }
        }
        assert!(rejected, "incomparable revisions must pause before unbounded state");
        assert!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM device_sync_conflict", [], |r| r.get(0)).unwrap() <= MAX_TOKEN_CONFLICT_PAIRS_PER_KEY);
    }

    #[test]
    fn pairing_invite_binds_one_pending_request_and_reject_requires_new_invite() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let invite = store.create_pairing_invite("https://localhost:47821", &"1".repeat(64), 100).unwrap();
        let first_device = identity("first-device");
        let first_request = PairingRequest::new(&invite, &first_device, "linux").unwrap();
        let first_state = store.receive_pairing_request(&first_request, 1).unwrap();
        assert_eq!(store.receive_pairing_request(&first_request, 2).unwrap(), first_state);

        let second_device = identity("second-device");
        let second_request = PairingRequest::new(&invite, &second_device, "linux").unwrap();
        assert!(matches!(store.receive_pairing_request(&second_request, 2), Err(SyncError::Conflict(message)) if message.contains("new invite")));
        store.reject_pairing(&first_request.request_id).unwrap();
        assert_eq!(store.receive_pairing_request(&first_request, 3).unwrap().status, "rejected");
        let retry_with_new_id = PairingRequest::new(&invite, &first_device, "linux").unwrap();
        assert!(matches!(store.receive_pairing_request(&retry_with_new_id, 3), Err(SyncError::Conflict(message)) if message.contains("new invite")));

        let replacement = store.create_pairing_invite("https://localhost:47821", &"2".repeat(64), 100).unwrap();
        let replacement_request = PairingRequest::new(&replacement, &second_device, "linux").unwrap();
        assert_eq!(store.receive_pairing_request(&replacement_request, 3).unwrap().status, "pending");
    }

    #[test]
    fn expired_pending_requests_are_released_before_capacity_check_and_boundary_is_consistent() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        for index in 0..MAX_PENDING_PAIRING_REQUESTS {
            let device = identity(&format!("expired-{index}"));
            let invite = store.create_pairing_invite("https://localhost:47821", &"3".repeat(64), 10).unwrap();
            let request = PairingRequest::new(&invite, &device, "linux").unwrap();
            store.receive_pairing_request(&request, 1).unwrap();
        }
        let fresh_device = identity("fresh-device");
        let fresh_invite = store.create_pairing_invite("https://localhost:47821", &"4".repeat(64), 100).unwrap();
        let fresh_request = PairingRequest::new(&fresh_invite, &fresh_device, "linux").unwrap();
        assert_eq!(store.receive_pairing_request(&fresh_request, 10).unwrap().status, "pending");
        assert_eq!(store.pairing_requests_at(10).unwrap().iter().filter(|request| request.status == "pending").count(), 1);

        let boundary_device = identity("boundary-device");
        let boundary_invite = store.create_pairing_invite("https://localhost:47821", &"5".repeat(64), 20).unwrap();
        let boundary_request = PairingRequest::new(&boundary_invite, &boundary_device, "linux").unwrap();
        store.receive_pairing_request(&boundary_request, 11).unwrap();
        assert!(matches!(store.approve_pairing(&boundary_request.request_id, 20), Err(SyncError::Conflict(message)) if message.contains("approvable")));
    }

    #[test]
    fn outbox_fifo_requires_one_scheduler_owner_and_never_skips_earlier_backoff() {
        let device = identity("outbox-device");
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let stream_id = "stream-outbox";
        let first_package = package("outbox-device", "k1", NetworkCounts { in_tok: Some(1.0), ..NetworkCounts::all_zero() }, vec![]);
        let second_package = package("outbox-device", "k2", NetworkCounts { in_tok: Some(2.0), ..NetworkCounts::all_zero() }, vec![]);
        let (first_id, first_upload) = store.enqueue_outbox_auto(&device.device_id, &device.origin, stream_id, 1, first_package.manifest_bytes.clone(), first_package.gzip_bytes.clone(), &device.signing_key, &[], 0).unwrap();
        let (second_id, second_upload) = store.enqueue_outbox_auto(&device.device_id, &device.origin, stream_id, 1, second_package.manifest_bytes.clone(), second_package.gzip_bytes.clone(), &device.signing_key, &[], 0).unwrap();
        assert_eq!(first_upload.envelope.generation, 1);
        assert_eq!(second_upload.envelope.generation, 2);
        assert!(store.acquire_scheduler_lease("scheduler-a", 0, 10_000).unwrap());
        assert!(!store.acquire_scheduler_lease("scheduler-b", 1, 10_000).unwrap());
        assert!(matches!(store.claim_outbox("scheduler-b", 1, 10), Err(SyncError::Security(_))));
        let first = store.claim_outbox("scheduler-a", 1, 10).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, first_id);
        assert!(store.claim_outbox("scheduler-a", 1, 10).unwrap().is_empty());
        store.retry_outbox("scheduler-a", first_id, 1, "temporary").unwrap();
        assert!(store.claim_outbox("scheduler-a", 1, 10).unwrap().is_empty());
        assert!(matches!(store.recover_inflight_outbox("scheduler-b", 1), Err(SyncError::Security(_))));

        let mut receipt = ImportReceipt::new("primary-1", &device.device_id, &device.origin, stream_id, 1, 1, hash_bytes(&first[0].manifest_bytes), first[0].gzip_sha256, 1, MergeOutcome { inserted: 1, ..MergeOutcome::default() }, 2);
        assert!(store.ack_outbox("scheduler-a", first_id, &receipt, 2_000).is_ok());
        assert_eq!(store.claim_outbox("scheduler-a", 2_100, 10).unwrap().first().map(|entry| entry.id), Some(second_id));
        receipt.primary_id = "wrong-primary".into();
        receipt.receipt_hash = receipt.compute_hash();
        assert!(matches!(store.ack_outbox("scheduler-a", second_id, &receipt, 2_100), Err(SyncError::Conflict(_))));
        assert_eq!(store.outbox_summary().unwrap().in_flight, 1);
    }

    #[test]
    fn scheduler_lease_rejects_a_second_acquire_even_for_the_same_owner_until_expiry() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        assert!(store.acquire_scheduler_lease("same-owner", 10, 100).unwrap());
        assert!(!store.acquire_scheduler_lease("same-owner", 11, 100).unwrap());
        assert!(!store.acquire_scheduler_lease("other-owner", 11, 100).unwrap());
        assert!(store.acquire_scheduler_lease("other-owner", 110, 100).unwrap());
    }

    #[test]
    fn snapshot_enqueue_replays_without_new_revision_and_keeps_late_history() {
        let device = identity("snapshot-revision");
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let certificate_der = vec![1u8, 2, 3];
        let config = SecondarySyncConfig {
            primary_id: "primary-1".into(), device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: certificate_der.clone(),
            certificate_pin: hash_bytes(&certificate_der), identity_path: PathBuf::from("identity.json"),
            stream_id: Some("snapshot-stream".into()), data_epoch: Some(1), expected_generation: Some(1), enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        };
        store.save_secondary_config(&config, 1).unwrap();
        let first_package = package(&device.origin, "history-1", NetworkCounts { in_tok: Some(1.0), ..NetworkCounts::all_zero() }, vec![]);
        let first = store.enqueue_snapshot_auto(&config, first_package.clone(), &[], &device.signing_key, 2).unwrap().unwrap();
        assert_eq!(first.1.envelope.generation, 1);
        assert!(store.enqueue_snapshot_auto(&config, first_package, &[], &device.signing_key, 3).unwrap().is_none());
        assert_eq!(store.outbox_summary().unwrap().pending, 1);

        let late_package = NetworkPackage::from_rows(&device.origin, INDEX_SCHEMA_VERSION, 0, 10_000, &[
            NetworkRow { key: Some("history-1".into()), occurrence_id: None, tool: "codex".into(), ts_ms: 10, session: "session".into(), project: Some("project".into()), model: Some("model".into()), meter: "tokens".into(), source: "local.jsonl".into(), counts: NetworkCounts { in_tok: Some(1.0), ..NetworkCounts::all_zero() }, calls: vec![] },
            NetworkRow { key: Some("late-history".into()), occurrence_id: None, tool: "codex".into(), ts_ms: 5, session: "session".into(), project: Some("project".into()), model: Some("model".into()), meter: "tokens".into(), source: "local.jsonl".into(), counts: NetworkCounts { out_tok: Some(2.0), ..NetworkCounts::all_zero() }, calls: vec![] },
        ], 5).unwrap();
        let second = store.enqueue_snapshot_auto(&config, late_package, &[], &device.signing_key, 4).unwrap().unwrap();
        assert_eq!(second.1.envelope.generation, 2);
        assert_eq!(store.outbox_summary().unwrap().pending, 2);
    }

    #[test]
    fn ack_clears_only_the_dirty_revision_captured_in_its_journal() {
        let device = identity("dirty-ack");
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let certificate_der = vec![1u8, 2, 3];
        let config = SecondarySyncConfig {
            primary_id: "primary-1".into(), device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: certificate_der.clone(),
            certificate_pin: hash_bytes(&certificate_der), identity_path: PathBuf::from("identity.json"),
            stream_id: Some("dirty-stream".into()), data_epoch: Some(1), expected_generation: Some(1), enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        };
        store.save_secondary_config(&config, 1).unwrap();
        store.index().conn().execute(
            "INSERT INTO meta(key, value) VALUES('device_sync_dirty_revision', '7')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        ).unwrap();
        store.index().conn().execute(
            "INSERT INTO sync_dirty(identity, source_key, occurrence_id, event_json, event_ts_ms, revision, dirty_since_ms, updated_ms)
             VALUES('key:ack-key', 'local.jsonl', NULL, '{}', 10, 7, 1, 1)",
            [],
        ).unwrap();
        let row = NetworkRow {
            key: Some("ack-key".into()), occurrence_id: None, tool: "codex".into(), ts_ms: 10,
            session: "s".into(), project: None, model: None, meter: "tokens".into(), source: "local.jsonl".into(),
            counts: NetworkCounts::all_zero(), calls: vec![],
        };
        let package = NetworkPackage::from_rows(&device.origin, INDEX_SCHEMA_VERSION, 0, 100, &[row.clone()], 10).unwrap();
        let journal = [DirtyJournalInput { identity: "key:ack-key".into(), revision: 7, kind: "event".into(), row }];
        let (id, _) = store.enqueue_snapshot_auto(&config, package, &journal, &device.signing_key, 2).unwrap().unwrap();
        store.index().conn().execute(
            "INSERT OR REPLACE INTO sync_dirty(identity, source_key, occurrence_id, event_json, event_id, event_ts_ms, revision, dirty_since_ms, updated_ms)
             VALUES('key:ack-key', 'local.jsonl', NULL, '{}', NULL, 10, 9, 1, 3)",
            [],
        ).unwrap();
        store.acquire_scheduler_lease("dirty-ack-owner", 2, 10_000).unwrap();
        let entry = store.claim_outbox("dirty-ack-owner", 3, 1).unwrap().pop().unwrap();
        let receipt = ImportReceipt::new(
            "primary-1", &device.device_id, &device.origin, "dirty-stream", 1, 1,
            hash_bytes(&entry.manifest_bytes), entry.gzip_sha256, 1, MergeOutcome::default(), 4,
        );
        store.ack_outbox("dirty-ack-owner", id, &receipt, 4).unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM sync_dirty", [], |r| r.get(0)).unwrap(), 1);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT revision FROM sync_dirty WHERE identity = 'key:ack-key'", [], |r| r.get(0)).unwrap(), 9);
    }

    #[test]
    fn ack_checks_origin_manifest_and_receipt_hash_without_registry_on_sender() {
        let device = identity("sender-only");
        let mut sender = SyncStore::open_in_memory("primary-1").unwrap();
        let package = package("sender-only", "ack-key", NetworkCounts::all_zero(), vec![]);
        let (id, _) = sender.enqueue_outbox_auto(&device.device_id, &device.origin, "ack-stream", 1, package.manifest_bytes, package.gzip_bytes, &device.signing_key, &[], 0).unwrap();
        sender.acquire_scheduler_lease("sender", 0, 1000).unwrap();
        let entry = sender.claim_outbox("sender", 1, 1).unwrap().pop().unwrap();
        let mut receipt = ImportReceipt::new("primary-1", &device.device_id, &device.origin, "ack-stream", 1, 1, hash_bytes(&entry.manifest_bytes), entry.gzip_sha256, 1, MergeOutcome::default(), 2);
        receipt.origin = "other-origin".into();
        receipt.receipt_hash = receipt.compute_hash();
        assert!(matches!(sender.ack_outbox("sender", id, &receipt, 2), Err(SyncError::Conflict(_))));
        assert_eq!(sender.outbox_summary().unwrap().in_flight, 1);
        receipt.origin = device.origin.clone();
        receipt.manifest_hash = [9; 32];
        receipt.receipt_hash = receipt.compute_hash();
        assert!(matches!(sender.ack_outbox("sender", id, &receipt, 2), Err(SyncError::Conflict(_))));
        assert_eq!(sender.outbox_summary().unwrap().in_flight, 1);
        receipt.manifest_hash = hash_bytes(&entry.manifest_bytes);
        receipt.receipt_hash = [7; 32];
        assert!(matches!(sender.ack_outbox("sender", id, &receipt, 2), Err(SyncError::Conflict(_))));
        assert_eq!(sender.outbox_summary().unwrap().in_flight, 1);
    }

    #[test]
    fn numeric_sums_are_finite_and_rollup_overflow_rolls_back() {
        let row_total_overflow = NetworkCounts { in_tok: Some(f64::MAX), cc_tok: Some(0.0), cr_tok: Some(0.0), out_tok: Some(f64::MAX), reason_tok: Some(0.0), credits: Some(0.0) };
        let single = NetworkRow { key: Some("single".into()), occurrence_id: None, tool: "codex".into(), ts_ms: 10, session: "session".into(), project: None, model: None, meter: "tokens".into(), source: "local.jsonl".into(), counts: row_total_overflow, calls: vec![] };
        assert!(matches!(NetworkPackage::from_rows("laptop", INDEX_SCHEMA_VERSION, 0, 10_000, &[single], 1), Err(SyncError::Limit(_))));
        assert!(matches!(NetworkPackage::from_rows("laptop", INDEX_SCHEMA_VERSION, 0, 10_000, &[
            NetworkRow { key: Some("a".into()), occurrence_id: None, tool: "codex".into(), ts_ms: 10, session: "session".into(), project: None, model: None, meter: "tokens".into(), source: "local.jsonl".into(), counts: NetworkCounts { in_tok: Some(f64::MAX), cc_tok: Some(f64::MAX), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }, calls: vec![] },
            NetworkRow { key: Some("b".into()), occurrence_id: None, tool: "codex".into(), ts_ms: 10, session: "session".into(), project: None, model: None, meter: "tokens".into(), source: "local.jsonl".into(), counts: NetworkCounts { in_tok: Some(f64::MAX), cc_tok: Some(f64::MAX), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }, calls: vec![] },
        ], 1), Err(SyncError::Limit(_))));

        let (mut store, device, stream) = paired_store();
        let one = package("laptop", "huge-1", NetworkCounts { in_tok: Some(f64::MAX), cc_tok: Some(0.0), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }, vec![]);
        store.import_network_strict(&upload(&device, &stream, one, 1), 20).unwrap();
        let second = package("laptop", "huge-2", NetworkCounts { in_tok: Some(f64::MAX), cc_tok: Some(0.0), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }, vec![]);
        assert!(matches!(store.import_network_strict(&upload(&device, &stream, second, 2), 21), Err(SyncError::Limit(_))));
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM event", [], |r| r.get(0)).unwrap(), 1);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM device_receipt", [], |r| r.get(0)).unwrap(), 1);
    }

    #[test]
    fn migration_is_additive_and_does_not_clear_existing_event() {
        let index = Index::open_in_memory().unwrap();
        index.conn().execute("INSERT INTO event(tool, ts_ms, session, meter, in_tok, source) VALUES('local', 1, 's', 'tokens', 1, 'local')", []).unwrap();
        let store = SyncStore::new(index, "primary-1").unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM event", [], |r| r.get(0)).unwrap(), 1);
        assert_eq!(store.index().conn().query_row::<String, _, _>("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).unwrap(), INDEX_SCHEMA_VERSION);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'device_sync_vector_candidate'", [], |r| r.get(0)).unwrap(), 0);
        assert_eq!(store.index().conn().query_row::<String, _, _>("SELECT value FROM device_sync_meta WHERE key='migration_version'", [], |r| r.get(0)).unwrap(), STATE_VERSION.to_string());
    }

    #[test]
    fn secondary_last_connected_is_added_to_an_old_table_and_persisted() {
        let index = Index::open_in_memory().unwrap();
        let certificate = vec![1u8, 2, 3];
        let pin = hash_bytes(&certificate);
        index.conn().execute_batch(
            "CREATE TABLE device_sync_secondary_config(
                 config_key TEXT PRIMARY KEY, primary_id TEXT NOT NULL, device_id TEXT NOT NULL,
                 origin TEXT NOT NULL, endpoint TEXT NOT NULL, server_name TEXT NOT NULL,
                 certificate_der BLOB NOT NULL, certificate_pin BLOB NOT NULL, identity_path TEXT NOT NULL,
                 stream_id TEXT, data_epoch INTEGER, expected_generation INTEGER, enabled INTEGER NOT NULL,
                 last_error TEXT, last_manifest_hash BLOB, last_gzip_hash BLOB, updated_at_ms INTEGER NOT NULL
             );",
        ).unwrap();
        index.conn().execute(
            "INSERT INTO device_sync_secondary_config(
                 config_key, primary_id, device_id, origin, endpoint, server_name,
                 certificate_der, certificate_pin, identity_path, enabled, updated_at_ms
             ) VALUES('secondary', 'primary-1', 'device-1', 'laptop', 'https://localhost:1',
                      'localhost', x'010203', ?1, 'identity.json', 1, 1)",
            [pin.as_slice()],
        ).unwrap();
        let mut store = SyncStore::new(index, "primary-1").unwrap();
        let migrated = store.secondary_config().unwrap().unwrap();
        assert_eq!(migrated.last_connected_at_ms, None);
        assert!(migrated.capture_enabled, "old secondary configs default to capture-on");
        assert_eq!(store.index().conn().query_row::<i64, _, _>(
            "SELECT capture_enabled FROM device_sync_secondary_config WHERE config_key = 'secondary'", [], |r| r.get(0),
        ).unwrap(), 1);
        store.mark_secondary_connected(42).unwrap();
        assert_eq!(store.secondary_config().unwrap().unwrap().last_connected_at_ms, Some(42));
    }

    #[test]
    fn secondary_capture_config_round_trips_and_network_pause_keeps_dirty_capture() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("capture.jsonl");
        std::fs::write(&source, b"fixture\n").unwrap();
        let device = identity("capture-pause");
        let certificate_der = vec![1u8, 2, 3];
        let config = SecondarySyncConfig {
            primary_id: "primary-1".into(), device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: certificate_der.clone(),
            certificate_pin: hash_bytes(&certificate_der), identity_path: PathBuf::from("identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        };
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        store.save_secondary_config(&config, 1).unwrap();
        assert_eq!(store.secondary_config().unwrap(), Some(config));

        store.set_secondary_enabled(false, 2).unwrap();
        let paused = store.secondary_config().unwrap().unwrap();
        assert!(!paused.enabled);
        assert!(paused.capture_enabled);
        let report = store.index_mut().ingest_adapter(&CaptureFixture { path: source }, &DateFilter::default()).unwrap();
        assert_eq!(report.new_events, 1);
        assert_eq!(store.index().sync_dirty_events().unwrap().len(), 1, "network pause must not pause local capture");

        store.set_secondary_capture_enabled(false, 3).unwrap();
        let capture_off = store.secondary_config().unwrap().unwrap();
        assert!(!capture_off.enabled);
        assert!(!capture_off.capture_enabled);
        store.set_secondary_enabled(true, 4).unwrap();
        let resumed = store.secondary_config().unwrap().unwrap();
        assert!(resumed.enabled);
        assert!(!resumed.capture_enabled);
    }

    #[test]
    fn clearing_secondary_config_keeps_local_index_history() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let config = SecondarySyncConfig {
            primary_id: "primary-1".into(), device_id: "device-1".into(), origin: "laptop".into(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: vec![1, 2, 3],
            certificate_pin: hash_bytes(&[1, 2, 3]), identity_path: PathBuf::from("identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        };
        store.save_secondary_config(&config, 1).unwrap();
        let identity_path = store.clear_secondary_config(2).unwrap();
        assert_eq!(identity_path, Some(PathBuf::from("identity.json")));
        assert!(store.secondary_config().unwrap().is_none());
        assert_eq!(store.index().event_count().unwrap(), 0);
        assert_eq!(store.index().conn().query_row::<String, _, _>(
            "SELECT value FROM meta WHERE key = 'device_sync_last_unpaired_at_ms'", [], |row| row.get(0),
        ).unwrap(), "2");
    }

    #[test]
    fn failed_import_rolls_back_event_and_receipt() {
        let (mut store, device, stream) = paired_store();
        let mut pkg = package("laptop", "k1", NetworkCounts::all_zero(), vec![]);
        let mut manifest: NetworkManifest = serde_json::from_slice(&pkg.manifest_bytes).unwrap();
        manifest.sums = NetworkSums { in_tok: 99.0, ..manifest.sums };
        pkg.manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let envelope = UploadEnvelope { protocol_id: PROTOCOL_ID.into(), primary_id: stream.primary_id.clone(), device_id: device.device_id.clone(), origin: device.origin.clone(), stream_id: stream.stream_id.clone(), data_epoch: stream.data_epoch, generation: 1, manifest_bytes: pkg.manifest_bytes.clone(), gzip_sha256: hash_bytes(&pkg.gzip_bytes) };
        let signed = SignedUpload::sign(envelope, &device.signing_key, pkg.gzip_bytes).unwrap();
        assert!(store.import_network_strict(&signed, 20).is_err());
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM event", [], |r| r.get(0)).unwrap(), 0);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM device_receipt", [], |r| r.get(0)).unwrap(), 0);
    }
}

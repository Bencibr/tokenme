use std::path::PathBuf;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use usage_core::{origin_of, origin_ok};
use usage_index::{Index, ORIGIN_SQL};

use crate::protocol::{
    hash_bytes, public_key_from_bytes, valid_text,
    AuthenticatedDisplayNameRequest, PairingInvite, PairingRequest,
};

const STATE_VERSION: i64 = 7;
const MAX_PENDING_PAIRING_REQUESTS: i64 = 32;
const NOTICE_BACKOFF_MAX_MS: i64 = 15 * 60 * 1000;
const DISCONNECT_NOTICE_PREFIX: &str = "device_sync_disconnect:";
const MAX_DISCONNECT_NOTICES: i64 = 128;
/// Per-device index-meta keys of the rollup uplink: the one-time migration
/// gate (value: the origin whose row-merged era was retired) and the ack
/// stamp the panel's upload status reads. Both are written by
/// [`SyncStore::apply_uplink_rollup`] in the same transaction that stores
/// the snapshot rows.
const UPLINK_ROLLUP_MIGRATED_PREFIX: &str = "device_sync_uplink_rollup_migrated:";
const UPLINK_ROLLUP_ACKED_PREFIX: &str = "device_sync_uplink_rollup_acked_at_ms:";
/// Index-meta mirror of "a secondary config exists and enabled = 1",
/// maintained by `write_capture_mirror` beside the capture switch: while it
/// reads '1', `Index::prune` must not age this machine's local-origin
/// history out — the uplink snapshot re-ships it as the shared truth.
const UPLINK_PAIRED_MIRROR: &str = "device_sync_uplink_paired";
/// Sibling file that carries device configuration/registry state so UI
/// operations never have to open (or lock) the usage index.
const SYNC_STATE_DB_NAME: &str = "sync_state.db";

/// Resolves the dedicated device-state database path next to an index path.
/// The state file is destructive (no migration from records previously stored
/// inside the index): a missing file means fresh state.
fn sync_state_path(index_path: &std::path::Path) -> SyncResult<PathBuf> {
    let parent = index_path.parent().filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| SyncError::Invalid(format!("index path has no parent directory: {}", index_path.display())))?;
    let state = parent.join(SYNC_STATE_DB_NAME);
    if std::fs::symlink_metadata(&state).map(|meta| meta.file_type().is_symlink()).unwrap_or(false) {
        return Err(SyncError::Security(format!("sync state is a symlink: {}", state.display())));
    }
    Ok(state)
}


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
    pub last_merged_at_ms: Option<i64>,
    pub snapshot_complete: bool,
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
    pub last_merged_at_ms: Option<i64>,
    pub snapshot_complete: bool,
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
            last_merged_at_ms: record.last_merged_at_ms,
            snapshot_complete: record.snapshot_complete,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_display_name: Option<String>,
    /// The primary's OS as it advertised itself during pairing. Drives the
    /// platform icon on the secondary's device list; absent on pairings made
    /// before the field travelled, and then the UI falls back to a generic
    /// device glyph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_os: Option<String>,
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

/// Public trust material and a reference to a private identity file. A local
/// disconnect commits before the network notification; no private key is copied
/// into SQLite, and later role changes do not erase this retry record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisconnectNotice {
    pub config: SecondarySyncConfig,
    pub attempts: u32,
    pub next_attempt_ms: i64,
    pub last_error: Option<String>,
}

/// Persisted primary listener settings. `enabled` is the user's last start
/// choice; stopping the listener only flips that bit and leaves sync history
/// and trust state untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimarySyncConfig {
    pub primary_id: String,
    pub bind_addr: String,
    pub advertise_host: Option<String>,
    pub allow_lan: bool,
    pub enabled: bool,
    #[serde(default, alias = "allow_secondary_read")]
    pub share_data_with_secondaries: bool,
    /// Auto-approve incoming pairing requests instead of queueing them for a
    /// manual tap. Off by default: a primary that turns it on is choosing
    /// convenience over the invite-only trust review.
    #[serde(default)]
    pub auto_approve_pairing: bool,
}

impl PrimarySyncConfig {
    fn validate(&self, bound_primary: &str) -> SyncResult<()> {
        if self.primary_id != bound_primary {
            return Err(SyncError::Security("primary config targets another primary".into()));
        }
        validate_identifier("primary_id", &self.primary_id)?;
        if self.bind_addr.parse::<std::net::SocketAddr>().is_err() {
            return Err(SyncError::Invalid("primary bind address is invalid".into()));
        }
        if self.advertise_host.as_deref().is_some_and(|host| host.is_empty() || host.len() > 16 * 1024 || host.chars().any(char::is_control)) {
            return Err(SyncError::Invalid("primary advertised host is invalid".into()));
        }
        Ok(())
    }
}

impl SecondarySyncConfig {
    fn validate(&self, bound_primary: &str) -> SyncResult<()> {
        if self.primary_id != bound_primary {
            return Err(SyncError::Security("secondary config targets another primary".into()));
        }
        if let Some(name) = self.primary_display_name.as_deref() { valid_text("primary_display_name", name, false)?; }
        if let Some(os) = self.primary_os.as_deref() { valid_text("primary_os", os, false)?; }
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

/// What `reset_secondary_sync_state` discarded. Local events are never part of
/// the reset: the next pass re-ships the whole local rollup snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryResetSummary {
    pub device_id: String,
}

/// Everything the panel's five-second status poll reads, gathered through one
/// state connection and one index connection instead of one each per field.
#[derive(Debug, Clone, Default)]
pub struct DeviceSyncRead {
    pub secondary_config: Option<SecondarySyncConfig>,
    pub primary_config: Option<PrimarySyncConfig>,
    pub status: DeviceSyncSnapshot,
    pub disconnect_requested: bool,
    pub upload_complete: Option<bool>,
    pub download_complete: Option<bool>,
}

pub struct SyncStore {
    index: Index,
    /// Device configuration and registry state lives outside the usage index.
    /// This connection is deliberately independent: panel config/status reads
    /// must not open or lock the (potentially very large) event database.
    /// `pub(crate)` so same-crate integration points (and their fixtures) can
    /// reach the dedicated state database; external callers go through the
    /// public accessors.
    pub(crate) state: Connection,
    primary_id: String,
}

/// Opens a read-only SQLite connection with a short busy timeout. Read-only
/// probes on `sync_state.db` or the usage index must not fail instantly with
/// `DatabaseBusy` just because a concurrent writer holds the lock for a couple
/// of milliseconds.
fn open_read_only(path: impl AsRef<std::path::Path>) -> SyncResult<Connection> {
    let conn = Connection::open_with_flags(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sql_err)?;
    conn.busy_timeout(std::time::Duration::from_millis(250)).map_err(sql_err)?;
    Ok(conn)
}

impl SyncStore {
    pub fn new(index: Index, primary_id: impl Into<String>) -> SyncResult<Self> {
        Self::new_with_state(index, Connection::open_in_memory().map_err(sql_err)?, primary_id)
    }

    fn new_with_state(mut index: Index, mut state: Connection, primary_id: impl Into<String>) -> SyncResult<Self> {
        let primary_id = primary_id.into();
        validate_identifier("primary_id", &primary_id)?;
        index.mark_device_sync_state().map_err(SyncError::from)?;
        // The marker is durable before any sync table/receipt mutation.  A
        // crash during the first migration must make the normal Index opener
        // preserve the database rather than quarantine it as disposable cache.
        migrate(&mut index)?;
        migrate_state(&mut state, &primary_id)?;
        let store = Self { index, state, primary_id };
        store.write_capture_mirror()?;
        Ok(store)
    }

    /// `open` minus the index-side maintenance writes (journal release,
    /// capture mirror refresh). For paths that must answer at UI speed while
    /// the ingest engine holds long write transactions on the usage index;
    /// `maintain` converges the skipped work once the listener is up.
    pub fn open_deferred(path: impl Into<PathBuf>, primary_id: impl Into<String>) -> SyncResult<Self> {
        let path = path.into();
        let mut index = Index::open_preserving_corruption(&path).map_err(SyncError::from)?;
        let state_path = sync_state_path(&path)?;
        let mut state = Connection::open(&state_path).map_err(sql_err)?;
        state.busy_timeout(std::time::Duration::from_secs(5)).map_err(sql_err)?;
        let primary_id = primary_id.into();
        validate_identifier("primary_id", &primary_id)?;
        index.mark_device_sync_state().map_err(SyncError::from)?;
        migrate(&mut index)?;
        migrate_state(&mut state, &primary_id)?;
        Ok(Self { index, state, primary_id })
    }

    /// The maintenance `open` performs eagerly: refresh the ingest-time capture
    /// mirror. `open_deferred` callers run this in the background.
    pub fn maintain(&mut self) -> SyncResult<()> {
        self.write_capture_mirror()
    }

    /// Recomputes the ingest-time capture switch and the uplink prune pin
    /// from the state database and mirrors both into the index `meta` table
    /// so `Index` ingest/prune never have to open the state file. Runs after
    /// open and after every config/pairing write that can change either
    /// answer.
    fn write_capture_mirror(&self) -> SyncResult<()> {
        let enabled = sync_capture_enabled_state(&self.state)?;
        self.index.conn().execute(
            "INSERT INTO meta(key, value) VALUES('device_sync_capture_enabled', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![if enabled { "1".to_string() } else { "0".to_string() }],
        ).map_err(sql_err)?;
        // The prune pin's half: while a secondary config is enabled, this
        // machine's own history is the shared truth the uplink snapshot
        // re-ships every pass, so `Index::prune` must not age it out (see
        // `Index::prune`). Unpaired machines write '0' — or nothing, if this
        // index was never opened through a SyncStore — and prune normally.
        let paired = sync_uplink_paired_state(&self.state)?;
        self.index.conn().execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![UPLINK_PAIRED_MIRROR, if paired { "1".to_string() } else { "0".to_string() }],
        ).map_err(sql_err)?;
        Ok(())
    }

    pub fn open(path: impl Into<PathBuf>, primary_id: impl Into<String>) -> SyncResult<Self> {
        let path = path.into();
        let index = Index::open_preserving_corruption(&path).map_err(SyncError::from)?;
        let state_path = sync_state_path(&path)?;
        let state = Connection::open(&state_path).map_err(sql_err)?;
        state.busy_timeout(std::time::Duration::from_secs(5)).map_err(sql_err)?;
        Self::new_with_state(index, state, primary_id)
    }

    pub fn open_in_memory(primary_id: impl Into<String>) -> SyncResult<Self> {
        Self::new(Index::open_in_memory().map_err(SyncError::from)?, primary_id)
    }

    /// Path of the destructive, non-migrating device state database.
    pub fn state_path(path: impl AsRef<std::path::Path>) -> SyncResult<PathBuf> {
        sync_state_path(path.as_ref())
    }

    /// Immediate-mode transaction on the dedicated state connection. Borrowed
    /// as an associated fn so closures reading other `self` fields (e.g.
    /// `primary_id`) stay disjoint-field borrows.
    fn state_tx<T, F>(state: &mut Connection, f: F) -> SyncResult<T>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> SyncResult<T>,
    {
        let tx = state.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql_err)?;
        match f(&tx) {
            Ok(value) => tx.commit().map(|()| value).map_err(sql_err),
            Err(error) => { let _ = tx.rollback(); Err(error) }
        }
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

    /// Status of a registered device for WebSocket connection gating.
    pub fn ws_device_status(&self, device_id: &str) -> SyncResult<Option<String>> {
        let status: Option<String> = self.state.query_row(
            "SELECT status FROM device_registry WHERE device_id = ?1",
            params![device_id],
            |row| row.get(0),
        ).optional().map_err(sql_err)?;
        Ok(status)
    }

    /// Data epoch of a registered device.
    pub fn ws_device_epoch(&self, device_id: &str) -> SyncResult<Option<i64>> {
        let epoch: Option<i64> = self.state.query_row(
            "SELECT data_epoch FROM device_registry WHERE device_id = ?1",
            params![device_id],
            |row| row.get(0),
        ).optional().map_err(sql_err)?;
        Ok(epoch)
    }

    /// Starts a new sync role/primary epoch without deleting local usage
    /// events. Trust, streams, receipts and unsent packages belong to the old
    /// role and must not be reused by the new role.
    pub fn rotate_primary_id(&mut self, new_primary_id: impl Into<String>, now_ms: i64) -> SyncResult<()> {
        let new_primary_id = new_primary_id.into();
        validate_identifier("primary_id", &new_primary_id)?;
        if new_primary_id == self.primary_id { return Ok(()); }
        rotate_state_primary_id(&mut self.state, &new_primary_id)?;
        self.index.with_network_transaction(|tx| {
            rotate_index_sync_state_tx(tx, now_ms)?;
            Ok::<_, SyncError>(())
        })?;
        self.primary_id = new_primary_id;
        self.write_capture_mirror()?;
        Ok(())
    }

    /// Reads the sync state without migrating or writing either database.
    /// Registry/pairing state comes from the dedicated state file; the row-era
    /// per-device stream progress reader is gone with the row-era streams.
    pub fn read_status(path: impl AsRef<std::path::Path>) -> SyncResult<DeviceSyncSnapshot> {
        let index_path = path.as_ref();
        let path = sync_state_path(index_path)?;
        if !path.exists() {
            return Ok(DeviceSyncSnapshot::default());
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| SyncError::Io(path.to_path_buf(), error))?;
        if metadata.file_type().is_symlink() {
            return Err(SyncError::Security(format!("sync state is a symlink: {}", path.display())));
        }
        let conn = open_read_only(&path)?;
        conn.busy_timeout(std::time::Duration::from_millis(250)).map_err(sql_err)?;
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
        let index = if index_path.exists() {
            open_read_only(index_path)?
        } else {
            Connection::open_in_memory().map_err(sql_err)?
        };
        read_status_conn(&conn, &index)
    }

    /// The poll-shaped read: both role configs, registry status, disconnect
    /// flag and up/down completion stamps through one state connection and one
    /// index connection. The panel asks for all of these every five seconds;
    /// opening each through its own connection made a single poll queue behind
    /// the ingest writer eight to ten times for the same answer.
    pub fn read_panel_state(path: impl AsRef<std::path::Path>) -> SyncResult<DeviceSyncRead> {
        let index_path = path.as_ref();
        let state_path = sync_state_path(index_path)?;
        let mut read = DeviceSyncRead::default();
        let mut state = None;
        if state_path.exists() {
            let metadata = std::fs::symlink_metadata(&state_path).map_err(|error| SyncError::Io(state_path.clone(), error))?;
            if metadata.file_type().is_symlink() {
                return Err(SyncError::Security(format!("sync state is a symlink: {}", state_path.display())));
            }
            let conn = open_read_only(&state_path)?;
            conn.busy_timeout(std::time::Duration::from_millis(250)).map_err(sql_err)?;
            let has_state: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'device_sync_meta')",
                    [],
                    |row| row.get(0),
                )
                .map_err(sql_err)?;
            if has_state {
                read.secondary_config = read_secondary_config_conn(&conn)?;
                read.primary_config = read_primary_config_conn(&conn)?;
            }
            state = Some((conn, has_state));
        }
        let index = if index_path.exists() {
            open_read_only(index_path)?
        } else {
            Connection::open_in_memory().map_err(sql_err)?
        };
        if let Some((conn, true)) = state.as_ref() {
            read.status = read_status_conn(conn, &index)?;
        }
        if let Some(config) = read.secondary_config.as_ref() {
            read.disconnect_requested = disconnect_requested_conn(&index, &config.primary_id)?;
            read.upload_complete = upload_cursor_complete_conn(&index, config);
            read.download_complete = download_cursor_complete_conn(&index, &config.primary_id);
        }
        Ok(read)
    }

    /// Read the persisted secondary configuration without migrating or
    /// acquiring a writer connection. This is safe for the panel's polling
    /// path and lets an older process remain read-only when this table exists.
    pub fn read_secondary_config(path: impl AsRef<std::path::Path>) -> SyncResult<Option<SecondarySyncConfig>> {
        let path = sync_state_path(path.as_ref())?;
        if !path.exists() { return Ok(None); }
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| SyncError::Io(path.to_path_buf(), error))?;
        if metadata.file_type().is_symlink() {
            return Err(SyncError::Security(format!("sync state is a symlink: {}", path.display())));
        }
        let conn = open_read_only(&path)?;
        read_secondary_config_conn(&conn)
    }

    /// Read the persisted primary listener settings without migrating or
    /// taking a writer connection. This is the restart recovery gate used by
    /// the panel.
    pub fn read_primary_config(path: impl AsRef<std::path::Path>) -> SyncResult<Option<PrimarySyncConfig>> {
        let path = sync_state_path(path.as_ref())?;
        if !path.exists() { return Ok(None); }
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| SyncError::Io(path.to_path_buf(), error))?;
        if metadata.file_type().is_symlink() {
            return Err(SyncError::Security(format!("sync state is a symlink: {}", path.display())));
        }
        let conn = open_read_only(&path)?;
        read_primary_config_conn(&conn)
    }

    /// Read-only upload completion for the panel's status bridge, mirroring
    /// `read_status`: never migrate, never take the writer lock. `None` means
    /// the pairing has not produced an answer yet; `Some(true)` means the
    /// primary has stored this device's rollup snapshot (the ack stamp of
    /// `apply_uplink_rollup`).
    pub fn read_upload_cursor_complete(path: impl AsRef<std::path::Path>, config: &SecondarySyncConfig) -> Option<bool> {
        // The ack stamp lives in the index: it commits atomically with the
        // rows it covers. This stays a read-only open.
        let conn = open_read_only(path.as_ref()).ok()?;
        upload_cursor_complete_conn(&conn, config)
    }

    /// The origins whose remote rows must not count in the figures — the
    /// disconnect side of the fold-time exclusion (see
    /// `ReportOptions::exclude_origins`), keyed on exact origins so the
    /// collector's separate `sync:linux:<origin>` import records keep
    /// counting whatever happens to a pairing.
    ///
    /// Primary side: the origins of revoked devices in the registry. A PAUSED
    /// device deliberately keeps counting — a pause is reversible ("offline,
    /// not unsubscribed"); only revocation ends the sharing promise, so the
    /// exclusion keys on `revoked` and nothing else.
    ///
    /// Secondary side: the origin its merged downlink rows carry (recorded by
    /// [`Self::queue_secondary_disconnect`] when the pairing ends),
    /// detached unless a live, enabled pairing says otherwise. The runtime's
    /// self-disable on a revoked pairing (`remember_secondary_error` →
    /// `set_secondary_enabled(false)`) lands here through that same bit — no
    /// further runtime write is needed to detach a revoked secondary.
    ///
    /// Restore needs no bookkeeping either: `transition_to_secondary` and
    /// `save_secondary_config` deliberately leave the meta key alone; an
    /// enabled config is what un-detaches the origin.
    ///
    /// Shaped like [`Self::read_panel_state`]: read-only opens, no writes,
    /// tolerant of missing tables and files — any unreadable piece means
    /// "nothing detached", which errs toward the pre-detach status quo.
    pub fn read_detached_origins(path: impl AsRef<std::path::Path>) -> Vec<String> {
        let mut detached: Vec<String> = Vec::new();
        let state_path = match sync_state_path(path.as_ref()) {
            Ok(state_path) => state_path,
            Err(_) => return detached,
        };
        if !state_path.exists() { return detached; }
        let Ok(state) = open_read_only(&state_path) else { return detached; };
        let has_registry: bool = state
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'device_registry')",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if has_registry {
            if let Ok(mut stmt) = state.prepare("SELECT origin FROM device_registry WHERE status = 'revoked'") {
                if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                    for origin in rows.flatten() {
                        if !origin.is_empty() && !detached.contains(&origin) { detached.push(origin); }
                    }
                }
            }
        }
        // A live, enabled pairing is what keeps the downlink origin counting:
        // no config row, or one with enabled = 0, detaches it. An unreadable
        // config errs the same way the rest of this function does.
        let live = match read_secondary_config_conn(&state) {
            Ok(Some(config)) => config.enabled,
            Ok(None) => false,
            Err(_) => true,
        };
        if live { return detached; }
        if let Ok(index) = open_read_only(path.as_ref()) {
            let has_meta: bool = index
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if has_meta {
                if let Ok(origin) = index.query_row(
                    "SELECT value FROM meta WHERE key = 'device_sync_downlink_origin'",
                    [],
                    |row| row.get::<_, String>(0),
                ) {
                    if !origin.is_empty() && !detached.contains(&origin) { detached.push(origin); }
                }
            }
        }
        detached
    }

    pub fn devices(&self) -> SyncResult<Vec<DeviceRecord>> {
        let mut stmt = self.state.prepare(
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
        device_tx(&self.state, self.index.conn(), device_id)
    }

    pub fn secondary_config(&self) -> SyncResult<Option<SecondarySyncConfig>> {
        read_secondary_config_conn(&self.state)
    }

    pub fn primary_config(&self) -> SyncResult<Option<PrimarySyncConfig>> {
        read_primary_config_conn(&self.state)
    }

    pub fn save_primary_config(&mut self, config: &PrimarySyncConfig, now_ms: i64) -> SyncResult<()> {
        config.validate(&self.primary_id)?;
        Self::state_tx(&mut self.state, |tx| upsert_primary_config_tx(tx, config, now_ms))
    }

    pub fn set_primary_enabled(&mut self, enabled: bool, now_ms: i64) -> SyncResult<()> {
        Self::state_tx(&mut self.state, |tx| {
            let changed = tx.execute(
                "UPDATE device_sync_primary_config
                    SET enabled = ?1, updated_at_ms = ?2
                  WHERE config_key = 'primary' AND primary_id = ?3",
                params![if enabled { 1_i64 } else { 0_i64 }, now_ms, self.primary_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("primary sync configuration".into())); }
            Ok(())
        })
    }

    pub fn set_primary_data_sharing(&mut self, enabled: bool, now_ms: i64) -> SyncResult<()> {
        Self::state_tx(&mut self.state, |tx| {
            let changed = tx.execute(
                "UPDATE device_sync_primary_config
                    SET share_data_with_secondaries = ?1, updated_at_ms = ?2
                  WHERE config_key = 'primary' AND primary_id = ?3",
                params![if enabled { 1_i64 } else { 0_i64 }, now_ms, self.primary_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("primary sync configuration".into())); }
            Ok(())
        })
    }

    pub fn set_primary_auto_approve(&mut self, enabled: bool, now_ms: i64) -> SyncResult<()> {
        Self::state_tx(&mut self.state, |tx| {
            let changed = tx.execute(
                "UPDATE device_sync_primary_config
                    SET auto_approve_pairing = ?1, updated_at_ms = ?2
                  WHERE config_key = 'primary' AND primary_id = ?3",
                params![if enabled { 1_i64 } else { 0_i64 }, now_ms, self.primary_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("primary sync configuration".into())); }
            Ok(())
        })
    }

    /// State-only write for the panel's share toggle: opens just the dedicated
    /// state database, never the usage index, so the toggle lands in
    /// milliseconds even while the ingest engine holds the index write lock.
    /// Sharing does not feed `sync_capture_enabled_state`, so the capture
    /// mirror on the index needs no refresh here.
    pub fn write_primary_data_sharing(
        path: impl AsRef<std::path::Path>,
        primary_id: &str,
        enabled: bool,
        now_ms: i64,
    ) -> SyncResult<()> {
        validate_identifier("primary_id", primary_id)?;
        let state_path = sync_state_path(path.as_ref())?;
        let mut state = Connection::open(&state_path).map_err(sql_err)?;
        state.busy_timeout(std::time::Duration::from_millis(500)).map_err(sql_err)?;
        migrate_state(&mut state, primary_id)?;
        Self::state_tx(&mut state, |tx| {
            let changed = tx.execute(
                "UPDATE device_sync_primary_config
                    SET share_data_with_secondaries = ?1, updated_at_ms = ?2
                  WHERE config_key = 'primary' AND primary_id = ?3",
                params![if enabled { 1_i64 } else { 0_i64 }, now_ms, primary_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("primary sync configuration".into())); }
            Ok(())
        })
    }

    /// State-only twin of `set_primary_auto_approve`: the panel's toggle must
    /// never queue behind the ingest engine's index write lock, and the flag
    /// lives in sync_state.db alone.
    pub fn write_primary_auto_approve(
        path: impl AsRef<std::path::Path>,
        primary_id: &str,
        enabled: bool,
        now_ms: i64,
    ) -> SyncResult<()> {
        validate_identifier("primary_id", primary_id)?;
        let state_path = sync_state_path(path.as_ref())?;
        let mut state = Connection::open(&state_path).map_err(sql_err)?;
        state.busy_timeout(std::time::Duration::from_millis(500)).map_err(sql_err)?;
        migrate_state(&mut state, primary_id)?;
        Self::state_tx(&mut state, |tx| {
            let changed = tx.execute(
                "UPDATE device_sync_primary_config
                    SET auto_approve_pairing = ?1, updated_at_ms = ?2
                  WHERE config_key = 'primary' AND primary_id = ?3",
                params![if enabled { 1_i64 } else { 0_i64 }, now_ms, primary_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("primary sync configuration".into())); }
            Ok(())
        })
    }

    /// State-only persist for the primary start path: the role is recorded in
    /// sync_state.db without opening the usage index, so the start cannot wait
    /// out an ingest write transaction just to write its own config row. The
    /// capture mirror refresh this upsert would imply is the background
    /// `maintain` pass's concern.
    pub fn write_primary_config_state(
        path: impl AsRef<std::path::Path>,
        primary_id: &str,
        config: &PrimarySyncConfig,
        now_ms: i64,
    ) -> SyncResult<()> {
        validate_identifier("primary_id", primary_id)?;
        config.validate(primary_id)?;
        let state_path = sync_state_path(path.as_ref())?;
        let mut state = Connection::open(&state_path).map_err(sql_err)?;
        state.busy_timeout(std::time::Duration::from_millis(500)).map_err(sql_err)?;
        migrate_state(&mut state, primary_id)?;
        Self::state_tx(&mut state, |tx| upsert_primary_config_tx(tx, config, now_ms))
    }

    /// State-only teardown for the secondary's stop button: deletes the
    /// binding row from sync_state.db without ever opening the usage index,
    /// so the stop cannot queue behind the ingest engine's write lock. The
    /// index-side bookkeeping (disconnect notice, aggregate detach, capture
    /// mirror) stays with `queue_secondary_disconnect`, which the caller runs
    /// on a background thread with the captured config.
    pub fn clear_secondary_config_state(
        path: impl AsRef<std::path::Path>,
        primary_id: &str,
        device_id: &str,
    ) -> SyncResult<()> {
        validate_identifier("primary_id", primary_id)?;
        validate_identifier("device_id", device_id)?;
        let state_path = sync_state_path(path.as_ref())?;
        let mut state = Connection::open(&state_path).map_err(sql_err)?;
        state.busy_timeout(std::time::Duration::from_millis(500)).map_err(sql_err)?;
        migrate_state(&mut state, primary_id)?;
        Self::state_tx(&mut state, |tx| {
            tx.execute(
                "DELETE FROM device_sync_secondary_config WHERE config_key = 'secondary' AND device_id = ?1",
                params![device_id],
            ).map_err(sql_err)?;
            Ok(())
        })
    }

    /// Removes the primary role configuration while retaining all usage,
    /// device history, receipts, and dirty/outbox state. Role switching uses
    /// this after the listener has been stopped so a later restart cannot
    /// resurrect the old primary role.
    pub fn clear_primary_config(&mut self, now_ms: i64) -> SyncResult<()> {
        Self::state_tx(&mut self.state, |tx| {
            tx.execute(
                "DELETE FROM device_sync_primary_config
                  WHERE config_key = 'primary' AND primary_id = ?1",
                params![self.primary_id],
            ).map_err(sql_err)?;
            Ok(())
        })?;
        self.index.with_network_transaction(|tx| {
            tx.execute(
                "INSERT INTO meta(key, value) VALUES('device_sync_last_primary_unpaired_at_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![now_ms.to_string()],
            ).map_err(sql_err)?;
            Ok::<_, SyncError>(())
        })?;
        self.write_capture_mirror()
    }

    /// State-only teardown for the panel's stop button: deletes the primary
    /// config from sync_state.db without ever opening the usage index, so the
    /// stop cannot queue behind the ingest engine's write lock. The index-side
    /// convergence (unpaired timestamp, capture mirror, journal release) is the
    /// caller's background concern — the next full-store open performs it.
    pub fn clear_primary_config_state(path: impl AsRef<std::path::Path>, primary_id: &str) -> SyncResult<()> {
        validate_identifier("primary_id", primary_id)?;
        let state_path = sync_state_path(path.as_ref())?;
        let mut state = Connection::open(&state_path).map_err(sql_err)?;
        state.busy_timeout(std::time::Duration::from_millis(500)).map_err(sql_err)?;
        Self::state_tx(&mut state, |tx| {
            tx.execute(
                "DELETE FROM device_sync_primary_config
                  WHERE config_key = 'primary' AND primary_id = ?1",
                params![primary_id],
            ).map_err(sql_err)?;
            Ok(())
        })
    }

    pub fn save_secondary_config(&mut self, config: &SecondarySyncConfig, now_ms: i64) -> SyncResult<()> {
        config.validate(&self.primary_id)?;
        self.save_secondary_config_inner(config, now_ms)
    }

    fn save_secondary_config_inner(&mut self, config: &SecondarySyncConfig, now_ms: i64) -> SyncResult<()> {
        Self::state_tx(&mut self.state, |tx| save_secondary_config_tx(tx, config, now_ms))?;
        self.write_capture_mirror()
    }

    /// Commit the new role and its connection together. Invalid configuration
    /// or a failed SQLite write leaves the stopped primary and its trust intact.
    pub fn transition_to_secondary(&mut self, config: &SecondarySyncConfig, now_ms: i64) -> SyncResult<()> {
        config.validate(&config.primary_id)?;
        let rotate = self.primary_id != config.primary_id;
        // One state transaction: the role wipe, the new binding and the old
        // primary's removal commit together, so a failed binding write (the
        // lifecycle tests simulate one) leaves the stopped primary intact.
        Self::state_tx(&mut self.state, |tx| {
            if rotate { rotate_state_primary_id_tx(tx, &config.primary_id)?; }
            save_secondary_config_tx(tx, config, now_ms)?;
            tx.execute("DELETE FROM device_sync_primary_config", []).map_err(sql_err)?;
            Ok(())
        })?;
        if rotate {
            self.index.with_network_transaction(|tx| rotate_index_sync_state_tx(tx, now_ms))?;
        }
        self.primary_id = config.primary_id.clone();
        self.write_capture_mirror()
    }

    pub fn set_secondary_enabled(&mut self, enabled: bool, now_ms: i64) -> SyncResult<()> {
        Self::state_tx(&mut self.state, |tx| {
            let changed = tx.execute(
                "UPDATE device_sync_secondary_config SET enabled = ?1, updated_at_ms = ?2 WHERE config_key = 'secondary' AND primary_id = ?3",
                params![if enabled { 1_i64 } else { 0_i64 }, now_ms, self.primary_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("secondary sync configuration".into())); }
            Ok(())
        })?;
        if enabled {
            self.index.with_network_transaction(|tx| {
                tx.execute(
                    "DELETE FROM meta WHERE key = 'device_sync_disconnect_requested' AND value = ?1",
                    params![self.primary_id],
                ).map_err(sql_err)?;
                Ok::<_, SyncError>(())
            })?;
        }
        self.write_capture_mirror()
    }

    /// Update only the transport address after a pinned discovery probe. Do
    /// not overwrite stream/ACK progress written by the running scheduler.
    pub fn update_secondary_endpoint(&mut self, device_id: &str, previous: &str, endpoint: &str, now_ms: i64) -> SyncResult<bool> {
        Self::state_tx(&mut self.state, |tx| {
            let changed = tx.execute(
                "UPDATE device_sync_secondary_config SET endpoint = ?1, updated_at_ms = ?2
                 WHERE primary_id = ?3 AND device_id = ?4 AND endpoint = ?5 AND enabled = 1",
                params![endpoint, now_ms, self.primary_id, device_id, previous],
            ).map_err(sql_err)?;
            Ok(changed == 1)
        })
    }

    /// Atomically detach locally, retire the old sender's immutable packages,
    /// and retain a bounded notification for background delivery. No network
    /// ACK is required to stop or to pair with another primary.
    pub fn request_secondary_disconnect(&mut self, now_ms: i64) -> SyncResult<()> {
        let Some(config) = self.secondary_config()? else { return Ok(()); };
        self.queue_secondary_disconnect(&config, now_ms)
    }

    /// Applies a fetched rollup snapshot from the current primary as a full
    /// replace for `origin_label`: delete that origin's previous rows, insert
    /// the received ones, stamp the fetch time. On the first apply this also
    /// retires the row-level download era: the remote-origin event rows the
    /// old event download merged into this index (and their rollup rows) are
    /// deleted — the aggregate snapshot replaces them, so figures neither
    /// double count nor lose history. Gated by a meta flag, once per origin.
    pub fn apply_remote_rollup(
        &mut self,
        origin_label: &str,
        rows: &[crate::transport::RollupRow],
        fetched_at_ms: i64,
    ) -> SyncResult<()> {
        validate_identifier("origin_label", origin_label)?;
        self.index.with_network_transaction(|tx| {
            // Per-LABEL gate: the label is the primary's live hostname, and
            // hostnames change — a gate keyed once globally would skip the
            // row-era retirement whenever the label differs from the pinned
            // export origin the old downloads merged under, leaving those
            // rows to double count beside the snapshot.
            let migrated: Option<String> = tx
                .query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    params![format!("device_sync_rollup_migrated:{origin_label}")],
                    |row| row.get(0),
                )
                .optional().map_err(sql_err)?;
            if migrated.is_none() {
                // One-time retirement of the row-level download era, for this
                // label: the old downloads merged under the pinned export
                // origin, which equals this label whenever the label is the
                // one this device has been syncing under.
                tx.execute(
                    &format!(
                        "DELETE FROM call WHERE event_id IN (SELECT id FROM event WHERE ({ORIGIN_SQL}) = ?1)"
                    ),
                    params![origin_label],
                ).map_err(sql_err)?;
                tx.execute(
                    &format!("DELETE FROM event WHERE ({ORIGIN_SQL}) = ?1"),
                    params![origin_label],
                ).map_err(sql_err)?;
                tx.execute(
                    "DELETE FROM event_rollup WHERE origin = ?1",
                    params![origin_label],
                ).map_err(sql_err)?;
                // A renamed primary leaves its old label's snapshot rows
                // behind; this device pairs exactly one primary, so any other
                // label's rows are stale by definition.
                tx.execute(
                    "DELETE FROM remote_rollup WHERE origin != ?1",
                    params![origin_label],
                ).map_err(sql_err)?;
                tx.execute(
                    "INSERT INTO meta(key, value) VALUES(?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![format!("device_sync_rollup_migrated:{origin_label}"), origin_label],
                ).map_err(sql_err)?;
            }
            // The 分家 detach reads this label (`read_detached_origins`): a
            // self-disabled or disconnected pairing detaches it from the
            // figures. The downlink era recorded it from merged event rows;
            // the rollup era has no rows to scan, so the apply is the writer.
            tx.execute(
                "INSERT INTO meta(key, value) VALUES('device_sync_downlink_origin', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![origin_label],
            ).map_err(sql_err)?;
            tx.execute("DELETE FROM remote_rollup WHERE origin = ?1", params![origin_label]).map_err(sql_err)?;
            for row in rows {
                tx.execute(
                    "INSERT OR REPLACE INTO remote_rollup(
                        day, origin, tool, session, project, model, meter,
                        in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts
                     ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                    params![
                        row.day, origin_label, row.tool, row.session, row.project, row.model, row.meter,
                        row.in_tok, row.cc_tok, row.cr_tok, row.out_tok, row.reason_tok, row.credits,
                        row.n, row.nonzero, row.min_ts, row.max_ts,
                    ],
                ).map_err(sql_err)?;
            }
            tx.execute(
                "INSERT INTO meta(key, value) VALUES('device_sync_rollup_fetched_at_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![fetched_at_ms.to_string()],
            ).map_err(sql_err)?;
            Ok::<_, SyncError>(())
        })
    }

    /// Applies a paired device's uplink rollup snapshot under the origin the
    /// REGISTRY assigned it — never a label the wire declares, because the
    /// receiver owns grouping. One index transaction: the one-time
    /// per-device migration gate retires the row-merged uplink era (the
    /// event copies the old package upload merged into this index under
    /// `linux:<origin>`, their call children and their materialized
    /// rollups), the snapshot lands in `remote_rollup`, and the ack stamp
    /// flips the panel's upload status. Returns the resolved origin.
    ///
    /// The registry read runs on the state connection first — device state
    /// never lives in the index — so the write half stays one network
    /// transaction, the same split `release_acked_dirty` uses (state
    /// decides, index writes).
    ///
    /// The snapshot is INSERT OR REPLACE per row and deliberately does NOT
    /// delete days it does not mention: aggregate history is
    /// append-mostly, and the only full clear is disconnect's
    /// `DELETE FROM remote_rollup`.
    pub fn apply_uplink_rollup(
        &mut self,
        device_id: &str,
        rows: &[crate::transport::RollupRow],
        at_ms: i64,
    ) -> SyncResult<String> {
        let origin: String = self.state.query_row(
            "SELECT origin FROM device_registry WHERE device_id = ?1",
            params![device_id], |row| row.get(0),
        ).optional().map_err(sql_err)?
            .ok_or_else(|| SyncError::NotPaired(device_id.to_string()))?;
        validate_identifier("origin_label", &origin)?;
        let tx_origin = origin.clone();
        self.index.with_network_transaction(move |tx| {
            let migrated: Option<String> = tx
                .query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    params![format!("{UPLINK_ROLLUP_MIGRATED_PREFIX}{device_id}")],
                    |row| row.get(0),
                )
                .optional().map_err(sql_err)?;
            if migrated.is_none() {
                // One-time retirement of the row-merged uplink era for this
                // device: the aggregate snapshot replaces those copies, so
                // figures neither double count nor lose history.
                tx.execute(
                    &format!(
                        "DELETE FROM call WHERE event_id IN (SELECT id FROM event WHERE ({ORIGIN_SQL}) = ?1)"
                    ),
                    params![tx_origin],
                ).map_err(sql_err)?;
                tx.execute(
                    &format!("DELETE FROM event WHERE ({ORIGIN_SQL}) = ?1"),
                    params![tx_origin],
                ).map_err(sql_err)?;
                tx.execute(
                    "DELETE FROM event_rollup WHERE origin = ?1",
                    params![tx_origin],
                ).map_err(sql_err)?;
                tx.execute(
                    "INSERT INTO meta(key, value) VALUES(?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![format!("{UPLINK_ROLLUP_MIGRATED_PREFIX}{device_id}"), tx_origin],
                ).map_err(sql_err)?;
            }
            for row in rows {
                tx.execute(
                    "INSERT OR REPLACE INTO remote_rollup(
                        day, origin, tool, session, project, model, meter,
                        in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts
                     ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                    params![
                        row.day, tx_origin, row.tool, row.session, row.project, row.model, row.meter,
                        row.in_tok, row.cc_tok, row.cr_tok, row.out_tok, row.reason_tok, row.credits,
                        row.n, row.nonzero, row.min_ts, row.max_ts,
                    ],
                ).map_err(sql_err)?;
            }
            tx.execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![format!("{UPLINK_ROLLUP_ACKED_PREFIX}{device_id}"), at_ms.to_string()],
            ).map_err(sql_err)?;
            Ok::<_, SyncError>(())
        })?;
        // The registry row is the panel's only display of this device's sync
        // recency: without this stamp the primary's list shows neither a
        // merge time nor a complete snapshot, while the secondary's own sheet
        // honestly says 刚刚同步 — the row-era writer died with the row
        // pipeline. A rollup snapshot is all-at-once by construction, so one
        // successful apply is a complete snapshot. The registry lives in the
        // state db, outside the index transaction above; the ws layer has
        // already re-checked the paired status around this call.
        self.state.execute(
            "UPDATE device_registry SET last_merged_at_ms = ?2, snapshot_complete = 1 WHERE device_id = ?1",
            params![device_id, at_ms],
        ).map_err(sql_err)?;
        Ok(origin)
    }

    /// The sender side of the uplink: this machine's own rollup rows,
    /// exactly what the primary will store under our registry origin.
    /// `event_rollup.origin` is the materialized `ORIGIN_SQL` of each
    /// event's source (usage_index folds it in at ingest time), so the
    /// local rows are precisely `origin = ''`; merged imports carry their
    /// bundle origin and never travel this wire.
    pub fn local_rollup_snapshot(&self) -> SyncResult<Vec<crate::transport::RollupRow>> {
        let mut statement = self.index.conn().prepare(
            "SELECT day, origin, tool, session, project, model, meter,
                    in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts
             FROM event_rollup WHERE origin = ''",
        ).map_err(sql_err)?;
        let rows = statement.query_map([], |row| Ok(crate::transport::RollupRow {
            day: row.get(0)?,
            origin: row.get(1)?,
            tool: row.get(2)?,
            session: row.get(3)?,
            project: row.get(4)?,
            model: row.get(5)?,
            meter: row.get(6)?,
            in_tok: row.get(7)?,
            cc_tok: row.get(8)?,
            cr_tok: row.get(9)?,
            out_tok: row.get(10)?,
            reason_tok: row.get(11)?,
            credits: row.get(12)?,
            n: row.get(13)?,
            nonzero: row.get(14)?,
            min_ts: row.get(15)?,
            max_ts: row.get(16)?,
        })).map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        Ok(rows)
    }

    pub fn queue_secondary_disconnect(&mut self, config: &SecondarySyncConfig, now_ms: i64) -> SyncResult<()> {
        config.validate(&config.primary_id)?;
        let mut config = config.clone();
        config.enabled = false;
        let key = format!("{DISCONNECT_NOTICE_PREFIX}{}", config.device_id);
        let notice = DisconnectNotice { config: config.clone(), attempts: 0, next_attempt_ms: now_ms, last_error: None };
        let encoded = serde_json::to_string(&notice).map_err(|error| SyncError::Invalid(error.to_string()))?;
        self.index.with_network_transaction(|tx| {
            let count: i64 = tx.query_row("SELECT count(*) FROM meta WHERE key GLOB 'device_sync_disconnect:*'", [], |row| row.get(0)).map_err(sql_err)?;
            if count >= MAX_DISCONNECT_NOTICES { return Err(SyncError::Limit("pending disconnect notification queue is full".into())); }
            tx.execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, encoded],
            ).map_err(sql_err)?;
            // Keep one identity per primary across explicit re-pairing. This
            // preserves the primary's per-device event keys and prevents a
            // fresh snapshot from counting the same local history twice.
            tx.execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![format!("device_sync_identity:{}", config.primary_id), config.identity_path.to_string_lossy().as_ref()],
            ).map_err(sql_err)?;
            // The origin the merged downlink rows carry leaves the figures at
            // disconnect time (`read_detached_origins`). Pages record it as
            // they merge; a pairing that has not pulled a page under this
            // build recovers it from the merged rows' `linux:<origin>:<key>`
            // sources, and one with no downlink rows at all records nothing —
            // there is nothing to detach. `DO NOTHING`: whatever a page wrote
            // for this pairing stands.
            let recorded: Option<String> = tx
                .query_row("SELECT value FROM meta WHERE key = 'device_sync_downlink_origin'", [], |row| row.get(0))
                .optional().map_err(sql_err)?;
            let downlink_origin = recorded.or_else(|| {
                let mut stmt = tx.prepare("SELECT source FROM event WHERE source LIKE 'linux:%' LIMIT 64").ok()?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0)).ok()?;
                for source in rows.flatten() {
                    let origin = origin_of(&source);
                    if !origin.is_empty() { return Some(origin.to_string()); }
                }
                None
            });
            if let Some(downlink_origin) = downlink_origin {
                tx.execute(
                    "INSERT INTO meta(key, value) VALUES('device_sync_downlink_origin', ?1)
                     ON CONFLICT(key) DO NOTHING",
                    params![downlink_origin],
                ).map_err(sql_err)?;
            }
            // 分家: the peer's aggregate rows leave the figures with the
            // pairing — the fold unions remote_rollup, so removing the rows
            // IS the detach. A re-pair re-syncs the snapshot in seconds.
            tx.execute("DELETE FROM remote_rollup", []).map_err(sql_err)?;
            tx.execute("DELETE FROM meta WHERE key = 'device_sync_disconnect_requested'", []).map_err(sql_err)?;
            tx.execute(
                "INSERT INTO meta(key, value) VALUES('device_sync_last_unpaired_at_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![now_ms.to_string()],
            ).map_err(sql_err)?;
            Ok::<_, SyncError>(())
        })?;
        Self::state_tx(&mut self.state, |tx| {
            tx.execute("DELETE FROM device_sync_secondary_config WHERE config_key = 'secondary' AND device_id = ?1", params![config.device_id]).map_err(sql_err)?;
            Ok(())
        })?;
        self.write_capture_mirror()
    }

    pub fn read_disconnect_notices(path: impl AsRef<std::path::Path>, now_ms: i64) -> SyncResult<Vec<DisconnectNotice>> {
        let path = path.as_ref();
        if !path.exists() { return Ok(Vec::new()); }
        if std::fs::symlink_metadata(path).map_err(|error| SyncError::Io(path.to_path_buf(), error))?.file_type().is_symlink() {
            return Err(SyncError::Security("sync index is a symlink".into()));
        }
        let conn = open_read_only(path)?;
        read_disconnect_notices_conn(&conn, now_ms)
    }

    pub fn read_retained_identity_path(path: impl AsRef<std::path::Path>, primary_id: &str) -> SyncResult<Option<PathBuf>> {
        if !path.as_ref().exists() { return Ok(None); }
        let conn = open_read_only(path)?;
        let value: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1", params![format!("device_sync_identity:{primary_id}")], |row| row.get(0)).optional().map_err(sql_err)?;
        Ok(value.map(PathBuf::from))
    }

    pub fn finish_disconnect_notice(&mut self, device_id: &str, now_ms: i64, error: Option<&str>) -> SyncResult<()> {
        let key = format!("{DISCONNECT_NOTICE_PREFIX}{device_id}");
        // A late notice can succeed after the user has already bound a new
        // primary; the binding check reads the state registry before the
        // index-side cleanup runs.
        let has_binding: bool = self.state.query_row(
            "SELECT EXISTS(SELECT 1 FROM device_sync_secondary_config WHERE config_key = 'secondary')",
            [], |row| row.get(0),
        ).map_err(sql_err)?;
        self.index.with_network_transaction(|tx| {
            let Some(error) = error else {
                // The pinned primary confirmed revocation. Retain audit
                // metadata, but release retired payload bytes: the local
                // usage index remains the source for the next full snapshot.
                let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key = ?1)", params![key], |row| row.get(0)).map_err(sql_err)?;
                if !exists { return Ok(()); }
                if !has_binding {
                    // 分家: with the binding gone, the peer's aggregate rows
                    // leave the figures (the fold unions remote_rollup).
                    tx.execute("DELETE FROM remote_rollup", []).map_err(sql_err)?;
                }
                let audit = serde_json::json!({ "completed_at_ms": now_ms }).to_string();
                tx.execute("INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![format!("device_sync_disconnect_done:{device_id}"), audit]).map_err(sql_err)?;
                tx.execute("DELETE FROM meta WHERE key = ?1", params![key]).map_err(sql_err)?;
                return Ok(());
            };
            let encoded: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| row.get(0)).optional().map_err(sql_err)?;
            if let Some(encoded) = encoded {
                let mut notice: DisconnectNotice = serde_json::from_str(&encoded).map_err(|error| SyncError::Invalid(error.to_string()))?;
                notice.attempts = notice.attempts.saturating_add(1);
                let delay = (15_000_i64.saturating_mul(1_i64 << notice.attempts.saturating_sub(1).min(6))).min(NOTICE_BACKOFF_MAX_MS);
                notice.next_attempt_ms = now_ms.saturating_add(delay);
                notice.last_error = Some(error.chars().take(512).collect());
                let encoded = serde_json::to_string(&notice).map_err(|error| SyncError::Invalid(error.to_string()))?;
                tx.execute("UPDATE meta SET value = ?2 WHERE key = ?1", params![key, encoded]).map_err(sql_err)?;
            }
            Ok(())
        })
    }

    pub fn secondary_disconnect_requested(&self) -> SyncResult<bool> {
        let value: Option<String> = self.index.conn().query_row(
            "SELECT value FROM meta WHERE key = 'device_sync_disconnect_requested'",
            [], |row| row.get(0),
        ).optional().map_err(sql_err)?;
        Ok(value.as_deref() == Some(self.primary_id.as_str()))
    }

    /// Read-only twin of `secondary_disconnect_requested` for the panel's
    /// status poll: answers from the index `meta` table without opening a
    /// writable store, so a polling snapshot never queues behind the ingest
    /// engine's write transactions. A missing index has no request pending.
    pub fn read_secondary_disconnect_requested(path: impl AsRef<std::path::Path>, primary_id: &str) -> SyncResult<bool> {
        let path = path.as_ref();
        if !path.exists() { return Ok(false); }
        let conn = open_read_only(path)?;
        disconnect_requested_conn(&conn, primary_id)
    }

    /// Forget every stream-scoped checkpoint of the secondary binding without
    /// touching local events or the pairing itself. `needs_recovery` marks the
    /// binding so the next runtime pass re-establishes the session from
    /// scratch; the next upload re-ships the whole local rollup snapshot.
    pub fn reset_secondary_sync_state(&mut self, now_ms: i64) -> SyncResult<SecondaryResetSummary> {
        let device_id: Option<String> = self.state.query_row(
            "SELECT device_id FROM device_sync_secondary_config WHERE config_key = 'secondary' AND primary_id = ?1",
            params![self.primary_id], |row| row.get(0),
        ).optional().map_err(sql_err)?;
        let Some(device_id) = device_id else { return Err(SyncError::NotFound("secondary sync configuration".into())); };
        Self::state_tx(&mut self.state, |tx| {
            let changed = tx.execute(
                "UPDATE device_sync_secondary_config
                    SET stream_id = NULL, data_epoch = NULL, expected_generation = NULL,
                        last_manifest_hash = NULL, last_gzip_hash = NULL,
                        last_ack_generation = NULL, last_ack_receipt_hash = NULL,
                        last_error = NULL, needs_recovery = 1, updated_at_ms = ?1
                  WHERE config_key = 'secondary' AND primary_id = ?2 AND device_id = ?3",
                params![now_ms, self.primary_id, device_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("secondary sync configuration".into())); }
            Ok(())
        })?;
        Ok(SecondaryResetSummary { device_id })
    }

    /// True after a reset until a fresh stream has been adopted from the
    /// primary's recovery handshake.
    pub fn secondary_needs_recovery(&self) -> SyncResult<bool> {
        let value: Option<i64> = self.state.query_row(
            "SELECT needs_recovery FROM device_sync_secondary_config WHERE config_key = 'secondary' AND primary_id = ?1",
            params![self.primary_id], |row| row.get(0),
        ).optional().map_err(sql_err)?;
        Ok(value.is_some_and(|value| value != 0))
    }

    /// Remove the active secondary binding while leaving local usage history
    /// intact for audit/reconciliation.
    pub fn clear_secondary_config(&mut self, now_ms: i64) -> SyncResult<Option<PathBuf>> {
        let identity_path: Option<String> = Self::state_tx(&mut self.state, |tx| {
            let identity_path: Option<String> = tx.query_row(
                "SELECT identity_path FROM device_sync_secondary_config WHERE config_key = 'secondary' AND primary_id = ?1",
                params![self.primary_id], |row| row.get(0),
            ).optional().map_err(sql_err)?;
            tx.execute(
                "DELETE FROM device_sync_secondary_config WHERE config_key = 'secondary' AND primary_id = ?1",
                params![self.primary_id],
            ).map_err(sql_err)?;
            Ok(identity_path)
        })?;
        self.write_capture_mirror()?;
        self.index.with_network_transaction(|tx| {
            // 分家: the departed primary's aggregate rows leave the figures;
            // a re-pair re-syncs the snapshot in seconds.
            tx.execute("DELETE FROM remote_rollup", []).map_err(sql_err)?;
            tx.execute(
                "DELETE FROM meta WHERE key = 'device_sync_disconnect_requested' AND value = ?1",
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
        Self::state_tx(&mut self.state, |tx| {
            let changed = tx.execute(
                "UPDATE device_sync_secondary_config SET capture_enabled = ?1, updated_at_ms = ?2 WHERE config_key = 'secondary' AND primary_id = ?3",
                params![if enabled { 1_i64 } else { 0_i64 }, now_ms, self.primary_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("secondary sync configuration".into())); }
            Ok(())
        })?;
        self.write_capture_mirror()
    }

    pub fn set_secondary_last_error(&mut self, error: Option<&str>, now_ms: i64) -> SyncResult<()> {
        let error = error.map(|value| value.chars().take(512).collect::<String>());
        Self::state_tx(&mut self.state, |tx| {
            tx.execute(
                "UPDATE device_sync_secondary_config SET last_error = ?1, updated_at_ms = ?2 WHERE config_key = 'secondary' AND primary_id = ?3",
                params![error, now_ms, self.primary_id],
            ).map_err(sql_err)?;
            Ok(())
        })
    }

    /// The primary acknowledged this device's last rollup push: stamp it in
    /// the SECONDARY's own index so `upload_cursor_complete_conn` can answer
    /// truthfully on the next panel poll. The primary's copy of the same key
    /// lives in its index and means the same thing from its side.
    pub fn mark_uplink_rollup_acked(&mut self, at_ms: i64) -> SyncResult<()> {
        let device_id = self
            .secondary_config()?
            .ok_or_else(|| SyncError::NotPaired("rollup ack requires a paired secondary".into()))?
            .device_id;
        self.index.with_network_transaction(|tx| {
            tx.execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![format!("{UPLINK_ROLLUP_ACKED_PREFIX}{device_id}"), at_ms.to_string()],
            ).map_err(sql_err)?;
            Ok::<_, SyncError>(())
        })
    }

    pub fn mark_secondary_connected(&mut self, now_ms: i64) -> SyncResult<()> {
        // Called once per successful status round trip. Refresh the timestamp
        // at most every 30 s and clear a stale error immediately; the old
        // unconditional UPDATE (plus the separate error clear) committed a
        // state transaction every poll.
        let current: Option<(Option<i64>, Option<String>)> = self.state.query_row(
            "SELECT last_connected_at_ms, last_error FROM device_sync_secondary_config
              WHERE config_key = 'secondary' AND primary_id = ?1",
            params![self.primary_id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(sql_err)?;
        let Some((last_connected_at_ms, last_error)) = current else { return Ok(()); };
        if last_error.is_none() && last_connected_at_ms.is_some_and(|ts| now_ms.saturating_sub(ts) < 30_000) {
            return Ok(());
        }
        self.state.execute(
            "UPDATE device_sync_secondary_config
                SET last_connected_at_ms = ?1, last_error = NULL, updated_at_ms = ?1
              WHERE config_key = 'secondary' AND primary_id = ?2",
            params![now_ms, self.primary_id],
        ).map_err(sql_err)?;
        Ok(())
    }

    pub fn set_secondary_primary_display_name(&mut self, name: &str, now_ms: i64) -> SyncResult<()> {
        valid_text("primary_display_name", name, false)?;
        Self::state_tx(&mut self.state, |tx| {
            let changed = tx.execute(
                "UPDATE device_sync_secondary_config SET primary_display_name = ?1, updated_at_ms = ?2 WHERE config_key = 'secondary' AND primary_id = ?3",
                params![name, now_ms, self.primary_id],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotFound("secondary sync configuration".into())); }
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
        Self::state_tx(&mut self.state, |tx| {
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
        let state = Self::state_tx(&mut self.state, |tx| {
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
        })?;
        // Auto-approve is a primary-side setting: when it is on, a fresh
        // pending request is approved on arrival, so the secondary pairs
        // without waiting for a human tap. An approval failure leaves the
        // request pending — the manual list still works, and the error is
        // visible there instead of failing an already-accepted request.
        if state.status == "pending" && self.primary_config()?.is_some_and(|config| config.auto_approve_pairing) {
            match self.approve_pairing(&state.request_id, now_ms) {
                Ok(_) => return Ok(PairingRequestState { status: "paired".into(), ..state }),
                Err(error) => eprintln!("tokenme sync: auto-approve failed, the request stays pending: {error}"),
            }
        }
        Ok(state)
    }

    pub fn pairing_requests(&self) -> SyncResult<Vec<PairingRequestState>> {
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| SyncError::Invalid(format!("system clock is before unix epoch: {e}")))?.as_millis().min(i64::MAX as u128) as i64;
        self.pairing_requests_at(now_ms)
    }

    pub fn pairing_requests_at(&self, now_ms: i64) -> SyncResult<Vec<PairingRequestState>> {
        let mut stmt = self.state.prepare(
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
        let approved_device_id = Self::state_tx(&mut self.state, |tx| {
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
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM device_registry WHERE device_id = ?1)",
                    params![device_id], |row| row.get(0),
                ).map_err(sql_err)?;
                if !exists { return Err(SyncError::NotFound("approved device".into())); }
                return Ok(device_id);
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
            let existing: Option<(Vec<u8>, String)> = tx.query_row(
                "SELECT public_key, origin FROM device_registry WHERE device_id = ?1",
                params![device_id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(sql_err)?;
            if let Some((stored_key, stored_origin)) = existing {
                if stored_key.as_slice() != key.as_slice() || stored_origin != origin {
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
            Ok(device_id)
        })?;
        self.write_capture_mirror()?;
        self.device(&approved_device_id)?.ok_or_else(|| SyncError::NotFound("registered device".into()))
    }

    /// Clear a pairing request from the actionable queue without ever making
    /// an expired request approvable.  A request whose invite crossed its
    /// expiry boundary is first materialized as `expired` in the same
    /// transaction, then both persisted `expired` rows and still-pending rows
    /// are moved to the retained `rejected` history state.
    pub fn clear_pairing_request(&mut self, request_id: &str, now_ms: i64) -> SyncResult<()> {
        Self::state_tx(&mut self.state, |tx| {
            tx.execute(
                "UPDATE device_pairing_request
                 SET status = 'expired'
                 WHERE request_id = ?1 AND status = 'pending' AND EXISTS(
                     SELECT 1 FROM device_pairing_invite i
                     WHERE i.invite_id = device_pairing_request.invite_id
                       AND i.expires_at_ms <= ?2
                 )",
                params![request_id, now_ms],
            ).map_err(sql_err)?;
            let changed = tx.execute(
                "UPDATE device_pairing_request SET status = 'rejected'
                 WHERE request_id = ?1 AND status IN ('pending', 'expired')",
                params![request_id],
            ).map_err(sql_err)?;
            if changed == 0 {
                let status: Option<String> = tx.query_row(
                    "SELECT status FROM device_pairing_request WHERE request_id = ?1",
                    params![request_id],
                    |row| row.get(0),
                ).optional().map_err(sql_err)?;
                return Err(match status {
                    Some(status) => SyncError::Conflict(format!("pairing request is {status}")),
                    None => SyncError::NotFound("pairing request".into()),
                });
            }
            Ok(())
        })
    }

    /// Backward-compatible rejection entry point for callers that do not
    /// supply a deterministic clock.  The command path uses
    /// `clear_pairing_request` with its captured operation timestamp.
    pub fn reject_pairing(&mut self, request_id: &str) -> SyncResult<()> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| SyncError::Invalid(format!("system clock is before unix epoch: {error}")))?
            .as_millis()
            .min(i64::MAX as u128) as i64;
        self.clear_pairing_request(request_id, now_ms)
    }

    pub fn pause_device(&mut self, device_id: &str) -> SyncResult<()> {
        self.set_device_status(device_id, DeviceStatus::Paused)
    }

    pub fn resume_device(&mut self, device_id: &str) -> SyncResult<()> {
        self.set_device_status(device_id, DeviceStatus::Paired)
    }

    fn set_device_status(&mut self, device_id: &str, status: DeviceStatus) -> SyncResult<()> {
        Self::state_tx(&mut self.state, |tx| {
            let changed = tx.execute("UPDATE device_registry SET status = ?2 WHERE device_id = ?1 AND status != 'revoked'", params![device_id, status.as_str()]).map_err(sql_err)?;
            if changed == 0 {
                return Err(SyncError::NotFound("device".into()));
            }
            Ok(())
        })
    }

    pub fn revoke_device(&mut self, device_id: &str, now_ms: i64) -> SyncResult<u64> {
        let new_epoch = Self::state_tx(&mut self.state, |tx| {
            let epoch: Option<i64> = tx.query_row("SELECT data_epoch FROM device_registry WHERE device_id = ?1", params![device_id], |row| row.get(0)).optional().map_err(sql_err)?;
            let Some(epoch) = epoch else { return Err(SyncError::NotFound("device".into())); };
            let new_epoch = epoch.checked_add(1).ok_or_else(|| SyncError::Invalid("data epoch overflow".into()))?;
            tx.execute("UPDATE device_registry SET status = 'revoked', data_epoch = ?2, revoked_at_ms = ?3 WHERE device_id = ?1", params![device_id, new_epoch, now_ms]).map_err(sql_err)?;
            Ok::<_, SyncError>(new_epoch)
        })?;
        self.write_capture_mirror()?;
        Ok(new_epoch as u64)
    }

    pub fn update_display_name(&mut self, request: &AuthenticatedDisplayNameRequest, now_ms: i64) -> SyncResult<()> {
        if request.primary_id != self.primary_id {
            return Err(SyncError::Auth("display-name update targets another primary".into()));
        }
        let request = request.clone();
        Self::state_tx(&mut self.state, |tx| {
            let registered: Option<(Vec<u8>, String, String)> = tx.query_row(
                "SELECT public_key, status, origin FROM device_registry WHERE device_id = ?1",
                params![request.device_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional().map_err(sql_err)?;
            let Some((public_key, status, origin)) = registered else {
                return Err(SyncError::NotPaired(request.device_id.clone()));
            };
            if status == "revoked" { return Err(SyncError::Revoked(request.device_id.clone())); }
            if status != "paired" { return Err(SyncError::NotPaired(request.device_id.clone())); }
            if origin != request.origin {
                return Err(SyncError::Auth("display-name update origin does not match paired device".into()));
            }
            request.verify(&public_key_from_bytes(&public_key)?)?;
            if !request.is_fresh(now_ms) {
                return Err(SyncError::Auth("display-name update is expired or from the future".into()));
            }
            let changed = tx.execute(
                "UPDATE device_registry SET display_name = ?2 WHERE device_id = ?1 AND origin = ?3 AND status = 'paired'",
                params![request.device_id, request.display_name, request.origin],
            ).map_err(sql_err)?;
            if changed != 1 { return Err(SyncError::NotPaired(request.device_id.clone())); }
            Ok(())
        })
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

    /// Reclaims the scheduler lease a killed panel process left hanging.
    /// The panel app is single-instance, so any `panel-secondary-{device}-*`
    /// lease that is not ours was written by a process that no longer exists
    /// — an installer's forced stop, a crash, a reboot. Waiting out its TTL
    /// (15 minutes) is what made an upgraded panel show its primary offline
    /// for a quarter hour while the network was perfectly fine. Foreign
    /// owners (a CLI runtime, another device's prefix) are never touched:
    /// those may be genuinely alive and keep their wait-out semantics.
    /// `device_id` is a fixed `dev-` + hex literal, so the LIKE pattern
    /// carries no wildcards of its own.
    pub fn steal_stale_panel_lease(&mut self, device_id: &str) -> SyncResult<bool> {
        let pattern = format!("panel-secondary-{device_id}-%");
        self.index.with_network_transaction(|tx| {
            let stolen = tx.execute(
                "DELETE FROM device_sync_scheduler_lease WHERE lease_key = 'outbox' AND owner LIKE ?1",
                params![pattern],
            ).map_err(sql_err)?;
            Ok(stolen > 0)
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

/// State-side half of a role switch: the registry, pairing queue and both role
/// configurations belong to the old primary and must not leak into the new
/// one, while the `device_sync_meta` binding records the new identity.
fn rotate_state_primary_id(state: &mut Connection, new_primary_id: &str) -> SyncResult<()> {
    let tx = state.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql_err)?;
    let result = rotate_state_primary_id_tx(&tx, new_primary_id);
    match result {
        Ok(()) => tx.commit().map_err(sql_err),
        Err(error) => { let _ = tx.rollback(); Err(error) }
    }
}

fn rotate_state_primary_id_tx(tx: &Transaction<'_>, new_primary_id: &str) -> SyncResult<()> {
    for table in [
        "device_registry", "device_sync_secondary_config", "device_sync_primary_config",
        "device_pairing_invite", "device_pairing_request",
    ] {
        tx.execute(&format!("DELETE FROM {table}"), []).map_err(sql_err)?;
    }
    tx.execute(
        "INSERT INTO device_sync_meta(key, value) VALUES('primary_id', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![new_primary_id],
    ).map_err(sql_err)?;
    Ok(())
}

/// Index-side half of a role switch: streams, receipts, markers, conflicts,
/// outbox/journal payloads and lease/recovery state of the old primary go.
/// The rows the old primary *downloaded* go too: a `device-sync:v1:` dedupe
/// key embeds the primary id (`network_event_key`), so a new primary re-sends
/// the same origin data under fresh keys and keeping the old rows would
/// Index-side half of a role switch: the lease and recovery state of the old
/// primary go. The rows the old primary *downloaded* go too: a `device-sync:v1:`
/// dedupe key embeds the primary id (`network_event_key`), so a new primary
/// re-sends the same origin data under fresh keys and keeping the old rows would
/// double-count every origin in the report. The days the rows touched are
/// recomputed in the same transaction, the way the import path did.
fn rotate_index_sync_state_tx(tx: &Transaction<'_>, now_ms: i64) -> SyncResult<()> {
    // Only the lease survives the row-era cleanup; the conflict/recovery
    // tables were dropped with it, so there is nothing else to clear.
    tx.execute("DELETE FROM device_sync_scheduler_lease", []).map_err(sql_err)?;
    // Downloaded rows are keyed by `network_event_key`: a lowercase literal
    // plus lowercase hex, so the ASCII range ['device-sync:v1:', 'device-sync:v1;')
    // selects exactly the rows `LIKE 'device-sync:v1:%'` did — and, unlike the
    // LIKE (case-insensitive by default, which reduced event_dedupe to a bare
    // index scan), it can seek. ';' is ':' + 1, and no other key family
    // collides with the prefix under any casing.
    const DOWNLOAD_KEY_LO: &str = "device-sync:v1:";
    const DOWNLOAD_KEY_HI: &str = "device-sync:v1;";
    let doomed_ts: Vec<i64> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT ts_ms FROM event WHERE dedupe_key >= '{DOWNLOAD_KEY_LO}' AND dedupe_key < '{DOWNLOAD_KEY_HI}'"
        )).map_err(sql_err)?;
        let rows = stmt.query_map([], |row| row.get(0)).map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?
    };
    if !doomed_ts.is_empty() {
        tx.execute(
            &format!("DELETE FROM call WHERE event_id IN (SELECT id FROM event WHERE dedupe_key >= '{DOWNLOAD_KEY_LO}' AND dedupe_key < '{DOWNLOAD_KEY_HI}')"),
            [],
        ).map_err(sql_err)?;
        tx.execute(
            &format!("DELETE FROM event WHERE dedupe_key >= '{DOWNLOAD_KEY_LO}' AND dedupe_key < '{DOWNLOAD_KEY_HI}'"),
            [],
        ).map_err(sql_err)?;
        usage_index::recompute_network_days(tx, &doomed_ts).map_err(SyncError::from)?;
        ensure_network_rollups_finite(tx)?;
    }
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('device_sync_last_role_switch_at_ms', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![now_ms.to_string()],
    ).map_err(sql_err)?;
    Ok(())
}

/// The ingest-time capture switch, computed from the state database: a
/// secondary's `capture_enabled` wins (a network pause must not pause local
/// capture); a primary journals while any downlink consumer — paired, paused
/// or reconciling — can still need it. Paused and reconciling devices are
/// offline, not unsubscribed: their cursor survives, so an update to an event
/// below it can only arrive through the change feed. Revoking is the explicit
/// way to give that share up.
/// Whether the uplink snapshot has a live consumer: a secondary config that
/// exists and is enabled. Paused keeps the pin (offline, not unsubscribed —
/// the config row survives a pause); disabling the sync switch, disconnect
/// or a missing config releases it, and prune owns retention again.
fn sync_uplink_paired_state(state: &Connection) -> SyncResult<bool> {
    state.query_row(
        "SELECT EXISTS(SELECT 1 FROM device_sync_secondary_config WHERE config_key = 'secondary' AND enabled = 1)",
        [], |row| row.get(0),
    ).map_err(sql_err)
}

fn sync_capture_enabled_state(state: &Connection) -> SyncResult<bool> {
    let has_capture_column: bool = state.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('device_sync_secondary_config') WHERE name = 'capture_enabled')",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    let column = if has_capture_column { "capture_enabled" } else { "enabled" };
    let secondary: Option<i64> = state.query_row(
        &format!("SELECT {column} FROM device_sync_secondary_config WHERE config_key = 'secondary'"),
        [], |row| row.get(0),
    ).optional().map_err(sql_err)?;
    if secondary.is_some() {
        return Ok(secondary == Some(1));
    }
    let configured: bool = state.query_row(
        "SELECT EXISTS(SELECT 1 FROM device_sync_primary_config WHERE config_key = 'primary')",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    if !configured {
        return Ok(false);
    }
    let consumers: i64 = state.query_row(
        "SELECT count(*) FROM device_registry WHERE status IN ('paired', 'paused', 'reconcile_required')", [], |row| row.get(0),
    ).map_err(sql_err)?;
    Ok(consumers > 0)
}

fn save_secondary_config_tx(tx: &Transaction<'_>, config: &SecondarySyncConfig, now_ms: i64) -> SyncResult<()> {
    tx.execute(
        "INSERT OR REPLACE INTO device_sync_secondary_config(
            config_key, primary_id, device_id, origin, endpoint, server_name,
            certificate_der, certificate_pin, identity_path, stream_id, data_epoch,
            expected_generation, enabled, capture_enabled, last_error, last_manifest_hash, last_gzip_hash,
            last_connected_at_ms, last_ack_generation, last_ack_receipt_hash, primary_display_name, primary_os, updated_at_ms
         ) VALUES('secondary', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
        params![
            config.primary_id, config.device_id, config.origin, config.endpoint,
            config.server_name, config.certificate_der, config.certificate_pin.as_slice(),
            config.identity_path.to_string_lossy().as_ref(), config.stream_id,
            config.data_epoch.map(|value| value as i64), config.expected_generation.map(|value| value as i64),
            if config.enabled { 1_i64 } else { 0_i64 }, if config.capture_enabled { 1_i64 } else { 0_i64 }, config.last_error,
            config.last_manifest_hash.map(|value| value.to_vec()), config.last_gzip_hash.map(|value| value.to_vec()),
            config.last_connected_at_ms, config.last_ack_generation.map(|value| value as i64),
            config.last_ack_receipt_hash.map(|value| value.to_vec()), config.primary_display_name, config.primary_os, now_ms,
        ],
    ).map_err(sql_err)?;
    Ok::<_, SyncError>(())
}

/// Tables the index side of the device-sync schema owns. `migrate`'s DDL and
/// `index_schema_complete` must describe the same set: a table present in the
/// DDL but missing here would let the fast path skip its creation on an old
/// database, so additions go in both places. The row-era stream/receipt/outbox
/// tables are dropped by `migrate`, not recreated.
const DEVICE_INDEX_TABLES: &[&str] = &[
    "device_sync_scheduler_lease",
];

/// Read-only probe for the device tables `migrate` ensures. A complete schema
/// means the DDL batch would be a no-op, so the caller can skip taking the
/// index write transaction entirely — that transaction is what made every
/// `SyncStore::open` queue for seconds behind the ingest engine.
fn index_schema_complete(conn: &Connection) -> bool {
    let mut tables: i64 = 0;
    if let Ok(value) = conn.query_row(
        &format!(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ({})",
            DEVICE_INDEX_TABLES.iter().map(|name| format!("'{name}'")).collect::<Vec<_>>().join(","),
        ),
        [], |row| row.get(0),
    ) { tables = value; }
    if tables != DEVICE_INDEX_TABLES.len() as i64 { return false; }
    // A pre-wave-3 database already has every kept table, so the count above
    // alone would take the fast path and the row-era DROP batch would never
    // run on it. Complete means: kept tables present AND dead tables gone.
    let dead: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN (
             'device_stream', 'device_receipt', 'device_package_marker',
             'device_package_tombstone', 'device_highwater', 'device_sync_outbox',
             'device_sync_outbox_sequence', 'device_sync_journal', 'device_sync_snapshot_cursor',
             'device_sync_conflict', 'device_sync_recovery'
         )",
        [], |row| row.get(0),
    ).unwrap_or(1);
    dead == 0
}

/// The one upsert behind `save_primary_config` and its state-only static
/// twin, so the instance write and the panel's direct write cannot drift.
fn upsert_primary_config_tx(tx: &rusqlite::Transaction<'_>, config: &PrimarySyncConfig, now_ms: i64) -> SyncResult<()> {
    tx.execute(
        "INSERT INTO device_sync_primary_config(
            config_key, primary_id, bind_addr, advertise_host, allow_lan, enabled,
            share_data_with_secondaries, auto_approve_pairing, updated_at_ms
         ) VALUES('primary', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(config_key) DO UPDATE SET
            primary_id = excluded.primary_id,
            bind_addr = excluded.bind_addr,
            advertise_host = excluded.advertise_host,
            allow_lan = excluded.allow_lan,
            enabled = excluded.enabled,
            share_data_with_secondaries = excluded.share_data_with_secondaries,
            auto_approve_pairing = excluded.auto_approve_pairing,
            updated_at_ms = excluded.updated_at_ms",
        params![
            config.primary_id, config.bind_addr, config.advertise_host,
            if config.allow_lan { 1_i64 } else { 0_i64 },
            if config.enabled { 1_i64 } else { 0_i64 },
            if config.share_data_with_secondaries { 1_i64 } else { 0_i64 },
            if config.auto_approve_pairing { 1_i64 } else { 0_i64 }, now_ms,
        ],
    ).map_err(sql_err)?;
    Ok(())
}

fn migrate(index: &mut Index) -> SyncResult<()> {
    if index_schema_complete(index.conn()) { return Ok(()); }
    index.with_network_transaction(|tx| {
        // The row-era machinery's storage is shed here, once per open-until-
        // complete: the stream/receipt/outbox/journal/cursor tables have no
        // readers left, and `index_schema_complete` no longer counts them, so
        // an old database runs this batch once and then takes the fast path.
        tx.execute_batch(
            "DROP TABLE IF EXISTS device_stream;
             DROP TABLE IF EXISTS device_receipt;
             DROP TABLE IF EXISTS device_package_marker;
             DROP TABLE IF EXISTS device_package_tombstone;
             DROP TABLE IF EXISTS device_highwater;
             DROP TABLE IF EXISTS device_sync_outbox;
             DROP TABLE IF EXISTS device_sync_outbox_sequence;
             DROP TABLE IF EXISTS device_sync_journal;
             DROP TABLE IF EXISTS device_sync_snapshot_cursor;
             DROP TABLE IF EXISTS device_sync_conflict;
             DROP TABLE IF EXISTS device_sync_recovery;
             CREATE TABLE IF NOT EXISTS device_sync_scheduler_lease(
                 lease_key TEXT PRIMARY KEY, owner TEXT NOT NULL, expires_at_ms INTEGER NOT NULL
             );",
        ).map_err(sql_err)?;
        Ok::<_, SyncError>(())
    })
}

/// Creates/migrates the dedicated device-state schema. The split is
/// destructive: device_* records previously stored inside the index are not
/// migrated, so a fresh `sync_state.db` starts empty and re-pairs.
fn migrate_state(state: &mut Connection, primary_id: &str) -> SyncResult<()> {
    state.execute_batch("PRAGMA synchronous=FULL;").map_err(sql_err)?;
    let tx = state.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql_err)?;
    let result = (|| {
        // Preserve consent from the earlier primary-read setting. If both
        // columns exist, the current setting wins, including an explicit off.
        let (current_sharing, legacy_sharing) = primary_sharing_columns(&tx)?;
        if legacy_sharing {
            let sql = if current_sharing {
                "ALTER TABLE device_sync_primary_config DROP COLUMN allow_secondary_read"
            } else {
                "ALTER TABLE device_sync_primary_config RENAME COLUMN allow_secondary_read TO share_data_with_secondaries"
            };
            tx.execute_batch(sql).map_err(sql_err)?;
        }
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS device_sync_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS device_registry(
                 device_id TEXT PRIMARY KEY, origin TEXT NOT NULL UNIQUE, display_name TEXT NOT NULL,
                 os TEXT NOT NULL, public_key BLOB NOT NULL, status TEXT NOT NULL,
                 data_epoch INTEGER NOT NULL, created_at_ms INTEGER NOT NULL, revoked_at_ms INTEGER,
                 last_merged_at_ms INTEGER, snapshot_complete INTEGER NOT NULL DEFAULT 0
             );
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
                 primary_display_name TEXT,
                 needs_recovery INTEGER NOT NULL DEFAULT 0,
                 updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS device_sync_primary_config(
                 config_key TEXT PRIMARY KEY,
                 primary_id TEXT NOT NULL,
                 bind_addr TEXT NOT NULL,
                 advertise_host TEXT,
                 allow_lan INTEGER NOT NULL,
                 enabled INTEGER NOT NULL,
                 share_data_with_secondaries INTEGER NOT NULL DEFAULT 0,
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
        ensure_column(&tx, "device_sync_primary_config", "share_data_with_secondaries", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(&tx, "device_sync_primary_config", "auto_approve_pairing", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(&tx, "device_sync_secondary_config", "last_connected_at_ms", "INTEGER")?;
        ensure_column(&tx, "device_sync_secondary_config", "last_ack_generation", "INTEGER")?;
        ensure_column(&tx, "device_sync_secondary_config", "last_ack_receipt_hash", "BLOB")?;
        // The registry's merge recency used to live in the row-era tables;
        // with those gone it is a real column here or the panel's device row
        // can never show when a secondary last landed its snapshot.
        ensure_column(&tx, "device_registry", "last_merged_at_ms", "INTEGER")?;
        ensure_column(&tx, "device_registry", "snapshot_complete", "INTEGER NOT NULL DEFAULT 0")?;
        // A parallel line of the same fix briefly shipped a last_sync_at_ms
        // column here; the merged tree stamps the two columns above instead,
        // so a registry carrying the orphan sheds it instead of keeping a
        // second, unread recency stamp beside the real ones.
        let has_orphan: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('device_registry') WHERE name = 'last_sync_at_ms')",
            [], |row| row.get(0),
        ).map_err(sql_err)?;
        if has_orphan {
            tx.execute_batch("ALTER TABLE device_registry DROP COLUMN last_sync_at_ms").map_err(sql_err)?;
        }
        ensure_column(&tx, "device_sync_secondary_config", "capture_enabled", "INTEGER NOT NULL DEFAULT 1")?;
        ensure_column(&tx, "device_sync_secondary_config", "primary_display_name", "TEXT")?;
        ensure_column(&tx, "device_sync_secondary_config", "primary_os", "TEXT")?;
        ensure_column(&tx, "device_sync_secondary_config", "needs_recovery", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(&tx, "device_pairing_invite", "bound_request_id", "TEXT")?;
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
                return Err(SyncError::Security("state database is bound to another primary_id".into()));
            }
        } else {
            tx.execute("INSERT INTO device_sync_meta(key, value) VALUES('primary_id', ?1)", params![primary_id]).map_err(sql_err)?;
        }
        tx.execute("INSERT OR REPLACE INTO device_sync_meta(key, value) VALUES('migration_version', ?1)", params![STATE_VERSION.to_string()]).map_err(sql_err)?;
        Ok::<_, SyncError>(())
    })();
    let result = match result {
        Ok(()) => tx.commit().map_err(sql_err),
        Err(error) => { let _ = tx.rollback(); Err(error) }
    };
    let _ = state.execute_batch("PRAGMA synchronous=NORMAL;");
    result
}

fn validate_identifier(label: &str, value: &str) -> SyncResult<()> {
    if value.is_empty() || value.len() > 16 * 1024 || value.chars().any(char::is_control) {
        return Err(SyncError::Invalid(format!("{label} is empty, too long, or contains control characters")));
    }
    Ok(())
}

fn read_disconnect_notices_conn(conn: &Connection, now_ms: i64) -> SyncResult<Vec<DisconnectNotice>> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key GLOB 'device_sync_disconnect:*' ORDER BY key LIMIT 128").map_err(sql_err)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(sql_err)?;
    let mut notices = Vec::new();
    for row in rows {
        let notice: DisconnectNotice = serde_json::from_str(&row.map_err(sql_err)?).map_err(|error| SyncError::Invalid(error.to_string()))?;
        notice.config.validate(&notice.config.primary_id)?;
        if notice.next_attempt_ms <= now_ms { notices.push(notice); }
    }
    notices.sort_by_key(|notice| notice.next_attempt_ms);
    Ok(notices)
}

fn upload_cursor_complete_conn(conn: &Connection, config: &SecondarySyncConfig) -> Option<bool> {
    // The uplink is a rollup snapshot: "complete" means the primary has
    // stored this device's snapshot at least once — `apply_uplink_rollup`
    // stamps the ack key in the same transaction that writes the rows.
    // `None` means the pairing has not produced an answer yet.
    let acked: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![format!("{UPLINK_ROLLUP_ACKED_PREFIX}{}", config.device_id)],
            |row| row.get(0),
        )
        .ok();
    acked.map(|_| true)
}

fn download_cursor_complete_conn(conn: &Connection, _primary_id: &str) -> Option<bool> {
    // The downlink is a rollup snapshot: "complete" means the first snapshot
    // has been fetched and applied — `apply_remote_rollup` stamps the fetch
    // time in the same transaction that writes the rows. `None` means the
    // primary has not shared a snapshot yet.
    let fetched: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'device_sync_rollup_fetched_at_ms'",
            [],
            |row| row.get(0),
        )
        .ok();
    fetched.map(|_| true)
}

fn disconnect_requested_conn(conn: &Connection, primary_id: &str) -> SyncResult<bool> {
    let value: Option<String> = conn.query_row(
        "SELECT value FROM meta WHERE key = 'device_sync_disconnect_requested'",
        [], |row| row.get(0),
    ).optional().map_err(sql_err)?;
    Ok(value.as_deref() == Some(primary_id))
}

fn primary_sharing_columns(conn: &Connection) -> SyncResult<(bool, bool)> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('device_sync_primary_config') WHERE name = 'share_data_with_secondaries'),
                EXISTS(SELECT 1 FROM pragma_table_info('device_sync_primary_config') WHERE name = 'allow_secondary_read')",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(sql_err)
}

fn read_primary_config_conn(conn: &Connection) -> SyncResult<Option<PrimarySyncConfig>> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'device_sync_primary_config')",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    if !exists { return Ok(None); }
    let (current_sharing, legacy_sharing) = primary_sharing_columns(conn)?;
    let sharing_column = if current_sharing { "share_data_with_secondaries" } else if legacy_sharing { "allow_secondary_read" } else { "0" };
    let auto_approve_column = {
        let mut statement = conn.prepare("PRAGMA table_info(device_sync_primary_config)").map_err(sql_err)?;
        let names = statement.query_map([], |row| row.get::<_, String>(1)).map_err(sql_err)?.collect::<Result<Vec<_>, _>>().map_err(sql_err)?;
        if names.iter().any(|name| name == "auto_approve_pairing") { "auto_approve_pairing" } else { "0" }
    };
    let sql = format!("SELECT primary_id, bind_addr, advertise_host, allow_lan, enabled, {sharing_column}, {auto_approve_column}
                       FROM device_sync_primary_config WHERE config_key = 'primary'");
    let row: Option<(String, String, Option<String>, i64, i64, i64, i64)> = conn.query_row(
        &sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
    ).optional().map_err(sql_err)?;
    let Some((primary_id, bind_addr, advertise_host, allow_lan, enabled, sharing, auto_approve)) = row else { return Ok(None); };
    let allow_lan = match allow_lan {
        0 => false,
        1 => true,
        value => return Err(SyncError::Security(format!("primary allow_lan flag is invalid: {value}"))),
    };
    let enabled = match enabled {
        0 => false,
        1 => true,
        value => return Err(SyncError::Security(format!("primary enabled flag is invalid: {value}"))),
    };
    let share_data_with_secondaries = match sharing {
        0 => false,
        1 => true,
        value => return Err(SyncError::Security(format!("primary data sharing flag is invalid: {value}"))),
    };
    let auto_approve_pairing = match auto_approve {
        0 => false,
        1 => true,
        value => return Err(SyncError::Security(format!("primary auto-approve flag is invalid: {value}"))),
    };
    let config = PrimarySyncConfig {
        primary_id: primary_id.clone(), bind_addr, advertise_host, allow_lan, enabled, share_data_with_secondaries, auto_approve_pairing };
    config.validate(&primary_id)?;
    Ok(Some(config))
}

fn read_secondary_config_conn(conn: &Connection) -> SyncResult<Option<SecondarySyncConfig>> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'device_sync_secondary_config')",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    if !exists { return Ok(None); }
    // One schema probe for every optional column. This function runs on each
    // panel tick (twice, on the runtime refresh path), and five separate
    // `PRAGMA table_info` passes read the same table definition five times.
    let columns: Vec<String> = {
        let mut statement = conn.prepare("PRAGMA table_info(device_sync_secondary_config)").map_err(sql_err)?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        names
    };
    let optional = |name: &'static str| -> &'static str {
        if columns.iter().any(|column| column.as_str() == name) { name } else { "NULL" }
    };
    let sql = format!(
        "SELECT primary_id, device_id, origin, endpoint, server_name, certificate_der,
                certificate_pin, identity_path, stream_id, data_epoch, expected_generation,
                enabled, last_error, last_manifest_hash, last_gzip_hash,
                {}, {}, {}, {}, {}, {}
           FROM device_sync_secondary_config WHERE config_key = 'secondary'",
        optional("last_connected_at_ms"),
        optional("last_ack_generation"),
        optional("last_ack_receipt_hash"),
        optional("capture_enabled"),
        optional("primary_display_name"),
        optional("primary_os"),
    );
    let row: Option<(
        String, String, String, String, String, Vec<u8>, Vec<u8>, String,
        Option<String>, Option<i64>, Option<i64>, i64, Option<String>, Option<Vec<u8>>, Option<Vec<u8>>,
        Option<i64>, Option<i64>, Option<Vec<u8>>, Option<i64>, Option<String>, Option<String>,
    )> = conn.query_row(&sql, [], |row| Ok((
        row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
        row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?,
        row.get(10)?, row.get(11)?, row.get(12)?, row.get(13)?, row.get(14)?,
        row.get(15)?, row.get(16)?, row.get(17)?, row.get(18)?, row.get(19)?, row.get(20)?,
    ))).optional().map_err(sql_err)?;
    let Some((primary_id, device_id, origin, endpoint, server_name, certificate_der, pin, identity_path, stream_id, data_epoch, expected_generation, enabled, last_error, manifest_hash, gzip_hash, last_connected_at_ms, last_ack_generation, last_ack_receipt_hash, capture_enabled, primary_display_name, primary_os)) = row else {
        return Ok(None);
    };
    let last_ack_generation = last_ack_generation.map(|value| u64::try_from(value).map_err(|_| SyncError::Security("stored secondary ACK generation is negative".into()))).transpose()?;
    let last_ack_receipt_hash = last_ack_receipt_hash.map(|bytes| bytes.try_into().map_err(|_| SyncError::Security("stored secondary ACK receipt hash is not 32 bytes".into()))).transpose()?;
    let capture_enabled = match capture_enabled {
        Some(value) => value == 1,
        // Pre-capture-enabled tables had no way to pause local capture; keep
        // their historical behavior when read without a writer migration.
        None => true,
    };
    let certificate_pin: [u8; 32] = pin.try_into().map_err(|_| SyncError::Security("stored secondary certificate pin is not 32 bytes".into()))?;
    let hash = |value: Option<Vec<u8>>, label: &str| -> SyncResult<Option<[u8; 32]>> {
        value.map(|bytes| bytes.try_into().map_err(|_| SyncError::Security(format!("stored secondary {label} is not 32 bytes")))).transpose()
    };
    let data_epoch = data_epoch.map(|value| u64::try_from(value).map_err(|_| SyncError::Security("stored secondary data_epoch is negative".into()))).transpose()?;
    let expected_generation = expected_generation.map(|value| u64::try_from(value).map_err(|_| SyncError::Security("stored secondary expected_generation is negative".into()))).transpose()?;
    let config = SecondarySyncConfig {
        primary_id, primary_display_name, primary_os, device_id, origin, endpoint, server_name, certificate_der, certificate_pin,
        identity_path: PathBuf::from(identity_path), stream_id, data_epoch, expected_generation,
        enabled: enabled != 0, capture_enabled, last_error, last_manifest_hash: hash(manifest_hash, "manifest hash")?,
        last_gzip_hash: hash(gzip_hash, "gzip hash")?, last_connected_at_ms, last_ack_generation,
        last_ack_receipt_hash,
    };
    config.validate(&config.primary_id)?;
    Ok(Some(config))
}

fn device_id_from_public_key(public_key: &[u8; 32]) -> String {
    let digest = hash_bytes(public_key);
    let mut out = String::from("dev-");
    for byte in digest { out.push_str(&format!("{byte:02x}")); }
    out
}

fn sql_err(error: rusqlite::Error) -> SyncError {
    match error {
        rusqlite::Error::SqliteFailure(code, message) => SyncError::Sqlite(format!(
            "{} (sqlite_code={:?}, sqlite_extended_code={})",
            message.unwrap_or_else(|| "SQLite failure".into()),
            code.code,
            code.extended_code,
        )),
        other => SyncError::Sqlite(other.to_string()),
    }
}

fn read_status_conn(state: &Connection, index: &Connection) -> SyncResult<DeviceSyncSnapshot> {
    let primary_id = state
        .query_row("SELECT value FROM device_sync_meta WHERE key = 'primary_id'", [], |row| row.get(0))
        .optional()
        .map_err(sql_err)?;
    let mut ids = state
        .prepare("SELECT device_id FROM device_registry ORDER BY origin, device_id")
        .map_err(sql_err)?;
    let devices = ids
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sql_err)?
        .map(|id| {
            let id = id.map_err(sql_err)?;
            device_tx(state, index, &id)?
                .ok_or_else(|| SyncError::NotFound("device disappeared".into()))
                .map(DeviceStatusView::from)
        })
        .collect::<SyncResult<Vec<_>>>()?;
    let mut requests = state
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

/// Registry fields come from the state database; the row-era stream progress
/// reader is gone with the row-era streams, so the stream/progress fields
/// answer empty. `index` is today unused but keeps the reader shape the status
/// bridge and registry callers were written against.
fn device_tx(state: &Connection, _index: &Connection, device_id: &str) -> SyncResult<Option<DeviceRecord>> {
    // The read-only status probe skips migrations, so a state database from
    // before the merge-recency columns must still answer — with NULL/false,
    // which is the truth: nothing could stamp them before the columns
    // existed, and the next read-write open backfills them via ensure_column.
    let has_recency: bool = state.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('device_registry') WHERE name = 'last_merged_at_ms')",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    let row: Option<(String, String, String, Vec<u8>, String, i64, Option<i64>, i64)> = state.query_row(
        &format!(
            "SELECT origin, display_name, os, public_key, status, data_epoch, {}, {} FROM device_registry WHERE device_id = ?1",
            if has_recency { "last_merged_at_ms" } else { "NULL" },
            if has_recency { "snapshot_complete" } else { "0" },
        ),
        params![device_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get::<_, Option<i64>>(6)?, row.get::<_, i64>(7)?)),
    ).optional().map_err(sql_err)?;
    let Some((origin, display_name, os, public_key, status, data_epoch, last_merged_at_ms, snapshot_complete)) = row else { return Ok(None); };
    let public_key: [u8; 32] = public_key.as_slice().try_into().map_err(|_| SyncError::Security("registered public key length is invalid".into()))?;
    let status = DeviceStatus::parse(&status)?;
    Ok(Some(DeviceRecord {
        device_id: device_id.to_string(), origin, display_name, os, public_key,
        role: DeviceRole::Secondary, status, data_epoch: data_epoch as u64,
        active_stream_id: None, last_generation: None, last_merged_at_ms, snapshot_complete: snapshot_complete != 0,
    }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DeviceIdentity;
    use usage_core::{DateFilter, DetectedSource, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile, UsageEvent};

    fn identity(origin: &str) -> DeviceIdentity {
        DeviceIdentity::create(origin, origin).unwrap()
    }

    #[test]
    fn steal_stale_panel_lease_reclaims_only_this_devices_killed_panel() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SyncStore::open(&dir.path().join("index.db"), "primary-1").unwrap();
        let device = "dev-abc-1111111111111111111111111111111111111111111111111111111111111111";
        let killed = format!("panel-secondary-{device}-deadbeef");
        let fresh = format!("panel-secondary-{device}-fresh");
        // A killed panel's lease still inside its TTL blocks the fresh runtime.
        assert!(store.acquire_scheduler_lease(&killed, 1_000, 900_000).unwrap());
        assert!(!store.acquire_scheduler_lease(&fresh, 2_000, 900_000).unwrap());
        assert!(store.steal_stale_panel_lease(device).unwrap(), "the killed panel's lease is reclaimable inside its TTL");
        assert!(store.acquire_scheduler_lease(&fresh, 3_000, 900_000).unwrap(), "the fresh panel acquires immediately after the steal");

        // A foreign owner is never stolen: a CLI runtime or another device's
        // panel may be genuinely alive and keeps its wait-out semantics.
        store.release_scheduler_lease(&fresh).unwrap();
        assert!(store.acquire_scheduler_lease("tokenme-cli-someone-else", 4_000, 900_000).unwrap());
        assert!(!store.acquire_scheduler_lease(&fresh, 5_000, 900_000).unwrap());
        assert!(!store.steal_stale_panel_lease(device).unwrap(), "a foreign CLI holder is left alone");
        store.release_scheduler_lease("tokenme-cli-someone-else").unwrap();
        let other_panel = "panel-secondary-dev-OTHER-2222222222222222222222222222222222222222222222222222222222222222-x";
        assert!(store.acquire_scheduler_lease(other_panel, 6_000, 900_000).unwrap());
        assert!(!store.steal_stale_panel_lease(device).unwrap(), "another device's panel lease is left alone");
    }

    #[test]
    fn auto_approve_pairing_approves_on_arrival_and_only_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SyncStore::open(&dir.path().join("index.db"), "primary-1").unwrap();
        let device = identity("auto-ok");
        let invite = store.create_pairing_invite("https://localhost:1", "0000000000000000000000000000000000000000000000000000000000000000", i64::MAX).unwrap();
        let request = PairingRequest::new(&invite, &device, "linux").unwrap();
        store.save_primary_config(&PrimarySyncConfig {
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(), advertise_host: Some("127.0.0.1".into()),
            allow_lan: false, enabled: true, share_data_with_secondaries: false, auto_approve_pairing: true,
        }, 1).unwrap();
        let state = store.receive_pairing_request(&request, 2).unwrap();
        assert_eq!(state.status, "paired", "the request answers approved without a manual tap");
        let devices = store.devices().unwrap();
        assert_eq!(devices[0].status, DeviceStatus::Paired);

        // The same primary with the flag off keeps the manual queue.
        let other = identity("auto-off");
        let invite = store.create_pairing_invite("https://localhost:1", "0000000000000000000000000000000000000000000000000000000000000000", i64::MAX).unwrap();
        let request = PairingRequest::new(&invite, &other, "linux").unwrap();
        store.set_primary_auto_approve(false, 4).unwrap();
        let state = store.receive_pairing_request(&request, 5).unwrap();
        assert_eq!(state.status, "pending");
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
    fn authenticated_display_name_refresh_changes_only_the_label() {
        let device = identity("stable-origin");
        let mut store = paired_store_for(&device);
        let mut forged = AuthenticatedDisplayNameRequest::new_at(&device, "primary-1", "New host", 10).unwrap();
        forged.display_name = "forged host".into();
        assert!(matches!(store.update_display_name(&forged, 20), Err(SyncError::Auth(_))));
        let stale = AuthenticatedDisplayNameRequest::new_at(&device, "primary-1", "Stale host", 10).unwrap();
        assert!(matches!(store.update_display_name(&stale, 10 + crate::protocol::MAX_AUTH_REQUEST_AGE_MS + 1), Err(SyncError::Auth(_))));

        let request = AuthenticatedDisplayNameRequest::new_at(&device, "primary-1", "New host", 10).unwrap();
        store.update_display_name(&request, 20).unwrap();
        let updated = store.device(&device.device_id).unwrap().unwrap();
        assert_eq!(updated.device_id, device.device_id);
        assert_eq!(updated.origin, "stable-origin");
        assert_eq!(updated.display_name, "New host");
    }

    fn paired_store_for(device: &DeviceIdentity) -> SyncStore {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let invite = store.create_pairing_invite("https://lan:47821", "1111111111111111111111111111111111111111111111111111111111111111", 10_000).unwrap();
        let request = PairingRequest::new(&invite, device, "linux").unwrap();
        let request = store.receive_pairing_request(&request, 1).unwrap();
        store.approve_pairing(&request.request_id, 2).unwrap();
        store
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
    fn persisted_and_dynamic_expired_pairing_requests_can_be_cleared_but_not_approved() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let dynamic_invite = store.create_pairing_invite("https://localhost:47821", &"6".repeat(64), 10).unwrap();
        let dynamic_device = identity("dynamic-expired");
        let dynamic_request = PairingRequest::new(&dynamic_invite, &dynamic_device, "linux").unwrap();
        store.receive_pairing_request(&dynamic_request, 1).unwrap();
        assert_eq!(store.pairing_requests_at(10).unwrap().iter().find(|request| request.request_id == dynamic_request.request_id).unwrap().status, "expired");
        store.clear_pairing_request(&dynamic_request.request_id, 10).unwrap();
        assert_eq!(store.pairing_requests_at(10).unwrap().iter().find(|request| request.request_id == dynamic_request.request_id).unwrap().status, "rejected");
        assert!(matches!(store.approve_pairing(&dynamic_request.request_id, 10), Err(SyncError::Conflict(message)) if message.contains("rejected")));

        let persisted_invite = store.create_pairing_invite("https://localhost:47821", &"7".repeat(64), 100).unwrap();
        let persisted_device = identity("persisted-expired");
        let persisted_request = PairingRequest::new(&persisted_invite, &persisted_device, "linux").unwrap();
        store.receive_pairing_request(&persisted_request, 1).unwrap();
        store.state.execute("UPDATE device_pairing_request SET status = 'expired' WHERE request_id = ?1", params![persisted_request.request_id]).unwrap();
        store.clear_pairing_request(&persisted_request.request_id, 2).unwrap();
        assert_eq!(store.pairing_requests_at(2).unwrap().iter().find(|request| request.request_id == persisted_request.request_id).unwrap().status, "rejected");
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
    fn reset_discards_stream_state_but_keeps_local_events_and_forces_recovery() {
        let device = identity("reset-secondary");
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        store.index().conn().execute(
            "INSERT INTO event(tool, ts_ms, session, meter, in_tok, source, dedupe_key)
             VALUES('codex', 10, 'session', 'tokens', 1, 'local.jsonl', 'reset-key')",
            [],
        ).unwrap();
        let certificate_der = vec![1u8, 2, 3];
        let config = SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: certificate_der.clone(),
            certificate_pin: hash_bytes(&certificate_der), identity_path: PathBuf::from("identity.json"),
            stream_id: Some("reset-old".into()), data_epoch: Some(1), expected_generation: Some(2), enabled: true, capture_enabled: true,
            last_error: Some("stuck".into()), last_manifest_hash: Some([1; 32]), last_gzip_hash: Some([2; 32]), last_connected_at_ms: None,
            last_ack_generation: Some(1), last_ack_receipt_hash: Some([9; 32]),
        };
        store.save_secondary_config(&config, 1).unwrap();
        assert!(!store.secondary_needs_recovery().unwrap());

        let summary = store.reset_secondary_sync_state(20).unwrap();
        assert_eq!(summary.device_id, device.device_id);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM event", [], |r| r.get(0)).unwrap(), 1,
            "a reset re-ships local data, it never deletes it");
        let reset = store.secondary_config().unwrap().unwrap();
        assert_eq!((reset.stream_id.as_deref(), reset.data_epoch, reset.expected_generation), (None, None, None));
        assert_eq!(reset.last_ack_generation, None);
        assert_eq!(reset.last_ack_receipt_hash, None);
        assert_eq!(reset.last_manifest_hash, None);
        assert_eq!(reset.last_gzip_hash, None);
        assert_eq!(reset.last_error, None);
        assert!(store.secondary_needs_recovery().unwrap(), "the old session's checkpoints are gone; the next pass re-establishes the session");
    }

    /// Pairing to a DIFFERENT primary rotates the sync state but used to leave
    /// the old primary's downloaded rows in place: their dedupe keys embed the
    /// old primary id (`network_event_key`), so the new primary re-downloads
    /// the same origin data under fresh keys and every origin double-counted
    /// in the report. The rotate now deletes the downloaded rows (and their
    /// calls) with the rest of the old role and recomputes the touched days,
    /// the way the import path does.
    #[test]
    fn rotating_to_a_new_primary_deletes_the_rows_the_old_one_downloaded() {
        let device = identity("rotate-download");
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let downloaded_key = "device-sync:v1:4c6f72656d"; // the row-era key shape the rotate purges
        store.index().conn().execute(
            "INSERT INTO event(tool, ts_ms, session, meter, in_tok, source, dedupe_key)
             VALUES('codex', 10, 'remote-session', 'tokens', 5, 'linux:remote-host:remote-row', ?1)",
            params![downloaded_key],
        ).unwrap();
        let downloaded_id = store.index().conn().last_insert_rowid();
        store.index().conn().execute(
            "INSERT INTO call(event_id, kind, name) VALUES(?1, 'mcp', 'remote-tool')",
            params![downloaded_id],
        ).unwrap();
        store.index().conn().execute(
            "INSERT INTO event(tool, ts_ms, session, meter, in_tok, source, dedupe_key)
             VALUES('codex', 10, 'local-session', 'tokens', 7, 'local.jsonl', 'local-row')",
            [],
        ).unwrap();
        store.index_mut().with_network_transaction(|tx| usage_index::recompute_network_days(tx, &[10])).unwrap();
        let remote_groups: i64 = store.index().conn().query_row(
            "SELECT count(*) FROM event_rollup WHERE origin = 'remote-host'", [], |row| row.get(0),
        ).unwrap();
        assert!(remote_groups > 0, "precondition: the downloaded rows reach the report through the rollup");

        store.transition_to_secondary(&SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-2".into(), primary_display_name: None, device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: vec![1u8, 2, 3],
            certificate_pin: hash_bytes(&[1u8, 2, 3]), identity_path: PathBuf::from("identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }, 20).unwrap();
        assert_eq!(store.primary_id(), "primary-2");
        assert_eq!(store.index().conn().query_row::<i64, _, _>(
            "SELECT count(*) FROM event WHERE dedupe_key LIKE 'device-sync:v1:%'", [], |row| row.get(0),
        ).unwrap(), 0, "the old primary's downloaded rows must not survive the rotate");
        assert_eq!(store.index().conn().query_row::<i64, _, _>(
            "SELECT count(*) FROM call WHERE event_id = ?1", params![downloaded_id], |row| row.get(0),
        ).unwrap(), 0, "the downloaded rows' calls go with them");
        assert_eq!(store.index().conn().query_row::<i64, _, _>(
            "SELECT count(*) FROM event WHERE dedupe_key = 'local-row'", [], |row| row.get(0),
        ).unwrap(), 1, "local usage never leaves");
        assert_eq!(store.index().conn().query_row::<i64, _, _>(
            "SELECT count(*) FROM event_rollup WHERE origin = 'remote-host'", [], |row| row.get(0),
        ).unwrap(), 0, "the recomputed day no longer bills the remote origin");
        assert!(store.index().conn().query_row::<i64, _, _>(
            "SELECT count(*) FROM event_rollup WHERE origin = ''", [], |row| row.get(0),
        ).unwrap() > 0, "the local group of the touched day stays");
    }

    #[test]
    fn reset_without_a_binding_is_an_explicit_error_and_repeatable() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        assert!(matches!(store.reset_secondary_sync_state(1), Err(SyncError::NotFound(_))));
        let device = identity("reset-repeat");
        let device_id = device.device_id.clone();
        let certificate_der = vec![1u8, 2, 3];
        store.save_secondary_config(&SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: device.device_id, origin: device.origin.clone(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: certificate_der.clone(),
            certificate_pin: hash_bytes(&certificate_der), identity_path: PathBuf::from("identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }, 1).unwrap();
        let summary = store.reset_secondary_sync_state(2).unwrap();
        assert_eq!(summary.device_id, device_id);
        store.reset_secondary_sync_state(3).unwrap();
        assert!(store.secondary_needs_recovery().unwrap());
    }

    #[test]
    fn late_disconnect_notice_detaches_only_when_the_binding_is_gone() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let certificate_der = vec![1u8, 2, 3];
        let make_config = |device_id: &str, origin: &str| SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: device_id.into(), origin: origin.into(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: certificate_der.clone(),
            certificate_pin: hash_bytes(&certificate_der), identity_path: PathBuf::from("identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        };
        store.save_secondary_config(&make_config("device-old", "old-laptop"), 1).unwrap();
        store.queue_secondary_disconnect(&make_config("device-old", "old-laptop"), 2).unwrap();
        store.save_secondary_config(&make_config("device-new", "new-laptop"), 3).unwrap();
        store.index().conn().execute(
            "INSERT INTO remote_rollup(day, origin, tool, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts)
             VALUES('2026-10-01', 'old-laptop', 'codex', 's', '', '', 'tokens', 3, 0, 0, 0, 0, 0, 1, 1, 1, 2)",
            [],
        ).unwrap();

        store.finish_disconnect_notice("device-old", 4, None).unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM remote_rollup", [], |r| r.get(0)).unwrap(), 1,
            "a notice that outlived a re-pair must not wipe the new binding's aggregate rows");

        store.queue_secondary_disconnect(&make_config("device-new", "new-laptop"), 5).unwrap();
        store.finish_disconnect_notice("device-new", 6, None).unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM remote_rollup", [], |r| r.get(0)).unwrap(), 0,
            "with no binding left the peer's aggregate rows leave the figures");
    }

    #[test]
    fn migration_is_additive_and_does_not_clear_existing_event() {
        let index = Index::open_in_memory().unwrap();
        index.conn().execute("INSERT INTO event(tool, ts_ms, session, meter, in_tok, source) VALUES('local', 1, 's', 'tokens', 1, 'local')", []).unwrap();
        let store = SyncStore::new(index, "primary-1").unwrap();
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM event", [], |r| r.get(0)).unwrap(), 1);
        assert_eq!(store.index().conn().query_row::<String, _, _>("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).unwrap(), usage_index::INDEX_SCHEMA_VERSION);
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'device_sync_vector_candidate'", [], |r| r.get(0)).unwrap(), 0);
        assert_eq!(store.state.query_row::<String, _, _>("SELECT value FROM device_sync_meta WHERE key='migration_version'", [], |r| r.get(0)).unwrap(), STATE_VERSION.to_string());
    }

    #[test]
    fn secondary_last_connected_is_added_to_an_old_table_and_persisted() {
        let index = Index::open_in_memory().unwrap();
        let state = Connection::open_in_memory().unwrap();
        let certificate = vec![1u8, 2, 3];
        let pin = hash_bytes(&certificate);
        state.execute_batch(
            "CREATE TABLE device_sync_secondary_config(
                 config_key TEXT PRIMARY KEY, primary_id TEXT NOT NULL, device_id TEXT NOT NULL,
                 origin TEXT NOT NULL, endpoint TEXT NOT NULL, server_name TEXT NOT NULL,
                 certificate_der BLOB NOT NULL, certificate_pin BLOB NOT NULL, identity_path TEXT NOT NULL,
                 stream_id TEXT, data_epoch INTEGER, expected_generation INTEGER, enabled INTEGER NOT NULL,
                 last_error TEXT, last_manifest_hash BLOB, last_gzip_hash BLOB, updated_at_ms INTEGER NOT NULL
             );",
        ).unwrap();
        state.execute(
            "INSERT INTO device_sync_secondary_config(
                 config_key, primary_id, device_id, origin, endpoint, server_name,
                 certificate_der, certificate_pin, identity_path, enabled, updated_at_ms
             ) VALUES('secondary', 'primary-1', 'device-1', 'laptop', 'https://localhost:1',
                      'localhost', x'010203', ?1, 'identity.json', 1, 1)",
            [pin.as_slice()],
        ).unwrap();
        let mut store = SyncStore::new_with_state(index, state, "primary-1").unwrap();
        let migrated = store.secondary_config().unwrap().unwrap();
        assert_eq!(migrated.last_connected_at_ms, None);
        assert_eq!(migrated.primary_display_name, None);
        assert!(migrated.capture_enabled, "old secondary configs default to capture-on");
        assert_eq!(store.state.query_row::<i64, _, _>(
            "SELECT capture_enabled FROM device_sync_secondary_config WHERE config_key = 'secondary'", [], |r| r.get(0),
        ).unwrap(), 1);
        store.mark_secondary_connected(42).unwrap();
        assert_eq!(store.secondary_config().unwrap().unwrap().last_connected_at_ms, Some(42));
        store.set_secondary_primary_display_name("Primary Mac", 43).unwrap();
        assert_eq!(store.secondary_config().unwrap().unwrap().primary_display_name.as_deref(), Some("Primary Mac"));
    }

    fn legacy_primary_state(sharing: i64, current: Option<i64>) -> (Index, Connection) {
        let index = Index::open_in_memory().unwrap();
        index.conn().execute(
            "INSERT INTO event(tool, ts_ms, session, meter, in_tok, source)
             VALUES('local', 1, 'session', 'tokens', 3, 'local')",
            [],
        ).unwrap();
        let state = Connection::open_in_memory().unwrap();
        state.execute_batch(
            "CREATE TABLE device_sync_primary_config(
                 config_key TEXT PRIMARY KEY, primary_id TEXT NOT NULL, bind_addr TEXT NOT NULL,
                 advertise_host TEXT, allow_lan INTEGER NOT NULL, allow_secondary_read INTEGER NOT NULL DEFAULT 0,
                 enabled INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
             );",
        ).unwrap();
        state.execute(
            "INSERT INTO device_sync_primary_config VALUES('primary', 'primary-1', '127.0.0.1:47821', '127.0.0.1', 0, ?1, 1, 10)",
            [sharing],
        ).unwrap();
        if let Some(value) = current {
            state.execute_batch("ALTER TABLE device_sync_primary_config ADD COLUMN share_data_with_secondaries INTEGER NOT NULL DEFAULT 0").unwrap();
            state.execute("UPDATE device_sync_primary_config SET share_data_with_secondaries = ?1", [value]).unwrap();
        }
        (index, state)
    }

    #[test]
    fn legacy_primary_sharing_is_read_and_migrated_without_resetting_consent() {
        for enabled in [false, true] {
            let (index, state) = legacy_primary_state(if enabled { 1 } else { 0 }, None);
            assert_eq!(read_primary_config_conn(&state).unwrap().unwrap().share_data_with_secondaries, enabled);
            let mut store = SyncStore::new_with_state(index, state, "primary-1").unwrap();
            assert_eq!(store.primary_config().unwrap().unwrap().share_data_with_secondaries, enabled);
            assert_eq!(store.index().event_count().unwrap(), 1);
            assert_eq!(primary_sharing_columns(&store.state).unwrap(), (true, false));
            store.set_primary_data_sharing(false, 20).unwrap();
            migrate_state(&mut store.state, "primary-1").unwrap();
            assert!(!store.primary_config().unwrap().unwrap().share_data_with_secondaries);
        }
    }

    #[test]
    fn current_primary_sharing_off_wins_over_an_obsolete_enabled_value() {
        let (index, state) = legacy_primary_state(1, Some(0));
        assert!(!read_primary_config_conn(&state).unwrap().unwrap().share_data_with_secondaries);
        let store = SyncStore::new_with_state(index, state, "primary-1").unwrap();
        assert!(!store.primary_config().unwrap().unwrap().share_data_with_secondaries);
        assert_eq!(primary_sharing_columns(&store.state).unwrap(), (true, false));
        assert_eq!(store.index().event_count().unwrap(), 1);
    }

    #[test]
    fn corrupt_legacy_primary_sharing_does_not_become_enabled() {
        let (index, state) = legacy_primary_state(2, None);
        assert!(matches!(read_primary_config_conn(&state), Err(SyncError::Security(_))));
        let store = SyncStore::new_with_state(index, state, "primary-1").unwrap();
        assert!(matches!(store.primary_config(), Err(SyncError::Security(_))));
    }

    #[test]
    fn primary_config_accepts_the_legacy_json_setting() {
        let config: PrimarySyncConfig = serde_json::from_value(serde_json::json!({
            "primary_id": "primary-1", "bind_addr": "127.0.0.1:47821", "advertise_host": "127.0.0.1",
            "allow_lan": false, "enabled": true, "allow_secondary_read": true,
        })).unwrap();
        assert!(config.share_data_with_secondaries);
        let saved = serde_json::to_value(config).unwrap();
        assert_eq!(saved["share_data_with_secondaries"], true);
        assert!(saved.get("allow_secondary_read").is_none());
    }

    #[test]
    fn primary_config_round_trips_and_stop_preserves_sync_history() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        store.index().conn().execute(
            "INSERT INTO event(tool, ts_ms, session, meter, in_tok, source) VALUES('local', 1, 'session', 'tokens', 3, 'local')",
            [],
        ).unwrap();
        let config = PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(),
            bind_addr: "127.0.0.1:47821".into(),
            advertise_host: Some("127.0.0.1".into()),
            allow_lan: false,
            enabled: true,
            share_data_with_secondaries: false,
        };
        store.save_primary_config(&config, 10).unwrap();
        assert_eq!(store.primary_config().unwrap(), Some(config.clone()));
        store.set_primary_data_sharing(true, 11).unwrap();
        assert!(store.primary_config().unwrap().unwrap().share_data_with_secondaries);
        store.set_primary_enabled(false, 11).unwrap();
        let stopped = store.primary_config().unwrap().unwrap();
        assert!(!stopped.enabled);
        assert!(stopped.share_data_with_secondaries);
        assert_eq!(stopped.primary_id, config.primary_id);
        store.clear_primary_config(12).unwrap();
        assert!(store.primary_config().unwrap().is_none());
        assert_eq!(store.index().event_count().unwrap(), 1, "stopping the listener must not clear local history");
    }

    fn secondary_config_fixture(dir: &std::path::Path, origin: &str) -> SecondarySyncConfig {
        let device = identity(origin);
        let certificate_der = vec![7_u8; 32];
        SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None,
            device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: "https://127.0.0.1:1".into(), server_name: "localhost".into(),
            certificate_pin: hash_bytes(&certificate_der), certificate_der,
            identity_path: dir.join("identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None,
            enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }
    }

    #[test]
    fn read_detached_origins_tracks_the_downlink_origin_through_disconnect_and_repair() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        // No sync state at all: the read is empty, not an error.
        assert!(SyncStore::read_detached_origins(&path).is_empty());

        let mut secondary = SyncStore::open(&path, "primary-1").unwrap();
        let config = secondary_config_fixture(dir.path(), "downloader");
        secondary.save_secondary_config(&config, 2).unwrap();
        // A live, enabled pairing: nothing is detached.
        assert!(SyncStore::read_detached_origins(&path).is_empty());

        // The runtime's self-disable on an error flips the enabled bit — with
        // no origin recorded yet there is nothing to detach.
        secondary.set_secondary_enabled(false, 3).unwrap();
        assert!(SyncStore::read_detached_origins(&path).is_empty());

        // A pairing that never recorded the key still detaches at disconnect:
        // the notice recovers the origin from the merged rows' `linux:<origin>:<key>`
        // sources.
        secondary.index().conn().execute(
            "INSERT INTO event(tool, ts_ms, session, meter, in_tok, source, dedupe_key)
             VALUES('codex', 10, 'their-session', 'tokens', 7, 'linux:MacBook-Pro:their-key', 'their-key')",
            [],
        ).unwrap();
        secondary.queue_secondary_disconnect(&config, 4).unwrap();
        assert_eq!(SyncStore::read_detached_origins(&path), vec!["MacBook-Pro".to_string()]);
        assert_eq!(secondary.index().conn().query_row::<String, _, _>(
            "SELECT value FROM meta WHERE key = 'device_sync_downlink_origin'", [], |row| row.get(0),
        ).unwrap(), "MacBook-Pro");

        // Re-pairing live again restores the figures losslessly.
        secondary.save_secondary_config(&config, 5).unwrap();
        assert!(SyncStore::read_detached_origins(&path).is_empty());
    }

    #[test]
    fn read_detached_origins_lists_revoked_devices_and_keeps_paused_counting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mut primary = SyncStore::open(&path, "primary-1").unwrap();
        let revoked = identity("departed-host");
        let paused = identity("paused-host");
        for device in [&revoked, &paused] {
            let invite = primary.create_pairing_invite("https://localhost:1", "0000000000000000000000000000000000000000000000000000000000000000", i64::MAX).unwrap();
            let request = PairingRequest::new(&invite, device, "linux").unwrap();
            let state = primary.receive_pairing_request(&request, 1).unwrap();
            primary.approve_pairing(&state.request_id, 2).unwrap();
        }
        primary.pause_device(&paused.device_id).unwrap();
        assert!(SyncStore::read_detached_origins(&path).is_empty(), "a paused device keeps counting — offline, not unsubscribed");
        primary.revoke_device(&revoked.device_id, 3).unwrap();
        assert_eq!(SyncStore::read_detached_origins(&path), vec![revoked.origin.clone()]);
        // A state file that does not exist reads empty, not an error. The
        // state path is directory-keyed (sync_state_path), so "another db"
        // must live in another directory to actually be absent.
        assert!(SyncStore::read_detached_origins(dir.path().join("absent").join("other.db")).is_empty());
    }

    #[test]
    fn primary_config_rejects_corrupt_boolean_flags() {
        let store = SyncStore::open_in_memory("primary-1").unwrap();
        store.state.execute(
            "INSERT INTO device_sync_primary_config(
                 config_key, primary_id, bind_addr, advertise_host, allow_lan, enabled, updated_at_ms
             ) VALUES('primary', 'primary-1', '127.0.0.1:47821', '127.0.0.1', 2, 1, 1)",
            [],
        ).unwrap();
        assert!(matches!(store.primary_config(), Err(SyncError::Security(message)) if message.contains("allow_lan")));
    }

    #[test]
    fn share_toggle_writes_state_without_ever_opening_the_usage_index() {
        let dir = tempfile::tempdir().unwrap();
        // A populated primary state, but deliberately NO index.db on disk: the
        // state-only write path must never touch the usage index, which is the
        // whole point of the split (the ingest engine can hold that file's
        // write lock for seconds).
        {
            let mut store = SyncStore::open(dir.path().join("index.db"), "primary-1").unwrap();
            store.save_primary_config(&crate::PrimarySyncConfig {
                auto_approve_pairing: false,
                primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
                advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: true,
                share_data_with_secondaries: false,
            }, 1).unwrap();
        }
        std::fs::remove_file(dir.path().join("index.db")).unwrap();
        std::fs::remove_file(dir.path().join("index.db-wal")).ok();
        std::fs::remove_file(dir.path().join("index.db-shm")).ok();
        std::fs::remove_file(dir.path().join("index.device-sync-state")).ok();

        SyncStore::write_primary_data_sharing(dir.path().join("index.db"), "primary-1", true, 2).unwrap();
        let config = SyncStore::read_primary_config(dir.path().join("index.db")).unwrap().unwrap();
        assert!(config.share_data_with_secondaries, "the state-only write lands in sync_state.db");
        assert!(!dir.path().join("index.db").exists(), "the usage index is never created by the toggle path");

        let missing = SyncStore::write_primary_data_sharing(dir.path().join("elsewhere.db"), "primary-9", true, 3);
        assert!(matches!(missing, Err(SyncError::NotFound(_)) | Err(SyncError::Security(_))), "a foreign primary id is refused, not silently applied: {missing:?}");
    }

    #[test]
    fn primary_stop_clears_state_without_the_usage_index() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = SyncStore::open(dir.path().join("index.db"), "primary-1").unwrap();
            store.save_primary_config(&PrimarySyncConfig {
                auto_approve_pairing: false,
                primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
                advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: true,
                share_data_with_secondaries: true,
            }, 1).unwrap();
        }
        for sidecar in ["index.db", "index.db-wal", "index.db-shm", "index.device-sync-state"] {
            std::fs::remove_file(dir.path().join(sidecar)).ok();
        }
        // The stop button's state-only teardown must succeed with the usage
        // index gone: it never opens that file.
        SyncStore::clear_primary_config_state(dir.path().join("index.db"), "primary-1").unwrap();
        assert!(SyncStore::read_primary_config(dir.path().join("index.db")).unwrap().is_none(), "the config row is gone from sync_state.db");
        assert!(!dir.path().join("index.db").exists(), "the usage index is never created by the stop path");
    }

    #[test]
    fn secondary_stop_clears_state_without_the_usage_index() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = SyncStore::open(dir.path().join("index.db"), "primary-1").unwrap();
            store.save_secondary_config(&crate::SecondarySyncConfig {
                primary_os: None,
                primary_id: "primary-1".into(), primary_display_name: Some("primary-1".into()),
                device_id: "device-1".into(), origin: "SP-DESGIN".into(),
                endpoint: "https://127.0.0.1:47821".into(), server_name: "localhost".into(),
                certificate_der: vec![0x00; 8], certificate_pin: crate::protocol::hash_bytes(&[0x00; 8]),
                identity_path: dir.path().join("secondary-identity.json"),
                stream_id: None, data_epoch: None, expected_generation: None,
                enabled: true, capture_enabled: true,
                last_error: None, last_manifest_hash: None, last_gzip_hash: None,
                last_connected_at_ms: None, last_ack_generation: None, last_ack_receipt_hash: None,
            }, 1).unwrap();
        }
        for sidecar in ["index.db", "index.db-wal", "index.db-shm", "index.device-sync-state"] {
            std::fs::remove_file(dir.path().join(sidecar)).ok();
        }
        // The secondary stop's state-only seam mirrors the primary's: the
        // binding row goes away without the usage index ever being opened.
        SyncStore::clear_secondary_config_state(dir.path().join("index.db"), "primary-1", "device-1").unwrap();
        assert!(SyncStore::read_secondary_config(dir.path().join("index.db")).unwrap().is_none(), "the binding row is gone from sync_state.db");
        assert!(!dir.path().join("index.db").exists(), "the usage index is never created by the secondary stop path");
    }

    #[test]
    fn primary_start_persists_its_config_without_the_usage_index() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = SyncStore::open(dir.path().join("index.db"), "primary-1").unwrap();
            store.save_primary_config(&PrimarySyncConfig {
                auto_approve_pairing: false,
                primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
                advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: true,
                share_data_with_secondaries: false,
            }, 1).unwrap();
        }
        for sidecar in ["index.db", "index.db-wal", "index.db-shm", "index.device-sync-state"] {
            std::fs::remove_file(dir.path().join(sidecar)).ok();
        }
        // The start path records the actually-bound listener through the same
        // state-only seam: it must never open the usage index, or starting the
        // primary queues behind the ingest engine's write lock.
        SyncStore::write_primary_config_state(dir.path().join("index.db"), "primary-1", &PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47822".into(),
            advertise_host: Some("192.168.101.66".into()), allow_lan: true, enabled: true,
            share_data_with_secondaries: true,
        }, 2).unwrap();
        let config = SyncStore::read_primary_config(dir.path().join("index.db")).unwrap().unwrap();
        assert_eq!(config.bind_addr, "127.0.0.1:47822", "the upsert replaces the remembered endpoint");
        assert!(config.allow_lan && config.share_data_with_secondaries);
        assert!(!dir.path().join("index.db").exists(), "the usage index is never created by the start's persist path");
    }

    #[test]
    fn deferred_open_leaves_index_maintenance_to_maintain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mirror = |store: &SyncStore| -> String {
            store.index().conn().query_row(
                "SELECT value FROM meta WHERE key = 'device_sync_capture_enabled'", [], |row| row.get(0),
            ).unwrap()
        };
        let mut store = SyncStore::open(&path, "primary-1").unwrap();
        store.save_primary_config(&PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
            advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: true,
            share_data_with_secondaries: true,
        }, 1).unwrap();
        let fresh = mirror(&store);
        assert_eq!(fresh, "0", "with no paired device the mirror starts disabled");
        // Pair a device straight into the state registry, the way an approved
        // request would: the next full open would refresh the mirror to 1, and
        // that refresh is an index write transaction the UI path must not pay
        // for.
        drop(store);
        let state = rusqlite::Connection::open(sync_state_path(&path).unwrap()).unwrap();
        state.execute(
            "INSERT INTO device_registry(device_id, origin, display_name, os, public_key, status, data_epoch, created_at_ms)
             VALUES('device-1', 'SP-DESGIN', 'SP-DESGIN', 'windows', X'00', 'paired', 1, 1)", [],
        ).unwrap();
        drop(state);

        let mut deferred = SyncStore::open_deferred(&path, "primary-1").unwrap();
        assert_eq!(mirror(&deferred), fresh, "open_deferred keeps the stale mirror instead of writing the index");
        deferred.maintain().unwrap();
        assert_ne!(mirror(&deferred), fresh, "maintain converges the mirror once the caller can afford the write");
    }

    #[test]
    fn migrate_recreates_a_dropped_device_table_through_the_read_only_probe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let store = SyncStore::open(&path, "primary-1").unwrap();
            store.index().conn().execute("DROP TABLE device_sync_scheduler_lease", []).unwrap();
        }
        let store = SyncStore::open(&path, "primary-1").unwrap();
        let exists: i64 = store.index().conn().query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'device_sync_scheduler_lease'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(exists, 1, "the probe sees the hole and the DDL batch refills it");
    }

    #[test]
    fn read_status_answers_a_registry_that_predates_the_merge_recency_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let store = SyncStore::open(&path, "primary-1").unwrap();
            store.state.execute(
                "INSERT INTO device_registry(device_id, origin, display_name, os, public_key, status, data_epoch, created_at_ms)
                 VALUES('dev-old', 'lab-old', 'Old Lab', 'Linux', zeroblob(32), 'paired', 0, 1)", [],
            ).unwrap();
            store.state.execute_batch(
                "ALTER TABLE device_registry DROP COLUMN last_merged_at_ms;
                 ALTER TABLE device_registry DROP COLUMN snapshot_complete;",
            ).unwrap();
        }
        let status = SyncStore::read_status(&path).unwrap();
        assert_eq!(status.devices.len(), 1, "the pre-column registry still answers the read-only probe");
        assert_eq!(status.devices[0].last_merged_at_ms, None, "nothing could stamp a merge before the columns existed");
        assert!(!status.devices[0].snapshot_complete, "same for the completeness flag");
    }

    #[test]
    fn migrate_sheds_the_orphan_last_sync_column_from_the_duplicate_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let store = SyncStore::open(&path, "primary-1").unwrap();
            store.state.execute("ALTER TABLE device_registry ADD COLUMN last_sync_at_ms INTEGER", []).unwrap();
        }
        let store = SyncStore::open(&path, "primary-1").unwrap();
        let columns = store.state.prepare("SELECT name FROM pragma_table_info('device_registry')").unwrap()
            .query_map([], |row| row.get::<_, String>(0)).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap();
        assert!(!columns.contains(&"last_sync_at_ms".into()), "the orphan column is dropped, not kept beside the real stamps");
        assert!(columns.contains(&"last_merged_at_ms".into()) && columns.contains(&"snapshot_complete".into()));
    }

    #[test]
    fn read_secondary_disconnect_requested_answers_without_a_writable_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let store = SyncStore::open(&path, "primary-1").unwrap();
            store.index().conn().execute(
                "INSERT INTO meta(key, value) VALUES('device_sync_disconnect_requested', 'primary-1')", [],
            ).unwrap();
        }
        assert!(SyncStore::read_secondary_disconnect_requested(&path, "primary-1").unwrap(), "the flag answers for its own primary");
        assert!(!SyncStore::read_secondary_disconnect_requested(&path, "primary-2").unwrap(), "another primary's flag is not this secondary's business");
        assert!(!dir.path().join("missing.db").exists(), "sanity: the absent path is really absent");
        assert!(!SyncStore::read_secondary_disconnect_requested(dir.path().join("missing.db"), "primary-1").unwrap(), "a missing index has no disconnect pending");
    }

    #[test]
    fn rotating_primary_id_isolates_sync_state_but_keeps_usage_history() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        store.index().conn().execute(
            "INSERT INTO event(tool, ts_ms, session, meter, in_tok, source) VALUES('local', 1, 'session', 'tokens', 3, 'local')",
            [],
        ).unwrap();
        store.save_primary_config(&PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
            advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: false,
            share_data_with_secondaries: false,
        }, 1).unwrap();
        store.rotate_primary_id("primary-2", 2).unwrap();
        assert_eq!(store.primary_id(), "primary-2");
        assert!(store.primary_config().unwrap().is_none());
        assert_eq!(store.index().event_count().unwrap(), 1);
        assert_eq!(store.state.query_row::<i64, _, _>("SELECT count(*) FROM device_registry", [], |row| row.get(0)).unwrap(), 0);
    }

    #[test]
    fn secondary_capture_config_round_trips_and_local_capture_survives_a_network_pause() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("capture.jsonl");
        std::fs::write(&source, b"fixture\n").unwrap();
        let device = identity("capture-pause");
        let certificate_der = vec![1u8, 2, 3];
        let config = SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: Some("Primary Mac".into()), device_id: device.device_id.clone(), origin: device.origin.clone(),
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
        assert_eq!(report.new_events, 1, "a network pause never pauses local capture");

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
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: "device-1".into(), origin: "laptop".into(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: vec![1, 2, 3],
            certificate_pin: hash_bytes(&[1, 2, 3]), identity_path: PathBuf::from("identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        };
        store.save_secondary_config(&config, 1).unwrap();
        store.request_secondary_disconnect(2).unwrap();
        assert!(store.secondary_config().unwrap().is_none(), "explicit disconnect must detach locally before any network ACK");
        assert!(!store.secondary_disconnect_requested().unwrap());
        let notices = read_disconnect_notices_conn(store.index().conn(), 2).unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].config.identity_path, PathBuf::from("identity.json"));
        let identity_path = store.clear_secondary_config(2).unwrap();
        assert_eq!(identity_path, None);
        assert!(store.secondary_config().unwrap().is_none());
        assert!(!store.secondary_disconnect_requested().unwrap());
        assert_eq!(store.index().event_count().unwrap(), 0);
        assert_eq!(store.index().conn().query_row::<String, _, _>(
            "SELECT value FROM meta WHERE key = 'device_sync_last_unpaired_at_ms'", [], |row| row.get(0),
        ).unwrap(), "2");
    }

    fn rollup_row(day: &str, session: &str, in_tok: f64, n: i64) -> crate::transport::RollupRow {
        crate::transport::RollupRow {
            day: day.into(), origin: String::new(), tool: "codex".into(), session: session.into(),
            project: String::new(), model: String::new(), meter: "tokens".into(),
            in_tok, cc_tok: 0.0, cr_tok: 0.0, out_tok: 0.0, reason_tok: 0.0, credits: 0.0,
            n, nonzero: n, min_ts: 1, max_ts: 2,
        }
    }

    fn secondary_config_for(device_id: &str, origin: &str) -> SecondarySyncConfig {
        SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: device_id.into(), origin: origin.into(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: vec![1, 2, 3],
            certificate_pin: hash_bytes(&[1, 2, 3]), identity_path: PathBuf::from("identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }
    }

    #[test]
    fn uplink_rollup_stores_under_registry_origin_and_keeps_absent_days() {
        let device = identity("uplink-origin");
        let mut store = paired_store_for(&device);
        let count = |store: &SyncStore, sql: &str| -> i64 {
            store.index().conn().query_row::<i64, _, _>(sql, [], |row| row.get(0)).unwrap()
        };
        let origin = store.apply_uplink_rollup(&device.device_id, &[
            rollup_row("2026-10-01", "s1", 10.0, 1),
            rollup_row("2026-10-02", "s1", 20.0, 2),
        ], 100).unwrap();
        assert_eq!(origin, "uplink-origin", "the label comes from the registry, never from the wire");
        assert_eq!(count(&store, "SELECT count(*) FROM remote_rollup WHERE origin = 'uplink-origin'"), 2);

        // A later snapshot grows day 1 and omits day 2: replace by day key,
        // keep the absent day — the snapshot is not a full clear.
        store.apply_uplink_rollup(&device.device_id, &[rollup_row("2026-10-01", "s1", 15.0, 3)], 200).unwrap();
        assert_eq!(count(&store, "SELECT count(*) FROM remote_rollup"), 2);
        assert_eq!(store.index().conn().query_row::<f64, _, _>(
            "SELECT in_tok FROM remote_rollup WHERE origin = 'uplink-origin' AND day = '2026-10-01'", [], |row| row.get(0),
        ).unwrap(), 15.0);
        assert_eq!(store.index().conn().query_row::<f64, _, _>(
            "SELECT in_tok FROM remote_rollup WHERE origin = 'uplink-origin' AND day = '2026-10-02'", [], |row| row.get(0),
        ).unwrap(), 20.0);
        let acked: String = store.index().conn().query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![format!("{UPLINK_ROLLUP_ACKED_PREFIX}{}", device.device_id)], |row| row.get(0),
        ).unwrap();
        assert_eq!(acked, "200", "the ack stamp follows the latest apply");
    }

    #[test]
    fn uplink_rollup_migration_gate_retires_row_merged_copies_once() {
        let device = identity("gate-origin");
        let mut store = paired_store_for(&device);
        let seed_merged = |store: &SyncStore, tag: &str| {
            store.index().conn().execute(
                "INSERT INTO event(tool, ts_ms, session, meter, in_tok, source, dedupe_key)
                 VALUES('codex', 10, 'their-session', 'tokens', 7, 'linux:gate-origin:their-key', ?1)",
                params![format!("gate-key-{tag}")],
            ).unwrap();
            let event_id = store.index().conn().last_insert_rowid();
            store.index().conn().execute(
                "INSERT INTO call(event_id, kind, name) VALUES(?1, 'tool_use', 'their-call')",
                params![event_id],
            ).unwrap();
            store.index().conn().execute(
                "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts)
                 VALUES('2026-10-01', 'gate-origin', 'codex', 'their-session', '', '', 'tokens', 7, 0, 0, 0, 0, 0, 1, 1, 10, 10)",
                [],
            ).unwrap();
        };
        // The row-merged era this device previously uploaded, plus one
        // untouched local rollup the gate must leave alone.
        seed_merged(&store, "first");
        store.index().conn().execute(
            "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts)
             VALUES('2026-10-01', '', 'codex', 'local-session', '', '', 'tokens', 5, 0, 0, 0, 0, 0, 1, 1, 1, 2)",
            [],
        ).unwrap();

        store.apply_uplink_rollup(&device.device_id, &[rollup_row("2026-10-01", "fresh", 9.0, 1)], 100).unwrap();
        let count = |store: &SyncStore, sql: &str| -> i64 {
            store.index().conn().query_row::<i64, _, _>(sql, [], |row| row.get(0)).unwrap()
        };
        assert_eq!(count(&store, "SELECT count(*) FROM event WHERE source LIKE 'linux:%'"), 0, "the merged event copies leave with the first aggregate apply");
        assert_eq!(count(&store, "SELECT count(*) FROM call"), 0, "their call children leave too");
        assert_eq!(count(&store, "SELECT count(*) FROM event_rollup WHERE origin = 'gate-origin'"), 0, "their materialized rollups leave too");
        assert_eq!(count(&store, "SELECT count(*) FROM event_rollup WHERE origin = ''"), 1, "local rollups are not the gate's business");
        assert_eq!(count(&store, "SELECT count(*) FROM remote_rollup WHERE origin = 'gate-origin'"), 1);

        // The gate is one-time per device: rows that appear afterwards (a
        // re-merged copy, say) belong to a later era and must survive the
        // next apply.
        seed_merged(&store, "second");
        store.apply_uplink_rollup(&device.device_id, &[rollup_row("2026-10-01", "fresh", 12.0, 4)], 200).unwrap();
        assert_eq!(count(&store, "SELECT count(*) FROM event WHERE source LIKE 'linux:%'"), 1, "the flag means the delete ran exactly once");
        assert_eq!(count(&store, "SELECT count(*) FROM remote_rollup WHERE origin = 'gate-origin'"), 1);
        assert_eq!(store.index().conn().query_row::<f64, _, _>(
            "SELECT in_tok FROM remote_rollup WHERE origin = 'gate-origin'", [], |row| row.get(0),
        ).unwrap(), 12.0);
    }

    #[test]
    fn uplink_rollup_rejects_unknown_devices_and_empty_registry_origins() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        assert!(matches!(
            store.apply_uplink_rollup("stranger", &[rollup_row("2026-10-01", "s", 1.0, 1)], 1),
            Err(SyncError::NotPaired(id)) if id == "stranger"
        ));
        let device = identity("empty-origin");
        let mut store = paired_store_for(&device);
        store.state.execute(
            "UPDATE device_registry SET origin = '' WHERE device_id = ?1",
            params![device.device_id],
        ).unwrap();
        assert!(matches!(
            store.apply_uplink_rollup(&device.device_id, &[rollup_row("2026-10-01", "s", 1.0, 1)], 1),
            Err(SyncError::Invalid(_))
        ));
        assert_eq!(store.index().conn().query_row::<i64, _, _>("SELECT count(*) FROM remote_rollup", [], |row| row.get(0)).unwrap(), 0);
    }

    #[test]
    fn local_rollup_snapshot_ships_only_local_origin_rows() {
        let store = SyncStore::open_in_memory("primary-1").unwrap();
        store.index().conn().execute_batch(
            "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts)
             VALUES('2026-10-01', '', 'codex', 'local-session', 'proj', 'model', 'tokens', 5, 1, 2, 3, 4, 0.5, 2, 1, 1, 2),
                    ('2026-10-01', 'other-device', 'codex', 'their-session', '', '', 'tokens', 50, 0, 0, 0, 0, 0, 1, 1, 1, 2)",
        ).unwrap();
        let rows = store.local_rollup_snapshot().unwrap();
        assert_eq!(rows.len(), 1, "merged imports never travel the uplink wire");
        assert_eq!(rows[0].origin, "");
        assert_eq!(rows[0].session, "local-session");
        assert_eq!(rows[0].project, "proj");
        assert_eq!(rows[0].model, "model");
        assert_eq!(rows[0].in_tok, 5.0);
        assert_eq!(rows[0].cc_tok, 1.0);
        assert_eq!(rows[0].n, 2);
        assert_eq!(rows[0].min_ts, 1);
        assert_eq!(rows[0].max_ts, 2);
    }

    #[test]
    fn uplink_ack_flips_upload_complete_for_the_panel() {
        let device = identity("uplink-ack");
        let mut store = paired_store_for(&device);
        let config = secondary_config_for(&device.device_id, &device.origin);
        assert_eq!(upload_cursor_complete_conn(store.index().conn(), &config), None, "no pairing answer yet");
        store.apply_uplink_rollup(&device.device_id, &[rollup_row("2026-10-01", "s", 1.0, 1)], 100).unwrap();
        assert_eq!(upload_cursor_complete_conn(store.index().conn(), &config), Some(true), "the ack stamp is the upload-complete answer");
        // A device that never shipped a rollup has no answer yet.
        let other = secondary_config_for("other-device", "other-origin");
        assert_eq!(upload_cursor_complete_conn(store.index().conn(), &other), None);
    }

    #[test]
    fn the_uplink_prune_pin_follows_the_secondary_switch() {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let mirror = |store: &SyncStore| -> String {
            store.index().conn().query_row(
                "SELECT value FROM meta WHERE key = 'device_sync_uplink_paired'", [], |row| row.get(0),
            ).unwrap()
        };
        assert_eq!(mirror(&store), "0", "never paired: local history is ordinary retention data");
        let config = secondary_config_for("device-1", "laptop");
        store.save_secondary_config(&config, 1).unwrap();
        assert_eq!(mirror(&store), "1", "an enabled pairing pins local history: the snapshot re-ships it");
        store.set_secondary_enabled(false, 2).unwrap();
        assert_eq!(mirror(&store), "0", "the sync switch off releases the pin");
        store.set_secondary_enabled(true, 3).unwrap();
        assert_eq!(mirror(&store), "1");
        store.queue_secondary_disconnect(&config, 4).unwrap();
        assert_eq!(mirror(&store), "0", "an explicit disconnect ends the shared-truth promise");
    }
}

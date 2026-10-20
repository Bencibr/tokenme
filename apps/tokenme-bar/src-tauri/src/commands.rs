//! The complete JS-facing surface; none of these commands computes a number.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;
use mdns_sd::{DaemonEvent, ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use usage_core::pricing::PricingOptions;
use usage_core::{origin_ok, MachineScope, PricingMap, PricingMeta, Report};
use usage_device_sync::{hash_bytes, ClientTransportCancellation, DeviceIdentity, DeviceStatus, DeviceStatusView, DeviceSyncSnapshot, DiagnosticsError, DiagnosticsHost, HttpsServerConfig, PairingInvite, PairingInviteDetails, PairingRequest, PairingRequestState, PrimarySyncConfig, SecondarySyncConfig, SyncError, SyncRuntime, SyncRuntimeConfig, SyncStore, TransportError, Tls13HttpsTransport, HttpsTransportConfig};

use crate::engine::{EngineChannel, Msg, Shared};
use crate::{bubble, panel};
use crate::settings::{BubbleSkin, NotifyTier, QuotaDisplayMode, Theme, TrayMode};
use crate::tray;

const DEFAULT_PRIMARY_BIND: &str = "0.0.0.0:47821";
const SYNC_RECOVERY_INTERVAL: Duration = Duration::from_secs(15);

/// Terminates the tray application, rather than merely hiding its panel.
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Mirrors the tray's live state so the panel can show what the bar shows.
#[derive(Debug, Clone, Serialize)]
pub struct TrayState {
    pub label: String,
    pub tooltip: String,
    pub mode: TrayMode,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SyncManagementRequest {
    Status,
    Discover,
    StartPrimary { bind: Option<String>, advertise_host: Option<String>, allow_lan: bool },
    SetPrimaryDataSharing { enabled: bool },
    StartSecondary,
    Stop,
    /// Clear a stopped primary role so this device can be paired as a
    /// secondary while retaining local usage and sync history.
    SwitchToSecondary,
    Unpair,
    CreateInvite { expires_at_ms: Option<i64> },
    Pair { invite: String, origin: Option<String>, display_name: Option<String>, os: Option<String> },
    PairDiscovered { primary_id: String, endpoint: String, certificate_pin: String, origin: Option<String>, display_name: Option<String>, os: Option<String> },
    Approve { request_id: String },
    Reject { request_id: String },
    ClearExpiredRequest { request_id: String },
    Pause { device_id: String },
    Resume { device_id: String },
    Revoke { device_id: String },
}

/// Keep command diagnostics useful without ever serializing invite URLs,
/// endpoints, device IDs, or other request payloads into the panel log.
fn sync_action_name(request: Option<&SyncManagementRequest>) -> &'static str {
    match request {
        None => "snapshot",
        Some(SyncManagementRequest::Status) => "status",
        Some(SyncManagementRequest::Discover) => "discover",
        Some(SyncManagementRequest::StartPrimary { .. }) => "start_primary",
        Some(SyncManagementRequest::SetPrimaryDataSharing { .. }) => "set_primary_data_sharing",
        Some(SyncManagementRequest::StartSecondary) => "start_secondary",
        Some(SyncManagementRequest::Stop) => "stop",
        Some(SyncManagementRequest::SwitchToSecondary) => "switch_to_secondary",
        Some(SyncManagementRequest::Unpair) => "unpair",
        Some(SyncManagementRequest::CreateInvite { .. }) => "create_invite",
        Some(SyncManagementRequest::Pair { .. }) => "pair",
        Some(SyncManagementRequest::PairDiscovered { .. }) => "pair_discovered",
        Some(SyncManagementRequest::Approve { .. }) => "approve",
        Some(SyncManagementRequest::Reject { .. }) => "reject",
        Some(SyncManagementRequest::ClearExpiredRequest { .. }) => "clear_expired_request",
        Some(SyncManagementRequest::Pause { .. }) => "pause",
        Some(SyncManagementRequest::Resume { .. }) => "resume",
        Some(SyncManagementRequest::Revoke { .. }) => "revoke",
    }
}

fn sync_error_code(error: &str) -> &'static str {
    if error.contains("reconcile") { "reconcile_required" }
    else if error.contains("primary download") { "primary_download" }
    else if error.contains("DatabaseCorrupt") || error.contains("database") || error.contains("SQLite") { "database" }
    else if error.contains("TLS handshake") || error.contains("pin mismatch") || error.contains("certificate") { "tls_or_pin" }
    else if error.contains("connection refused") || error.contains("connection reset") || error.contains("no route") || error.contains("transport I/O") || error.contains("timed out") { "transport_unreachable" }
    else if error.contains("pairing") || error.contains("not paired") || error.contains("revoked") { "pairing_state" }
    else if error.contains("non-loopback listen address") { "lan_loopback_bind" }
    else if error.contains("advertise host") { "lan_missing_advertise_host" }
    else if error.contains("cannot start sync listener") { "listener_start" }
    else if error.contains("mDNS") { "mdns" }
    else if error.contains("certificate") { "certificate" }
    else { "request_failed" }
}

fn log_sync_action(action: &str, started: Instant, error: Option<&str>) {
    if action == "snapshot" || action == "status" {
        if error.is_none() { return; }
    }
    let message = match error {
        Some(error) => format!("sync: action={action} total_ms={} outcome=error error_code={}", started.elapsed().as_millis(), sync_error_code(error)),
        None => format!("sync: action={action} total_ms={} outcome=ok", started.elapsed().as_millis()),
    };
    if error.is_some() { crate::logging::error(&message); } else { crate::logging::info(&message); }
}

#[derive(Debug, Clone)]
pub struct SyncControl {
    state: Arc<Mutex<SyncControlState>>,
    interrupt: Arc<SyncInterrupt>,
}

#[derive(Debug, Default)]
struct SyncInterrupt {
    stopping: AtomicBool,
    transport: Mutex<Option<ClientTransportCancellation>>,
}

impl SyncInterrupt {
    fn install(&self, transport: &Tls13HttpsTransport) {
        if let Ok(mut slot) = self.transport.lock() {
            let cancellation = transport.cancellation();
            if self.stopping.load(Ordering::Acquire) { cancellation.cancel(); }
            *slot = Some(cancellation);
        }
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Ok(slot) = self.transport.lock() {
            if let Some(cancellation) = slot.as_ref() { cancellation.cancel(); }
        }
    }
}

struct SyncRecoveryService {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SyncRecoveryService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SyncRecoveryService").field("join", &self.join.is_some()).finish()
    }
}

#[derive(Debug, Default)]
struct SyncControlState {
    role: String,
    primary_id: Option<String>,
    service: Option<SyncService>,
    runtime: Option<SecondaryRuntimeService>,
    endpoint: Option<String>,
    certificate_pin: Option<String>,
    invite_url: Option<String>,
    invite_expires_at_ms: Option<i64>,
    last_error: Option<String>,
    discovery_error: Option<String>,
    phase: SyncPhase,
    primary_starting: bool,
    last_checked_at_ms: Option<i64>,
    discovered_primaries: Vec<DiscoveredPrimaryView>,
    pending_pairing_attempted: bool,
    discovery_in_flight: bool,
    recovery: Option<SyncRecoveryService>,
    disconnect_delivery: Arc<Mutex<()>>,
    interrupt: Arc<SyncInterrupt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncPhase { Idle, Running, Success, Error, Cancelled }

impl Default for SyncPhase {
    fn default() -> Self { Self::Idle }
}

impl SyncPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Success => "success",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

struct SyncService {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<(), TransportError>>>,
    certificate_der: Vec<u8>,
    diagnostics: usage_device_sync::HttpsSyncServerHandle,
    mdns: Option<MdnsAdvertisement>,
}

impl std::fmt::Debug for SyncService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SyncService")
            .field("join", &self.join.is_some())
            .field("certificate_der_len", &self.certificate_der.len())
            .field("mdns", &self.mdns)
            .finish_non_exhaustive()
    }
}

/// A pairing request is persisted before the first network write.  If the
/// process dies after the primary accepted it but before the secondary config
/// was committed, the same signed request can be retried after restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingPairing {
    request: PairingRequest,
    primary_id: String,
    #[serde(default)]
    primary_display_name: Option<String>,
    endpoint: String,
    server_name: String,
    certificate_der: Vec<u8>,
    certificate_pin: [u8; 32],
    #[serde(default)]
    identity_path: Option<PathBuf>,
}

const MDNS_SERVICE_TYPE: &str = "_tokenme-sync._tcp.local.";

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredPrimaryView {
    pub service_name: String,
    pub primary_id: String,
    pub display_name: String,
    pub endpoint: String,
    pub certificate_pin: String,
    pub identity_conflict: bool,
}

struct MdnsAdvertisement {
    daemon: ServiceDaemon,
    fullname: String,
    monitor: Option<JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for MdnsAdvertisement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("MdnsAdvertisement").field("fullname", &self.fullname).finish_non_exhaustive()
    }
}

impl Drop for MdnsAdvertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
        if let Some(monitor) = self.monitor.take() { let _ = monitor.join(); }
    }
}

#[derive(Debug)]
struct SecondaryRuntimeService {
    stop: Arc<AtomicBool>,
    cancellation: ClientTransportCancellation,
    join: Option<JoinHandle<Result<usage_device_sync::RuntimeRunReport, SyncError>>>,
}

impl Default for SyncControl {
    fn default() -> Self {
        let state = SyncControlState::default();
        let interrupt = Arc::clone(&state.interrupt);
        Self { state: Arc::new(Mutex::new(state)), interrupt }
    }
}

impl Drop for SyncService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() { let _ = join.join(); }
    }
}

impl Drop for SecondaryRuntimeService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.cancellation.cancel();
        if let Some(join) = self.join.take() { let _ = join.join(); }
    }
}

impl Drop for SyncRecoveryService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() { let _ = join.join(); }
    }
}

/// State and mutating actions for the strict sync service. The IPC surface is
/// one command, but every mutation below opens the real SyncStore and returns
/// its error; the old SSH ServerSheet remains a separate feature.
#[derive(Debug, Clone, Serialize)]
pub struct SyncDeviceView {
    pub device_id: String,
    pub origin: String,
    pub display_name: String,
    pub os: String,
    pub role: String,
    pub status: String,
    pub data_epoch: u64,
    pub active_stream_id: Option<String>,
    pub last_generation: Option<u64>,
    pub last_merged_at_ms: Option<i64>,
    pub snapshot_complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncDevicesView {
    pub primary_id: Option<String>,
    pub primary_display_name: Option<String>,
    pub role: String,
    pub service_running: bool,
    pub primary_starting: bool,
    pub primary_data_sharing_enabled: bool,
    pub service_health: Option<String>,
    pub endpoint: Option<String>,
    pub certificate_pin: Option<String>,
    pub stream_id: Option<String>,
    pub data_epoch: Option<u64>,
    /// Secondary only: initial upload snapshot completion. `None` before a
    /// stream exists, `Some(false)` while pages are still being ACKed.
    pub upload_complete: Option<bool>,
    /// Secondary only: primary-data downlink completion. `None` until the
    /// primary shares its first page, `Some(false)` while the backfill runs.
    pub download_complete: Option<bool>,
    pub pending: u64,
    pub in_flight: u64,
    pub pending_bytes: u64,
    pub last_error: Option<String>,
    pub discovery_error: Option<String>,
    pub last_connected_at_ms: Option<i64>,
    pub operation_state: String,
    pub last_checked_at_ms: Option<i64>,
    pub invite_url: Option<String>,
    pub invite_expires_at_ms: Option<i64>,
    pub pairing_pending: bool,
    pub discovered_primaries: Vec<DiscoveredPrimaryView>,
    pub devices: Vec<SyncDeviceView>,
    pub pairing_requests: Vec<PairingRequestState>,
}

fn sync_device_view(device: DeviceStatusView) -> SyncDeviceView {
    SyncDeviceView {
        device_id: device.device_id,
        origin: device.origin,
        display_name: device.display_name,
        os: device.os,
        role: match device.role {
            usage_device_sync::DeviceRole::Primary => "primary",
            usage_device_sync::DeviceRole::Secondary => "secondary",
        }.into(),
        status: match device.status {
            DeviceStatus::Pending => "pending",
            DeviceStatus::Paired => "paired",
            DeviceStatus::Paused => "paused",
            DeviceStatus::Revoked => "revoked",
            DeviceStatus::ReconcileRequired => "reconcile_required",
        }.into(),
        data_epoch: device.data_epoch,
        active_stream_id: device.active_stream_id,
        last_generation: device.last_generation,
        last_merged_at_ms: device.last_merged_at_ms,
        snapshot_complete: device.snapshot_complete,
    }
}

fn sync_dir() -> Result<PathBuf, String> {
    dirs::data_dir()
        .map(|root| root.join("tokenme").join("device-sync"))
        .ok_or_else(|| "no platform user data directory available".into())
}

fn sync_index_path() -> Result<PathBuf, String> {
    usage_index::Index::default_path().ok_or_else(|| "no platform data directory available".into())
}

#[derive(Debug, Clone, Serialize)]
pub struct McpClientConfigView {
    pub config_json: String,
    pub endpoint: Option<String>,
    pub ready: bool,
    pub reason: Option<String>,
    pub expires_at_ms: Option<i64>,
    /// Whether the single credential currently carries control powers.
    pub control: bool,
}

/// Build the standard HTTP `mcpServers` entry that can be pasted into another
/// AI client's MCP settings. The endpoint is reachable on this machine's LAN
/// and exists only while the MCP switch is enabled in this TokenMe process.
///
/// One credential for the LAN MCP server. The 允许局域网 MCP 控制 switch
/// decides what that credential is worth: full control when on, read-only
/// when off. Same token, same config either way — only the scope moves.
#[tauri::command]
pub fn get_mcp_client_config(state: State<'_, crate::diagnostics::DiagnosticsState>) -> Result<McpClientConfigView, String> {
    let config = state.local_http_client_config();
    let control = state.snapshot().lan_control_enabled;
    let Some((endpoint, bearer, expiry)) = config else {
        return Ok(McpClientConfigView {
            config_json: String::new(), endpoint: None, ready: false,
            reason: Some("请先开启 MCP 诊断服务；MCP 保持开启时配置长期有效，手动关闭后重新开启会生成新凭据".into()),
            expires_at_ms: None, control,
        });
    };
    let config_json = serde_json::to_string_pretty(&json!({
        "mcpServers": { "tokenme": { "url": endpoint, "headers": { "Authorization": format!("Bearer {bearer}") } } }
    })).map_err(|error| format!("cannot encode MCP client config: {error}"))?;
    Ok(McpClientConfigView {
        config_json, endpoint: Some(endpoint), ready: true, reason: None,
        expires_at_ms: expiry, control,
    })
}

fn current_ms() -> i64 {
    usage_core::report::now_ms()
}

fn state_primary_id(state: &SyncControlState, snapshot: &DeviceSyncSnapshot) -> Result<String, String> {
    state.primary_id.clone().or_else(|| snapshot.primary_id.clone()).ok_or_else(|| "primary is not configured; start the primary service first".into())
}

/// The diagnostics authority belongs to the running HTTPS primary.  Keep the
/// listener handle and certificate behind this small crate-private snapshot so
/// the panel command cannot manufacture a local or detached grant.
pub(crate) struct PrimaryDiagnosticsContext {
    pub handle: usage_device_sync::HttpsSyncServerHandle,
    pub endpoint: String,
    pub certificate_pin: String,
    pub certificate_der: Vec<u8>,
}

pub(crate) fn primary_diagnostics_context(control: &SyncControl) -> Result<PrimaryDiagnosticsContext, String> {
    let state = control.state.lock().map_err(|_| "sync management state is busy".to_string())?;
    let service = state.service.as_ref().ok_or_else(|| "primary sync service is not running".to_string())?;
    if state.role != "primary" {
        return Err("diagnostics grants are available only while this device is the primary".into());
    }
    if service.join.as_ref().is_some_and(JoinHandle::is_finished) {
        return Err("primary sync service is no longer running".into());
    }
    Ok(PrimaryDiagnosticsContext {
        handle: service.diagnostics.clone(),
        endpoint: state.endpoint.clone().ok_or_else(|| "primary endpoint is unavailable".to_string())?,
        certificate_pin: state.certificate_pin.clone().ok_or_else(|| "primary certificate pin is unavailable".to_string())?,
        certificate_der: service.certificate_der.clone(),
    })
}

fn reap_service(state: &mut SyncControlState) {
    let finished = state.service.as_ref().and_then(|service| service.join.as_ref()).is_some_and(JoinHandle::is_finished);
    if !finished { return; }
    let mut service = state.service.take().expect("finished service exists");
    if let Some(join) = service.join.take() {
        if let Err(error) = join.join().unwrap_or_else(|_| Err(TransportError::Config("sync service thread panicked".into()))) {
            state.last_error = Some(error.to_string());
            state.phase = SyncPhase::Error;
        }
    }
    state.endpoint = None;
    state.certificate_pin = None;
}

fn stop_service(state: &mut SyncControlState) {
    state.runtime.take();
    state.service.take();
}

fn retry_disconnect_notifications(index_path: &std::path::Path) -> Result<(), String> {
    let notices = SyncStore::read_disconnect_notices(index_path, current_ms()).map_err(|error| error.to_string())?;
    // At most four seconds of network work per recovery tick, outside the
    // control mutex. A status read and a new role never wait for this ACK.
    for notice in notices.into_iter().take(2) {
        let result = usage_device_sync::deliver_disconnect_notice(&notice.config, current_ms());
        let Some(primary_id) = SyncStore::read_status(index_path).map_err(|error| error.to_string())?.primary_id else { continue; };
        let mut store = SyncStore::open(index_path, primary_id).map_err(|error| error.to_string())?;
        let error = result.as_ref().err().map(ToString::to_string);
        store.finish_disconnect_notice(&notice.config.device_id, current_ms(), error.as_deref()).map_err(|error| error.to_string())?;
        // Retain the key for this primary so re-pairing preserves event IDs.
        // Metadata only; queue/identity/endpoint details stay out of logs.
        log_sync_action("disconnect_notify", Instant::now(), error.as_deref());
    }
    Ok(())
}

fn rediscover_secondary_endpoint(index_path: &std::path::Path) -> Option<(SecondarySyncConfig, String)> {
    let config = SyncStore::read_secondary_config(index_path).ok()??;
    let error = config.last_error.as_deref()?;
    if !config.enabled || !(error.contains("transport I/O") || error.contains("transport timed out") || error.contains("TLS handshake") || error.contains("pin mismatch")) { return None; }
    let candidate = discover_mdns_primaries().ok()?.into_iter().find(|candidate| {
        candidate.primary_id == config.primary_id && !candidate.identity_conflict
            && candidate.certificate_pin == hex_bytes(&config.certificate_pin) && candidate.endpoint != config.endpoint
    })?;
    let mut transport_config = HttpsTransportConfig::pinned(&candidate.endpoint, &config.server_name, config.certificate_der.clone());
    transport_config.pinned_certificate_sha256 = config.certificate_pin;
    transport_config.connect_timeout = Duration::from_secs(1);
    transport_config.request_timeout = Duration::from_secs(2);
    Tls13HttpsTransport::new(transport_config).ok()?.probe_tls().ok()?;
    Some((config, candidate.endpoint))
}

fn clear_secondary_state(state: &mut SyncControlState) {
    state.role.clear();
    state.primary_id = None;
    state.endpoint = None;
    state.certificate_pin = None;
    state.last_error = None;
    state.discovery_error = None;
    state.phase = SyncPhase::Idle;
}

fn clear_primary_state(state: &mut SyncControlState) {
    state.role.clear();
    state.primary_id = None;
    state.endpoint = None;
    state.certificate_pin = None;
    state.invite_url = None;
    state.invite_expires_at_ms = None;
    state.last_error = None;
    state.discovery_error = None;
    state.primary_starting = false;
    state.phase = SyncPhase::Idle;
}

fn pending_pairing_path() -> Result<PathBuf, String> {
    Ok(sync_dir()?.join("pending-pairing.json"))
}

fn write_pending_pairing(pending: &PendingPairing) -> Result<(), String> {
    let path = pending_pairing_path()?;
    let bytes = serde_json::to_vec(pending).map_err(|error| format!("cannot encode pending pairing: {error}"))?;
    if bytes.len() > usage_device_sync::MAX_MANIFEST_BYTES {
        return Err("pending pairing is too large".into());
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!("pending pairing path is not a regular file: {}", path.display()));
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|error| format!("cannot persist pending pairing: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|error| format!("cannot protect pending pairing: {error}"))?;
    }
    file.write_all(&bytes).map_err(|error| format!("cannot persist pending pairing: {error}"))?;
    file.sync_all().map_err(|error| format!("cannot persist pending pairing: {error}"))?;
    Ok(())
}

fn read_pending_pairing() -> Result<Option<PendingPairing>, String> {
    let path = pending_pairing_path()?;
    let Ok(metadata) = std::fs::symlink_metadata(&path) else { return Ok(None); };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("pending pairing path is not a regular file: {}", path.display()));
    }
    if metadata.len() > usage_device_sync::MAX_MANIFEST_BYTES as u64 {
        return Err("pending pairing is too large".into());
    }
    let raw = std::fs::read(&path).map_err(|error| format!("cannot read pending pairing: {error}"))?;
    let pending = serde_json::from_slice(&raw).map_err(|error| format!("invalid pending pairing: {error}"))?;
    Ok(Some(pending))
}

fn clear_pending_pairing() -> Result<(), String> {
    let path = pending_pairing_path()?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(format!("pending pairing path is not a regular file: {}", path.display()))
        }
        Ok(_) => std::fs::remove_file(&path).map_err(|error| format!("cannot clear pending pairing: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect pending pairing: {error}")),
    }
}

fn cancel_pending_pairing(index_path: &std::path::Path) -> Result<(), String> {
    let Some(pending) = read_pending_pairing()? else { return Ok(()); };
    let identity_path = pending.identity_path.unwrap_or(sync_dir()?.join("secondary-identity.json"));
    let bound = SyncStore::read_status(index_path).map_err(|error| error.to_string())?.primary_id.unwrap_or_else(|| pending.primary_id.clone());
    let mut store = SyncStore::open(index_path, bound).map_err(|error| error.to_string())?;
    store.queue_secondary_disconnect(&SecondarySyncConfig {
        primary_id: pending.primary_id, primary_display_name: pending.primary_display_name, device_id: pending.request.device_id, origin: pending.request.origin,
        endpoint: pending.endpoint, server_name: pending.server_name,
        certificate_der: pending.certificate_der, certificate_pin: pending.certificate_pin, identity_path,
        stream_id: None, data_epoch: None, expected_generation: None, enabled: false, capture_enabled: false,
        last_error: None, last_manifest_hash: None, last_gzip_hash: None,
        last_connected_at_ms: None, last_ack_generation: None, last_ack_receipt_hash: None,
    }, current_ms()).map_err(|error| error.to_string())?;
    clear_pending_pairing()
}

fn send_pending_pairing(state: &SyncControlState, pending: &PendingPairing) -> Result<(), String> {
    pending.request.verify().map_err(|error| format!("pending pairing is invalid: {error}"))?;
    let mut transport_config = HttpsTransportConfig::pinned(
        pending.endpoint.clone(), pending.server_name.clone(), pending.certificate_der.clone(),
    );
    transport_config.pinned_certificate_sha256 = pending.certificate_pin;
    let mut transport = Tls13HttpsTransport::new(transport_config).map_err(|error| error.to_string())?;
    state.interrupt.install(&transport);
    let response = transport.send_pairing_request(&pending.request).map_err(|error| format!("pairing request failed: {error}"))?;
    match response.status.as_str() {
        "pending" | "approved" => Ok(()),
        "rejected" => Err("pairing request was rejected".into()),
        "expired" => Err("pairing invite expired".into()),
        _ => Err("pairing response contains an unknown state".into()),
    }
}

fn pairing_invite_needs_renewal(error: &str) -> bool {
    error.contains("pairing invite expired")
        || error.contains("pairing invite already consumed")
        || error.contains("pairing invite is already bound")
        || error.contains("pairing invite was concurrently claimed")
}

fn renew_pending_pairing(state: &SyncControlState, pending: &PendingPairing, identity: &DeviceIdentity) -> Result<PendingPairing, String> {
    let mut transport_config = HttpsTransportConfig::pinned(
        pending.endpoint.clone(), pending.server_name.clone(), pending.certificate_der.clone(),
    );
    transport_config.pinned_certificate_sha256 = pending.certificate_pin;
    let mut transport = Tls13HttpsTransport::new(transport_config).map_err(|error| error.to_string())?;
    state.interrupt.install(&transport);
    let (invite, certificate_der) = transport.request_pairing_invite(&pending.primary_id, &pending.endpoint)
        .map_err(|error| format!("cannot renew pairing invite: {error}"))?;
    let certificate_pin = parse_pin(&invite.certificate_pin)?;
    if hash_bytes(&certificate_der) != certificate_pin || certificate_pin != pending.certificate_pin {
        return Err("renewed pairing invite changed the pinned primary certificate".into());
    }
    let request = PairingRequest::new(&invite, identity, pending.request.os.clone()).map_err(|error| error.to_string())?;
    Ok(PendingPairing {
        request,
        primary_id: pending.primary_id.clone(),
        primary_display_name: pending.primary_display_name.clone(),
        endpoint: invite.endpoint,
        server_name: pending.server_name.clone(),
        certificate_der,
        certificate_pin,
        identity_path: pending.identity_path.clone(),
    })
}

fn primary_endpoint(config: &PrimarySyncConfig) -> Option<String> {
    let address: std::net::SocketAddr = config.bind_addr.parse().ok()?;
    let host = config.advertise_host.as_deref()?;
    let host = advertised_endpoint_host(host, config.allow_lan).ok()?;
    Some(format!("https://{host}:{}", address.port()))
}

fn reap_runtime(state: &mut SyncControlState) {
    let finished = state.runtime.as_ref()
        .and_then(|runtime| runtime.join.as_ref())
        .is_some_and(JoinHandle::is_finished);
    if !finished { return; }
    let mut runtime = state.runtime.take().expect("finished secondary runtime exists");
    if let Some(join) = runtime.join.take() {
        let result = join.join()
            .map_err(|_| SyncError::Security("secondary runtime thread panicked".into()))
            .and_then(|result| result);
        if let Err(error) = result {
            state.last_error = Some(error.to_string());
            state.phase = SyncPhase::Error;
        }
    }
}

fn secondary_transport(config: &SecondarySyncConfig) -> Result<Tls13HttpsTransport, String> {
    let mut transport_config = HttpsTransportConfig::pinned(
        config.endpoint.clone(), config.server_name.clone(), config.certificate_der.clone(),
    );
    transport_config.pinned_certificate_sha256 = config.certificate_pin;
    Tls13HttpsTransport::new(transport_config).map_err(|error| format!("cannot configure pinned sync transport: {error}"))
}

fn start_secondary_runtime(state: &mut SyncControlState) -> Result<(), String> {
    reap_runtime(state);
    if state.runtime.is_some() { return Ok(()); }
    if state.service.is_some() || state.role == "primary" {
        return Err("this TokenMe instance is already the primary sync service".into());
    }
    let index_path = sync_index_path()?;
    let config = SyncStore::read_secondary_config(&index_path)
        .map_err(|error| format!("cannot read secondary sync configuration: {error}"))?
        .ok_or_else(|| "pair this device before starting secondary sync".to_string())?;
    if !config.enabled { return Err("secondary sync is disabled; pair this device or press Start again".into()); }
    let identity = DeviceIdentity::load_or_create(
        config.identity_path.clone(), config.origin.clone(), config.origin.clone(),
    ).map_err(|error| format!("cannot load secondary identity: {error}"))?;
    if identity.device_id != config.device_id || identity.origin != config.origin {
        return Err("secondary identity does not match the paired device configuration".into());
    }
    let transport = secondary_transport(&config)?;
    state.interrupt.install(&transport);
    let cancellation = transport.cancellation();
    let store = SyncStore::open(&index_path, config.primary_id.clone())
        .map_err(|error| format!("cannot open secondary sync store: {error}"))?;
    let owner = format!("panel-secondary-{}", identity.device_id);
    let runtime_config = SyncRuntimeConfig::new(owner).map_err(|error| error.to_string())?;
    let mut runtime = SyncRuntime::new_secondary_with_display_name(
        store, transport, runtime_config, identity, usage_index::hostname(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = thread::spawn(move || runtime.run_until_real(&thread_stop));
    state.role = "secondary".into();
    state.primary_id = Some(config.primary_id);
    state.endpoint = Some(config.endpoint);
    state.certificate_pin = Some(hex_bytes(&config.certificate_pin));
    state.last_error = config.last_error;
    state.runtime = Some(SecondaryRuntimeService { stop, cancellation, join: Some(join) });
    Ok(())
}

fn start_primary(state: &mut SyncControlState, bind: Option<String>, advertise_host: Option<String>, allow_lan: bool, diagnostics_app: Option<AppHandle>) -> Result<(), String> {
    let started = Instant::now();
    let stage = |name: &str, at: Instant| crate::logging::info(&format!("sync: action=start_primary stage={name} elapsed_ms={}", at.elapsed().as_millis()));
    let index_path = sync_index_path()?;
    stage("path", started);
    reap_service(state);
    if state.service.is_some() { return Ok(()); }
    if state.runtime.is_some() {
        return Err("stop secondary sync before switching this device to primary".into());
    }
    let secondary_config = SyncStore::read_secondary_config(&index_path).map_err(|error| error.to_string())?;
    if let Some(config) = &secondary_config {
        if config.enabled {
            // A crashed/stale secondary has no runtime to stop, so make the
            // persisted stop explicit before switching roles.
            let mut stopped = SyncStore::open(&index_path, config.primary_id.clone()).map_err(|e| e.to_string())?;
            stopped.set_secondary_enabled(false, current_ms()).map_err(|e| format!("cannot stop stale secondary sync: {e}"))?;
        }
    }
    let directory = sync_dir()?;
    let identity_path = directory.join("primary-identity.json");
    let identity = DeviceIdentity::load_or_create(&identity_path, "primary", "TokenMe primary").map_err(|e| format!("cannot create primary identity: {e}"))?;
    stage("identity", started);
    let share_data_with_secondaries = SyncStore::read_primary_config(&index_path)
        .map_err(|error| format!("cannot read primary sharing preference: {error}"))?
        .filter(|config| config.primary_id == identity.device_id)
        .is_some_and(|config| config.share_data_with_secondaries);
    let cert_path = directory.join("server-cert.pem");
    let key_path = directory.join("server-key.pem");
    let (certificate_der, private_key_der) = usage_device_sync::ensure_server_certificate(&cert_path, &key_path, "localhost").map_err(|e| format!("cannot create private sync certificate: {e}"))?;
    stage("cert", started);
    let bind_text = bind.unwrap_or_else(|| DEFAULT_PRIMARY_BIND.into());
    let bind_addr: std::net::SocketAddr = bind_text.parse().map_err(|e| format!("sync bind address is invalid: {e}"))?;
    if allow_lan && bind_addr.ip().is_loopback() {
        return Err("LAN/VPN access requires a non-loopback listen address".into());
    }
    if !allow_lan && !bind_addr.ip().is_loopback() {
        return Err("a non-loopback primary listener requires LAN/VPN access; enable Allow LAN/VPN or use a loopback bind for local testing".into());
    }
    let host = match advertise_host.filter(|host| !host.trim().is_empty()) {
        Some(host) => host,
        None if bind_addr.ip().is_loopback() => "127.0.0.1".into(),
        None => infer_advertise_host().ok_or_else(|| "could not determine a reachable LAN address; enter one such as 192.168.1.20 in Advertised address".to_string())?,
    };
    let endpoint_host = advertised_endpoint_host(&host, allow_lan)?;
    let existing_primary_id = SyncStore::read_status(&index_path).map_err(|error| format!("cannot read sync role: {error}"))?.primary_id;
    let current_primary_id = existing_primary_id.unwrap_or_else(|| identity.device_id.clone());
    // Deferred open: no journal release, no capture mirror write — those are
    // index write transactions that queue for seconds behind the ingest
    // engine, and the start's banner must not wait for them. The background
    // start_converge pass performs them once the listener is up.
    let mut store = SyncStore::open_deferred(&index_path, current_primary_id.clone()).map_err(|e| format!("cannot open sync store: {e}"))?;
    stage("store_open", started);
    if current_primary_id != identity.device_id {
        store.rotate_primary_id(identity.device_id.clone(), current_ms()).map_err(|e| format!("cannot switch sync role: {e}"))?;
        if let Some(config) = &secondary_config {
            if config.identity_path.parent() == Some(directory.as_path()) { let _ = std::fs::remove_file(&config.identity_path); }
        }
    } else if let Some(config) = &secondary_config {
        store.clear_secondary_config(current_ms()).map_err(|e| format!("cannot clear stopped secondary binding: {e}"))?;
        if config.identity_path.parent() == Some(directory.as_path()) { let _ = std::fs::remove_file(&config.identity_path); }
    }
    let diagnostics_host = usage_device_sync::DiagnosticsHost::default();
    diagnostics_host.set_log_path("panel", crate::logging::log_dir().join("panel.log")).map_err(|e| format!("cannot configure diagnostics panel log: {e}"))?;
    diagnostics_host.set_log_path("sync", crate::logging::log_dir().join("diagnostics.log")).map_err(|e| format!("cannot configure diagnostics sync log: {e}"))?;
    diagnostics_host.set_build_summary(Some(format!("TokenMe {} build {}", env!("CARGO_PKG_VERSION"), env!("TOKENME_BUILD_ID")))).map_err(|e| format!("cannot configure diagnostics build summary: {e}"))?;
    diagnostics_host.set_panel_state(Some("sync".into())).map_err(|e| format!("cannot configure diagnostics panel state: {e}"))?;
    if let Some(app) = diagnostics_app.clone() {
        diagnostics_host.set_check_runner(move |checks| crate::diagnostics::run_remote_checks(app.clone(), checks))
            .map_err(|e| format!("cannot configure diagnostics check runner: {e}"))?;
    }
    let server = usage_device_sync::HttpsSyncServer::bind_with_host(HttpsServerConfig {
        bind_addr,
        certificate_der: certificate_der.clone(),
        private_key_der,
        allow_lan,
        allowed_peer_ips: Vec::new(),
        max_body_bytes: usage_device_sync::MAX_COMPRESSED_BYTES,
        max_concurrent: 4,
        staging_dir: directory.join("staging"),
        idle_timeout: std::time::Duration::from_secs(30),
        total_timeout: std::time::Duration::from_secs(600),
        staging_quota_bytes: 512 * 1024 * 1024,
    }, store, diagnostics_host).map_err(|e| format!("cannot start sync listener: {e}"))?;
    stage("listener_bind", started);
    server.set_log_sink(|is_error, message| {
        if is_error { crate::logging::error(message); } else { crate::logging::info(message); }
    }).map_err(|error| format!("cannot configure sync runtime logging: {error}"))?;
    if let Some(app) = diagnostics_app {
        let diagnostics_host = server.diagnostics_host();
        server.diagnostics_host().set_control_handler(move |tool, arguments| {
            dispatch_diagnostics_control(app.clone(), diagnostics_host.clone(), tool, arguments)
        }).map_err(|e| format!("cannot configure diagnostics control handler: {e}"))?;
    }
    let diagnostics = server.diagnostics_handle();
    let address = server.local_addr().map_err(|e| e.to_string())?;
    let endpoint = format!("https://{endpoint_host}:{}", address.port());
    // The role row lives in sync_state.db: persist it without opening the
    // usage index, so a start cannot wait out the ingest engine's write lock
    // just to record its own config. `:0` is valid for tests and must not
    // come back as an unusable endpoint after an app restart.
    SyncStore::write_primary_config_state(
        &index_path,
        &identity.device_id,
        &PrimarySyncConfig {
            primary_id: identity.device_id.clone(),
            bind_addr: address.to_string(),
            advertise_host: Some(host),
            allow_lan,
            enabled: true,
            share_data_with_secondaries,
        },
        current_ms(),
    ).map_err(|e| format!("cannot persist primary sync configuration: {e}"))?;
    stage("persist_config", started);
    // Index-side convergence the start no longer waits for: the readable
    // downlink origin pin (this machine's hostname, so secondaries see a name,
    // not a device id) plus the journal release and capture mirror refresh a
    // full open performs. Secondaries cannot attach before pairing, so a pin
    // landing seconds after the banner clears still precedes every reader.
    {
        let converge_path = index_path.clone();
        let converge_primary = identity.device_id.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let outcome = (|| {
                let mut store = match SyncStore::open(&converge_path, converge_primary) {
                    Ok(store) => store,
                    Err(error) => return Err(format!("cannot open sync store: {error}")),
                };
                store.pin_primary_export_origin(&usage_index::hostname())
                    .map_err(|error| format!("cannot pin the primary export origin: {error}"))?;
                Ok(())
            })();
            log_sync_action("start_converge", started, outcome.as_ref().err().map(String::as_str));
        });
    }
    let mdns = if allow_lan {
        let certificate_pin = hex_bytes(&hash_bytes(&certificate_der));
        match register_mdns_primary(&identity.device_id, &certificate_pin, &endpoint) {
            Ok(advertisement) => Some(advertisement),
            Err(error) => { eprintln!("tokenme sync: {error}"); None }
        }
    } else {
        None
    };
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = thread::spawn(move || server.run_until(&thread_stop));
    state.role = "primary".into();
    state.primary_id = Some(identity.device_id);
    state.endpoint = Some(endpoint);
    state.certificate_pin = Some(hex_bytes(&hash_bytes(&certificate_der)));
    state.discovery_error = if allow_lan && mdns.is_none() { Some(mdns_permission_hint("mDNS publish failed")) } else { None };
    state.last_error = None;
    state.invite_url = None;
    state.invite_expires_at_ms = None;
    state.service = Some(SyncService { stop, join: Some(join), certificate_der, diagnostics, mdns });
    Ok(())
}

fn diagnostics_params_object(arguments: &Value) -> Result<&serde_json::Map<String, Value>, DiagnosticsError> {
    arguments.as_object().ok_or(DiagnosticsError::InvalidParams)
}

fn diagnostics_optional_string(params: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>, DiagnosticsError> {
    params.get(key).map(|value| value.as_str().map(str::to_owned).ok_or(DiagnosticsError::InvalidParams)).transpose()
}

fn diagnostics_required_identifier(params: &serde_json::Map<String, Value>, key: &str) -> Result<String, DiagnosticsError> {
    let value = diagnostics_optional_string(params, key)?.ok_or(DiagnosticsError::InvalidParams)?;
    if value.is_empty() || value.len() > usage_device_sync::MAX_DIAGNOSTICS_STRING_BYTES { return Err(DiagnosticsError::InvalidParams); }
    Ok(value)
}

fn spawn_diagnostics_panel_action(
    app: AppHandle,
    host: DiagnosticsHost,
    tool: &str,
    operation_id: String,
    event: &str,
    payload: Value,
) {
    let tool = tool.to_owned();
    let event = event.to_owned();
    tauri::async_runtime::spawn(async move {
        let result = crate::diagnostics::wait_for_panel_visible(&app).await
            .map_err(|_| DiagnosticsError::ToolUnavailable)
            .and_then(|_| app.emit(&event, payload).map_err(|_| DiagnosticsError::Internal));
        host.complete_control_operation(&operation_id, result.is_ok(), current_ms());
        let response = match result {
            Ok(()) => json!({ "tool": tool, "operation_id": operation_id, "ok": true }),
            Err(error) => json!({ "tool": tool, "operation_id": operation_id, "ok": false, "error": error.to_string() }),
        };
        let _ = app.emit("diagnostics-control-result", response);
    });
}

pub(crate) fn diagnostics_primary_start_request(value: &Value) -> Result<SyncManagementRequest, DiagnosticsError> {
    let empty = serde_json::Map::new();
    let params = if value.is_null() { &empty } else { value.as_object().ok_or(DiagnosticsError::InvalidParams)? };
    diagnostics_only_keys(params, &["bind", "advertise_host", "allow_lan"])?;
    let bind = diagnostics_optional_string(params, "bind")?;
    let advertise_host = diagnostics_optional_string(params, "advertise_host")?;
    let address = bind.as_deref().unwrap_or(DEFAULT_PRIMARY_BIND).parse::<std::net::SocketAddr>().map_err(|_| DiagnosticsError::InvalidParams)?;
    let allow_lan = params.get("allow_lan").map(|value| value.as_bool().ok_or(DiagnosticsError::InvalidParams)).transpose()?.unwrap_or(!address.ip().is_loopback());
    Ok(SyncManagementRequest::StartPrimary { bind, advertise_host, allow_lan })
}

fn diagnostics_only_keys(params: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<(), DiagnosticsError> {
    if params.keys().all(|key| allowed.iter().any(|allowed| key == allowed)) { Ok(()) } else { Err(DiagnosticsError::InvalidParams) }
}

fn dispatch_diagnostics_control(app: AppHandle, host: DiagnosticsHost, tool: &str, arguments: Value) -> Result<Value, DiagnosticsError> {
    let params = diagnostics_params_object(&arguments)?;
    if serde_json::to_vec(params).map_err(|_| DiagnosticsError::InvalidParams)?.len() > usage_device_sync::MAX_DIAGNOSTICS_PARAMS_BYTES { return Err(DiagnosticsError::Limit); }
    match tool {
        "ui.capture_panel" => {
            if !params.is_empty() { return Err(DiagnosticsError::InvalidParams); }
            if !crate::diagnostics::panel_is_visible(&app) {
                return Err(DiagnosticsError::ToolUnavailable);
            }
            let image = panel::capture_panel(&app).map_err(|_| DiagnosticsError::ToolUnavailable)?;
            if image.data.len() > usage_device_sync::MAX_DIAGNOSTICS_IMAGE_BYTES { return Err(DiagnosticsError::Limit); }
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
        "ui.click_panel" => {
            diagnostics_only_keys(params, &["x", "y"])?;
            let x = params.get("x").and_then(Value::as_f64).ok_or(DiagnosticsError::InvalidParams)?;
            let y = params.get("y").and_then(Value::as_f64).ok_or(DiagnosticsError::InvalidParams)?;
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            if !crate::diagnostics::panel_is_visible(&app) {
                host.complete_control_operation(&operation_id, false, current_ms());
                return Err(DiagnosticsError::ToolUnavailable);
            }
            match panel::click_panel(&app, x, y) {
                Ok(()) => {
                    host.complete_control_operation(&operation_id, true, current_ms());
                    Ok(json!({ "accepted": true, "tool": tool, "operation_id": operation_id, "x": x, "y": y, "scope": "tokenme-panel" }))
                }
                Err(_) => {
                    host.complete_control_operation(&operation_id, false, current_ms());
                    Err(DiagnosticsError::ToolUnavailable)
                }
            }
        }
        "ui.open_sync_sheet" => {
            if !params.is_empty() { return Err(DiagnosticsError::InvalidParams); }
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            panel::show(&app, None);
            spawn_diagnostics_panel_action(app, host, tool, operation_id.clone(), "sync-open-sheet", Value::Null);
            Ok(json!({ "accepted": true, "tool": tool, "operation_id": operation_id }))
        }
        "ui.focus_device" => {
            diagnostics_only_keys(params, &["device_id"])?;
            let device_id = diagnostics_required_identifier(params, "device_id")?;
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            panel::show(&app, None);
            spawn_diagnostics_panel_action(app, host, tool, operation_id.clone(), "sync-focus-device", json!(device_id));
            Ok(json!({ "accepted": true, "tool": tool, "device_id": device_id, "operation_id": operation_id }))
        }
        "sync.pair" => {
            diagnostics_only_keys(params, &["invite", "origin", "display_name", "os"])?;
            let invite = diagnostics_required_identifier(params, "invite")?;
            if !invite.starts_with("tokenme://device-pair?") || invite.len() > usage_device_sync::MAX_DIAGNOSTICS_STRING_BYTES {
                return Err(DiagnosticsError::InvalidParams);
            }
            let origin = diagnostics_optional_string(params, "origin")?;
            let display_name = diagnostics_optional_string(params, "display_name")?;
            let os = diagnostics_optional_string(params, "os")?;
            if origin.as_ref().is_some_and(|value| value.len() > usage_device_sync::MAX_DIAGNOSTICS_STRING_BYTES)
                || display_name.as_ref().is_some_and(|value| value.len() > usage_device_sync::MAX_DIAGNOSTICS_STRING_BYTES)
                || os.as_ref().is_some_and(|value| value.len() > usage_device_sync::MAX_DIAGNOSTICS_STRING_BYTES) {
                return Err(DiagnosticsError::Limit);
            }
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            spawn_diagnostics_sync_action(app, host, tool, operation_id.clone(), SyncManagementRequest::Pair { invite, origin, display_name, os });
            Ok(json!({ "accepted": true, "tool": tool, "operation_id": operation_id }))
        }
        "sync.start_primary" => {
            let request = diagnostics_primary_start_request(&Value::Object(params.clone()))?;
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            spawn_diagnostics_sync_action(app, host, tool, operation_id.clone(), request);
            Ok(json!({ "accepted": true, "tool": tool, "operation_id": operation_id }))
        }
        "sync.stop_primary" => {
            if !params.is_empty() { return Err(DiagnosticsError::InvalidParams); }
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            spawn_diagnostics_sync_action(app, host, tool, operation_id.clone(), SyncManagementRequest::Stop);
            Ok(json!({ "accepted": true, "tool": tool, "operation_id": operation_id }))
        }
        "sync.stop_secondary" => {
            if !params.is_empty() { return Err(DiagnosticsError::InvalidParams); }
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            spawn_diagnostics_sync_action(app, host, tool, operation_id.clone(), SyncManagementRequest::Stop);
            Ok(json!({ "accepted": true, "tool": tool, "operation_id": operation_id }))
        }
        "sync.discover" => {
            if !params.is_empty() { return Err(DiagnosticsError::InvalidParams); }
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            spawn_diagnostics_sync_action(app, host, tool, operation_id.clone(), SyncManagementRequest::Discover);
            Ok(json!({ "accepted": true, "tool": tool, "operation_id": operation_id }))
        }
        "sync.approve_pairing" | "sync.reject_pairing" | "sync.clear_expired_request" => {
            diagnostics_only_keys(params, &["request_id"])?;
            let request_id = diagnostics_required_identifier(params, "request_id")?;
            let action = if tool == "sync.approve_pairing" { SyncManagementRequest::Approve { request_id } } else if tool == "sync.reject_pairing" { SyncManagementRequest::Reject { request_id } } else { SyncManagementRequest::ClearExpiredRequest { request_id } };
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            spawn_diagnostics_sync_action(app, host, tool, operation_id.clone(), action);
            Ok(json!({ "accepted": true, "tool": tool, "operation_id": operation_id }))
        }
        "sync.pause" | "sync.resume" | "sync.revoke_device" => {
            diagnostics_only_keys(params, &["device_id"])?;
            let device_id = diagnostics_required_identifier(params, "device_id")?;
            let action = if tool == "sync.pause" { SyncManagementRequest::Pause { device_id } } else if tool == "sync.resume" { SyncManagementRequest::Resume { device_id } } else { SyncManagementRequest::Revoke { device_id } };
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            spawn_diagnostics_sync_action(app, host, tool, operation_id.clone(), action);
            Ok(json!({ "accepted": true, "tool": tool, "operation_id": operation_id }))
        }
        "sync.refresh" => {
            if !params.is_empty() { return Err(DiagnosticsError::InvalidParams); }
            let operation_id = host.begin_control_operation(tool, current_ms())?;
            spawn_diagnostics_sync_action(app, host, tool, operation_id.clone(), SyncManagementRequest::Status);
            Ok(json!({ "accepted": true, "tool": tool, "operation_id": operation_id }))
        }
        _ => Err(DiagnosticsError::ToolUnavailable),
    }
}

fn spawn_diagnostics_sync_action(app: AppHandle, host: DiagnosticsHost, tool: &str, operation_id: String, action: SyncManagementRequest) {
    let tool = tool.to_owned();
    tauri::async_runtime::spawn(async move {
        let control = app.state::<SyncControl>();
        let result = sync_devices(app.clone(), Some(action), control).await;
        host.complete_control_operation(&operation_id, result.is_ok(), current_ms());
        let payload = match result { Ok(snapshot) => json!({ "tool": tool, "operation_id": operation_id, "ok": true, "snapshot": snapshot }), Err(error) => json!({ "tool": tool, "operation_id": operation_id, "ok": false, "error": error }) };
        let _ = app.emit("diagnostics-control-result", payload);
    });
}

fn hex_bytes(bytes: &[u8]) -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() }

fn register_mdns_primary(primary_id: &str, certificate_pin: &str, endpoint: &str) -> Result<MdnsAdvertisement, String> {
    let port = endpoint.rsplit(':').next().and_then(|value| value.parse::<u16>().ok()).ok_or_else(|| "mDNS cannot read the primary port".to_string())?;
    let short_id: String = primary_id.chars().filter(|ch| ch.is_ascii_alphanumeric()).take(10).collect();
    let short_id = if short_id.is_empty() { "primary".into() } else { short_id };
    let instance = format!("TokenMe-{short_id}");
    let hostname = format!("tokenme-{short_id}.local.");
    let properties = vec![("v".to_string(), "1".to_string()), ("proto".to_string(), "tokenme-device-sync/1".to_string()), ("role".to_string(), "primary".to_string()), ("id".to_string(), primary_id.to_string()), ("name".to_string(), usage_index::hostname()), ("pin".to_string(), format!("sha256:{certificate_pin}")), ("caps".to_string(), "pair,upload".to_string())];
    // addr_auto lets mdns-sd enumerate the interfaces itself, and on this
    // Windows test machine that enumeration advertised a WSL vEthernet
    // address plus three link-local v6 addresses while skipping the real LAN
    // IPv4 — a remote peer resolved the service to an unreachable address
    // (wire dump: A=172.26.224.1, AAAA=fe80::…, no 10.0.0.112 record). The
    // endpoint host is the address the user picked, or the default-route one
    // infer_advertise_host found, so register exactly that address and fall
    // back to addr_auto only when the host is a name mdns-sd must resolve.
    let without_scheme = endpoint.strip_prefix("https://").unwrap_or(endpoint);
    let without_port = without_scheme.rsplit_once(':').map(|(host, _)| host).unwrap_or(without_scheme);
    let unbracketed = without_port.trim_start_matches('[').trim_end_matches(']');
    let advertise_ip = unbracketed.parse::<std::net::IpAddr>().ok();
    let daemon = ServiceDaemon::new().map_err(|error| format!("mDNS unavailable: {error}"))?;
    let service = match advertise_ip {
        Some(ip) => ServiceInfo::new(MDNS_SERVICE_TYPE, &instance, &hostname, &ip.to_string(), port, &properties[..]),
        None => ServiceInfo::new(MDNS_SERVICE_TYPE, &instance, &hostname, "", port, &properties[..]).map(|service| service.enable_addr_auto()),
    }
    .map_err(|error| format!("mDNS service is invalid: {error}"))?;
    daemon.register(service).map_err(|error| format!("mDNS registration failed: {error}"))?;
    let fullname = format!("{instance}.{MDNS_SERVICE_TYPE}");
    let monitor_error = Arc::new(Mutex::new(None));
    let error_for_thread = Arc::clone(&monitor_error);
    let monitor = daemon.monitor().ok().map(|events| thread::spawn(move || {
        loop {
            match events.recv() {
                Ok(event) => {
                    let event_name = match event {
                        DaemonEvent::Announce(_, _) => "announce",
                        DaemonEvent::Error(_) => "error",
                        DaemonEvent::IpAdd(_) => "ip_add",
                        DaemonEvent::IpDel(_) => "ip_del",
                        DaemonEvent::NameChange(_) => "name_change",
                        DaemonEvent::Respond(_) => "respond",
                        _ => "other",
                    };
                    eprintln!("tokenme sync mDNS: event={event_name}");
                }
                Err(_) => {
                    if let Ok(mut slot) = error_for_thread.lock() { *slot = Some(mdns_permission_hint("mDNS monitor stopped")); }
                    eprintln!("tokenme sync mDNS: event=monitor_stopped");
                    break;
                }
            }
        }
    }));
    Ok(MdnsAdvertisement { daemon, fullname, monitor, error: monitor_error })
}

fn mdns_permission_hint(detail: &str) -> String {
    detail.to_string()
}

fn is_loopback_advertise_host(value: &str) -> bool {
    let host = value.trim().trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case("localhost.") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn infer_advertise_host() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(("8.8.8.8", 80)).ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_loopback() && !address.is_unspecified()).then(|| address.to_string())
}

fn merge_discovered_primary(results: &mut Vec<DiscoveredPrimaryView>, candidate: DiscoveredPrimaryView) {
    if let Some(existing) = results.iter_mut().find(|item| item.primary_id == candidate.primary_id) {
        if existing.certificate_pin != candidate.certificate_pin { existing.identity_conflict = true; }
        return;
    }
    results.push(candidate);
}

fn apply_discovery_result(state: &mut SyncControlState, result: Result<Vec<DiscoveredPrimaryView>, String>, started: Instant) {
    // A new discovery attempt supersedes a failed pairing attempt; do not
    // carry that stale error back into the discovery sheet snapshot.
    state.last_error = None;
    match result {
        Ok(primaries) => {
            let result_count = primaries.len();
            state.discovered_primaries = primaries;
            state.discovery_error = None;
            state.phase = SyncPhase::Success;
            eprintln!("tokenme sync: action=discover duration_ms={} result_count={} outcome=ok", started.elapsed().as_millis(), result_count);
        }
        Err(error) => {
            state.discovered_primaries.clear();
            state.discovery_error = Some(mdns_permission_hint(&error));
            state.phase = SyncPhase::Error;
            eprintln!("tokenme sync: action=discover duration_ms={} result_count=0 outcome=error", started.elapsed().as_millis());
        }
    }
}

/// Preference order for the endpoint an advertisement resolves to: a global
/// IPv4 first (the shape the invite UI and the TLS transport are exercised
/// against), then a global/ULA IPv6, then the link-scope addresses — IPv4's
/// plain 169.254 beats IPv6's fe80, whose zone-suffixed string ([…%14]) is
/// only meaningful on the resolving machine. Loopback and unspecified are
/// filtered before ranking.
fn advertised_address_rank(ip: std::net::IpAddr) -> u8 {
    match ip {
        std::net::IpAddr::V4(v4) => if v4.is_link_local() { 2 } else { 0 },
        std::net::IpAddr::V6(v6) => {
            let segment = v6.segments()[0];
            let routable = (0x2000..=0x3fff).contains(&segment) || (0xfc00..=0xfdff).contains(&segment);
            if routable { 1 } else { 3 }
        }
    }
}

fn discover_mdns_primaries() -> Result<Vec<DiscoveredPrimaryView>, String> {
    let daemon = ServiceDaemon::new().map_err(|error| format!("mDNS unavailable: {error}"))?;
    let receiver = daemon.browse(MDNS_SERVICE_TYPE).map_err(|error| format!("mDNS browse failed: {error}"))?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut results = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(service)) => {
                if service.get_property_val_str("v") != Some("1") || service.get_property_val_str("proto") != Some("tokenme-device-sync/1") || service.get_property_val_str("role") != Some("primary") || !service.get_property_val_str("caps").is_some_and(|caps| caps.split(',').any(|cap| cap == "pair") && caps.split(',').any(|cap| cap == "upload")) { continue; }
                let Some(primary_id) = service.get_property_val_str("id") else { continue; };
                let Some(pin) = service.get_property_val_str("pin") else { continue; };
                if !pin.strip_prefix("sha256:").is_some_and(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))) { continue; }
                // The responder answers with its whole address set. A link-local
                // IPv6 only reaches a peer when the zone index happens to line
                // up — the self-discovery probe here resolved to fe80::…%14 and
                // a cross-machine pairing would have dialled that dead string —
                // so rank a routable address first and keep link-local as the
                // last resort instead of taking the set's first entry.
                let Some(address) = service.get_addresses().iter()
                    .filter(|candidate| { let ip = candidate.to_ip_addr(); !ip.is_loopback() && !ip.is_unspecified() })
                    .min_by_key(|candidate| advertised_address_rank(candidate.to_ip_addr()))
                else { continue; };
                let address = address.to_string();
                let endpoint = if address.contains(':') { format!("https://[{address}]:{}", service.port) } else { format!("https://{address}:{}", service.port) };
                let certificate_pin = pin.strip_prefix("sha256:").unwrap_or(pin).to_string();
                let service_name = service.fullname.split('.').next().unwrap_or("TokenMe").to_string();
                merge_discovered_primary(&mut results, DiscoveredPrimaryView {
                    service_name,
                    primary_id: primary_id.to_string(),
                    display_name: service.get_property_val_str("name").unwrap_or("TokenMe primary").to_string(),
                    endpoint,
                    certificate_pin,
                    identity_conflict: false,
                });
            }
            Ok(ServiceEvent::SearchStopped(_)) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(MDNS_SERVICE_TYPE);
    let _ = daemon.shutdown();
    Ok(results)
}

fn advertised_endpoint_host(value: &str, _allow_lan: bool) -> Result<String, String> {
    let host = value.trim().trim_start_matches('[').trim_end_matches(']').to_string();
    if host.is_empty() || host.contains('/') || host.contains('?') || host.contains('#') {
        return Err("sync advertise host is invalid".into());
    }
    if _allow_lan && is_loopback_advertise_host(&host) {
        return Err("sync advertise host must be reachable; localhost and loopback addresses are not valid".into());
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip.is_unspecified() {
            return Err("sync advertise host must be reachable; unspecified addresses such as 0.0.0.0 are not valid".into());
        }
        return Ok(match ip {
            std::net::IpAddr::V6(_) => format!("[{host}]"),
            std::net::IpAddr::V4(_) => host,
        });
    }
    if host.contains(':') { return Err("sync advertise host is invalid".into()); }
    Ok(host)
}

fn parse_pin(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("pairing certificate pin must be exactly 64 hexadecimal characters".into());
    }
    let mut pin = [0u8; 32];
    for (index, slot) in pin.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "pairing certificate pin is invalid".to_string())?;
    }
    Ok(pin)
}

fn sync_snapshot(state: &mut SyncControlState, diagnostics_app: Option<AppHandle>, recover_pairing: bool) -> Result<SyncDevicesView, String> {
    reap_service(state);
    reap_runtime(state);
    if let Some(error) = state.service.as_ref().and_then(|service| service.mdns.as_ref()).and_then(|mdns| mdns.error.lock().ok().and_then(|slot| slot.clone())) {
        state.discovery_error = Some(error);
    }
    let index_path = sync_index_path()?;
    let mut secondary_config = SyncStore::read_secondary_config(&index_path).map_err(|error| format!("cannot read secondary sync configuration: {error}"))?;
    let primary_config = SyncStore::read_primary_config(&index_path).map_err(|error| format!("cannot read primary sync configuration: {error}"))?;
    if let Some(config) = secondary_config.clone() {
        // Read-only probe: a polling snapshot must never open a writable
        // store, or every status tick queues behind the ingest write lock.
        let pending_disconnect = SyncStore::read_secondary_disconnect_requested(&index_path, &config.primary_id)
            .map_err(|error| format!("cannot read pending secondary disconnect: {error}"))?;
        if pending_disconnect {
            stop_service(state);
            // Migrate a stop requested by an older build without blocking on
            // its failed network notification again.
            SyncStore::open(&index_path, config.primary_id.clone()).map_err(|error| error.to_string())?
                .request_secondary_disconnect(current_ms()).map_err(|error| error.to_string())?;
            clear_secondary_state(state);
            secondary_config = None;
        }
    }
    let pending_pairing = if secondary_config.is_none() { read_pending_pairing()? } else { None };
    // The start worker persists its config before it registers the service,
    // so a status poll in that window would see a role row with no service
    // and try to start a listener/runtime a second time — against the port
    // the worker already bound. `primary_starting` marks that window for both
    // restore paths: a primary being started must not resurrect a secondary
    // binding either.
    let restore_secondary = !state.primary_starting
        && secondary_config.is_some()
        && (state.role.is_empty() || (state.role == "secondary" && state.runtime.is_none()));
    if restore_secondary {
        if let Some(config) = &secondary_config {
            state.role = "secondary".into();
            state.primary_id = Some(config.primary_id.clone());
            state.endpoint = Some(config.endpoint.clone());
            state.certificate_pin = Some(hex_bytes(&config.certificate_pin));
            state.last_error = config.last_error.clone();
            if config.enabled && state.runtime.is_none() {
                if let Err(error) = start_secondary_runtime(state) {
                    state.last_error = Some(error);
                    state.phase = SyncPhase::Error;
                    if let Ok(mut store) = SyncStore::open(&index_path, config.primary_id.clone()) {
                        let now = current_ms();
                        let _ = store.set_secondary_last_error(state.last_error.as_deref(), now);
                    }
                } else {
                    state.phase = SyncPhase::Success;
                }
            }
        }
    }
    if recover_pairing && state.role.is_empty() && !state.pending_pairing_attempted {
        if let Some(pending) = pending_pairing {
            state.pending_pairing_attempted = true;
            if let Err(error) = resume_pending_pairing(state, &index_path, pending) {
                state.last_error = Some(error);
                state.phase = SyncPhase::Error;
            } else {
                state.phase = SyncPhase::Success;
            }
        }
    }
    // The start worker persists its config before it registers the service,
    // so a status poll in that window would see a role row with no service
    // and try to start the listener a second time — against the port the
    // worker already bound. `primary_starting` marks that window.
    let restore_primary = !state.primary_starting
        && primary_config.is_some()
        && (state.role.is_empty() || (state.role == "primary" && state.service.is_none()));    if restore_primary {
        if let Some(config) = &primary_config {
            state.role = "primary".into();
            state.primary_id = Some(config.primary_id.clone());
            state.endpoint = primary_endpoint(config);
            if config.enabled && state.service.is_none() {
                if let Err(error) = start_primary(state, Some(config.bind_addr.clone()), config.advertise_host.clone(), config.allow_lan, diagnostics_app.clone()) {
                    state.last_error = Some(error);
                    state.phase = SyncPhase::Error;
                } else {
                    state.phase = SyncPhase::Success;
                }
            }
        }
    }
    // A previous secondary transport failure can outlive an explicit unpair
    // or a role switch in the in-memory control state. Do not attach that old
    // error to a genuinely unconfigured snapshot; the setup page should start
    // clean and let the next pairing attempt report its own result.
    if state.role.is_empty() && secondary_config.is_none() && primary_config.is_none() && read_pending_pairing()?.is_none() {
        state.last_error = None;
    }
    if state.runtime.is_some() {
        // The runtime can clear its persisted error between the first config
        // read and this snapshot. Re-read before publishing the UI state so a
        // successful retry cannot leave the previous transport error visible.
        secondary_config = SyncStore::read_secondary_config(&index_path)
            .map_err(|error| format!("cannot refresh secondary sync configuration: {error}"))?;
    }
    if state.role == "secondary" {
        state.last_error = secondary_config.as_ref().and_then(|config| config.last_error.clone());
    }
    let snapshot = SyncStore::read_status(&index_path).map_err(|e| format!("cannot read device sync status: {e}"))?;
    let primary_id = state.primary_id.clone().or(snapshot.primary_id.clone()).or_else(|| secondary_config.as_ref().map(|config| config.primary_id.clone()));
    let summary = SyncStore::read_outbox_summary(&index_path).ok();
    let endpoint = state.endpoint.clone()
        .or_else(|| secondary_config.as_ref().map(|config| config.endpoint.clone()))
        .or_else(|| primary_config.as_ref().and_then(primary_endpoint));
    let certificate_pin = state.certificate_pin.clone().or_else(|| secondary_config.as_ref().map(|config| hex_bytes(&config.certificate_pin)));
    let service_health = state.service.as_ref().map(|service| service.diagnostics.health().to_string())
        .or_else(|| state.runtime.as_ref().map(|runtime| if runtime.join.as_ref().is_some_and(JoinHandle::is_finished) { "stopped".into() } else { "running".into() }))
        .or_else(|| (!state.role.is_empty()).then(|| "stopped".into()));
    let stream_id = secondary_config.as_ref().and_then(|config| config.stream_id.clone());
    let data_epoch = secondary_config.as_ref().and_then(|config| config.data_epoch);
    let upload_complete = secondary_config.as_ref().and_then(|config| SyncStore::read_upload_cursor_complete(&index_path, config));
    let download_complete = secondary_config.as_ref().and_then(|config| SyncStore::read_download_cursor_complete(&index_path, &config.primary_id));
    let last_error = if state.role == "secondary" {
        secondary_config.as_ref().and_then(|config| config.last_error.clone())
    } else {
        state.last_error.clone().or_else(|| secondary_config.as_ref().and_then(|config| config.last_error.clone()))
    };
    state.last_checked_at_ms = Some(current_ms());
    let pairing_pending = secondary_config.is_none() && read_pending_pairing()?.is_some();
    let primary_data_sharing_enabled = SyncStore::read_primary_config(&index_path)
        .map_err(|error| format!("cannot refresh primary sharing preference: {error}"))?
        .is_some_and(|config| config.share_data_with_secondaries);
    Ok(SyncDevicesView {
        primary_id,
        primary_display_name: secondary_config.as_ref().and_then(|config| config.primary_display_name.clone()),
        role: if state.role.is_empty() { "unconfigured".into() } else { state.role.clone() },
        service_running: state.service.is_some() || state.runtime.is_some(),
        primary_starting: state.primary_starting,
        primary_data_sharing_enabled,
        service_health,
        endpoint,
        certificate_pin,
        stream_id,
        data_epoch,
        upload_complete,
        download_complete,
        pending: summary.as_ref().map(|s| s.pending).unwrap_or(0),
        in_flight: summary.as_ref().map(|s| s.in_flight).unwrap_or(0),
        pending_bytes: summary.as_ref().map(|s| s.bytes).unwrap_or(0),
        last_error,
        discovery_error: state.discovery_error.clone(),
        last_connected_at_ms: secondary_config.as_ref().and_then(|config| config.last_connected_at_ms),
        operation_state: state.phase.as_str().into(),
        last_checked_at_ms: state.last_checked_at_ms,
        invite_url: state.invite_url.clone(),
        invite_expires_at_ms: state.invite_expires_at_ms,
        pairing_pending,
        discovered_primaries: state.discovered_primaries.clone(),
        devices: snapshot.devices.into_iter().map(sync_device_view).collect(),
        pairing_requests: snapshot.pairing_requests,
    })
}

/// Restore an explicitly enabled sync role at boot. A disabled config is an
/// intentional user stop and must not be treated as an autostart request.
/// The restore snapshot can wait behind the ingest engine's index.db write
/// lock for the whole of `store_open`/`persist_config` (23 s measured while
/// it still ran on the main thread during setup), so the snapshot must never
/// run inline here: kick the same locked-state restore the recovery loop
/// performs onto a worker and return immediately. `ensure_sync_recovery_loop`
/// is started first so its periodic tick re-arms anything the one-shot misses;
/// `start_primary`/`start_secondary_runtime` are idempotent behind the
/// control-state mutex, and a webview `sync_devices` read simply queues on
/// that mutex until the restore finishes, so overlap is a wait, not a race.
pub fn restore_sync_on_launch(app: &tauri::AppHandle) {
    ensure_sync_recovery_loop(app);
    let Some(control) = app.try_state::<SyncControl>() else { return; };
    let state_ref = Arc::clone(&control.state);
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Ok(mut state) = state_ref.lock() else { return; };
        if let Err(error) = sync_snapshot(&mut state, Some(worker_app), true) {
            state.last_error = Some(error);
            state.phase = SyncPhase::Error;
        }
    });
}

fn rearm_pending_pairing_retry(state: &mut SyncControlState) {
    state.pending_pairing_attempted = false;
}

fn ensure_sync_recovery_loop(app: &AppHandle) {
    let Some(control) = app.try_state::<SyncControl>() else { return; };
    let Ok(mut state) = control.state.lock() else { return; };
    if state.recovery.is_some() { return; }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let state_ref = Arc::downgrade(&control.state);
    let disconnect_delivery = Arc::clone(&state.disconnect_delivery);
    let app = app.clone();
    let join = thread::spawn(move || {
        while !thread_stop.load(Ordering::Acquire) {
            let deadline = Instant::now() + SYNC_RECOVERY_INTERVAL;
            while Instant::now() < deadline {
                if thread_stop.load(Ordering::Acquire) { return; }
                thread::sleep(Duration::from_millis(100));
            }
            let Some(state_ref) = state_ref.upgrade() else { return; };
            if let Ok(index_path) = sync_index_path() {
                let delivery = disconnect_delivery.lock();
                if delivery.is_ok() && retry_disconnect_notifications(&index_path).is_err() {
                    crate::logging::info("sync: action=disconnect_notify outcome=error error_code=retry_state");
                }
            }
            let recovered_endpoint = sync_index_path().ok().and_then(|path| rediscover_secondary_endpoint(&path));
            let Ok(mut state) = state_ref.lock() else { return; };
            if let Some((config, endpoint)) = recovered_endpoint {
                if let Ok(index_path) = sync_index_path() {
                    if let Ok(mut store) = SyncStore::open(index_path, config.primary_id.clone()) {
                        if store.update_secondary_endpoint(&config.device_id, &config.endpoint, &endpoint, current_ms()).unwrap_or(false) {
                            stop_service(&mut state);
                            crate::logging::info("sync: action=rediscover_endpoint outcome=ok");
                        }
                    }
                }
            }
            rearm_pending_pairing_retry(&mut state);
            if let Err(error) = sync_snapshot(&mut state, Some(app.clone()), true) {
                state.last_error = Some(error);
                state.phase = SyncPhase::Error;
            }
        }
    });
    state.recovery = Some(SyncRecoveryService { stop, join: Some(join) });
}

fn pair_secondary_with_details(
    state: &mut SyncControlState,
    index_path: &std::path::Path,
    snapshot: &DeviceSyncSnapshot,
    details: PairingInviteDetails,
    certificate_der: Vec<u8>,
    origin: Option<String>,
    display_name: Option<String>,
    os: Option<String>,
) -> Result<(), String> {
    reap_service(state);
    if state.service.is_some() { return Err("stop the primary sync service before pairing this device as a secondary".into()); }
    if state.runtime.is_some() || state.role == "secondary" || SyncStore::read_secondary_config(index_path).map_err(|e| e.to_string())?.is_some() {
        return Err("this device is already paired; unpair it before using a new invite".into());
    }
    let primary_id = details.primary_id.clone().ok_or_else(|| "pairing response did not include the primary id".to_string())?;
    let primary_display_name = details.primary_display_name.clone();
    let certificate_pin = parse_pin(&details.invite.certificate_pin)?;
    if hash_bytes(&certificate_der) != certificate_pin { return Err("pairing certificate does not match its advertised pin".into()); }
    let directory = sync_dir()?;
    // Serialize an old revoke against a new pairing to the same primary. A
    // delayed notification must never revoke a freshly approved connection.
    let delivery_lock = Arc::clone(&state.disconnect_delivery);
    let _delivery = delivery_lock.lock().map_err(|_| "disconnect delivery state is busy".to_string())?;
    for notice in SyncStore::read_disconnect_notices(index_path, i64::MAX).map_err(|error| error.to_string())?
        .into_iter().filter(|notice| notice.config.primary_id == primary_id)
    {
        usage_device_sync::deliver_disconnect_notice(&notice.config, current_ms())
            .map_err(|error| format!("cannot finish the previous disconnect before re-pairing: {error}"))?;
        if let Some(bound) = SyncStore::read_status(index_path).map_err(|error| error.to_string())?.primary_id {
            SyncStore::open(index_path, bound).map_err(|error| error.to_string())?
                .finish_disconnect_notice(&notice.config.device_id, current_ms(), None).map_err(|error| error.to_string())?;
        }
    }
    // `secondary` is the transport role, not the device name. Use the local
    // hostname for the persisted data origin and the human-readable label.
    let local_name = usage_index::hostname();
    let identity_path = SyncStore::read_retained_identity_path(index_path, &primary_id).map_err(|error| error.to_string())?
        .unwrap_or_else(|| directory.join(format!("secondary-{}.json", hex_bytes(&hash_bytes(primary_id.as_bytes())))));
    if identity_path.parent() != Some(directory.as_path()) { return Err("retained secondary identity is outside the sync directory".into()); }
    let display_name = display_name.unwrap_or_else(|| local_name.clone());
    let mut identity = DeviceIdentity::load_or_create(
        identity_path.clone(), origin.unwrap_or_else(|| local_name.clone()), display_name.clone(),
    ).map_err(|e| e.to_string())?;
    identity.display_name = display_name;
    let request = PairingRequest::new(&details.invite, &identity, os.unwrap_or_else(|| std::env::consts::OS.into())).map_err(|e| e.to_string())?;
    let server_name = details.server_name.unwrap_or_else(|| "localhost".into());
    let pending = PendingPairing {
        request,
        primary_id: primary_id.clone(),
        primary_display_name: primary_display_name.clone(),
        endpoint: details.invite.endpoint.clone(),
        server_name: server_name.clone(),
        certificate_der: certificate_der.clone(),
        certificate_pin,
        identity_path: Some(identity_path.clone()),
    };
    // Persist before the network write. The same request_id is idempotent on
    // the primary, so a crash after acceptance can be retried safely.
    write_pending_pairing(&pending)?;
    if let Err(error) = send_pending_pairing(state, &pending) {
        return Err(error);
    }
    let config = SecondarySyncConfig {
        primary_id: primary_id.clone(), primary_display_name, device_id: identity.device_id.clone(), origin: identity.origin.clone(),
        endpoint: details.invite.endpoint.clone(), server_name, certificate_der, certificate_pin,
        identity_path, stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
        last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
        last_ack_generation: None, last_ack_receipt_hash: None,
    };
    let current_primary_id = snapshot.primary_id.clone().or_else(|| state.primary_id.clone()).unwrap_or_else(|| primary_id.clone());
    let mut store = SyncStore::open(index_path, current_primary_id).map_err(|error| error.to_string())?;
    store.transition_to_secondary(&config, current_ms()).map_err(|error| error.to_string())?;
    drop(store);
    clear_pending_pairing()?;
    state.role = "secondary".into();
    state.primary_id = Some(primary_id.clone());
    state.endpoint = Some(details.invite.endpoint);
    state.certificate_pin = Some(hex_bytes(&certificate_pin));
    state.last_error = None;
    match start_secondary_runtime(state) {
        Ok(()) => Ok(()),
        Err(error) => {
            let mut rollback = SyncStore::open(index_path, primary_id).map_err(|open_error| open_error.to_string())?;
            let _ = rollback.set_secondary_last_error(Some(error.as_str()), current_ms());
            Err(error)
        }
    }
}

fn resume_pending_pairing(state: &mut SyncControlState, index_path: &std::path::Path, pending: PendingPairing) -> Result<(), String> {
    let directory = sync_dir()?;
    let identity_path = pending.identity_path.clone().unwrap_or_else(|| directory.join("secondary-identity.json"));
    let identity = DeviceIdentity::load_or_create(
        identity_path.clone(), pending.request.origin.clone(), pending.request.display_name.clone(),
    ).map_err(|error| format!("cannot load pending secondary identity: {error}"))?;
    if identity.device_id != pending.request.device_id || identity.origin != pending.request.origin {
        return Err("pending pairing identity does not match the stored request".into());
    }
    let pending = match send_pending_pairing(state, &pending) {
        Ok(()) => pending,
        Err(error) if pairing_invite_needs_renewal(&error) => {
            let renewed = renew_pending_pairing(state, &pending, &identity)?;
            write_pending_pairing(&renewed)?;
            send_pending_pairing(state, &renewed)?;
            renewed
        }
        Err(error) => return Err(error),
    };
    let bound = SyncStore::read_status(index_path).map_err(|error| error.to_string())?.primary_id.unwrap_or_else(|| pending.primary_id.clone());
    let mut store = SyncStore::open(index_path, bound).map_err(|error| error.to_string())?;
    let config = SecondarySyncConfig {
        primary_id: pending.primary_id.clone(), primary_display_name: pending.primary_display_name.clone(), device_id: identity.device_id.clone(), origin: identity.origin.clone(),
        endpoint: pending.endpoint.clone(), server_name: pending.server_name.clone(),
        certificate_der: pending.certificate_der.clone(), certificate_pin: pending.certificate_pin,
        identity_path, stream_id: None, data_epoch: None, expected_generation: None,
        enabled: true, capture_enabled: true, last_error: None, last_manifest_hash: None,
        last_gzip_hash: None, last_connected_at_ms: None, last_ack_generation: None,
        last_ack_receipt_hash: None,
    };
    store.transition_to_secondary(&config, current_ms()).map_err(|error| error.to_string())?;
    drop(store);
    clear_pending_pairing()?;
    state.role = "secondary".into();
    state.primary_id = Some(pending.primary_id);
    state.endpoint = Some(pending.endpoint);
    state.certificate_pin = Some(hex_bytes(&pending.certificate_pin));
    match start_secondary_runtime(state) {
        Ok(()) => Ok(()),
        Err(error) => {
            let primary_id = state.primary_id.clone().ok_or_else(|| "pending secondary primary id is missing".to_string())?;
            let mut rollback = SyncStore::open(index_path, primary_id).map_err(|open_error| open_error.to_string())?;
            let _ = rollback.set_secondary_last_error(Some(error.as_str()), current_ms());
            Err(error)
        }
    }
}

fn run_sync_action(state: &mut SyncControlState, action: SyncManagementRequest, diagnostics_app: Option<AppHandle>) -> Result<(), String> {
    let is_status = matches!(&action, SyncManagementRequest::Status);
    if !is_status { state.phase = SyncPhase::Running; }
    let result = (|| {
        let index_path = sync_index_path()?;
        let snapshot = SyncStore::read_status(&index_path).map_err(|e| format!("cannot read device sync status: {e}"))?;
        match action {
        SyncManagementRequest::Status => {
            Ok(())
        },
        SyncManagementRequest::Discover => {
            Err("mDNS discovery must run asynchronously".into())
        }
        SyncManagementRequest::StartPrimary { bind, advertise_host, allow_lan } => start_primary(state, bind, advertise_host, allow_lan, diagnostics_app),
        SyncManagementRequest::SetPrimaryDataSharing { enabled } => {
            if state.role != "primary" { return Err("only the primary can change data sharing".into()); }
            let config = SyncStore::read_primary_config(&index_path)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "primary sync is not configured".to_string())?;
            // State-only write: never opens the usage index, so the toggle
            // cannot queue behind the ingest engine's write lock.
            SyncStore::write_primary_data_sharing(&index_path, &config.primary_id, enabled, current_ms()).map_err(|error| error.to_string())
        }
        SyncManagementRequest::StartSecondary => {
            let config = SyncStore::read_secondary_config(&index_path)
                .map_err(|error| format!("cannot read secondary sync configuration: {error}"))?
                .ok_or_else(|| "pair this device before starting secondary sync".to_string())?;
            let primary_id = config.primary_id.clone();
            let mut store = SyncStore::open(&index_path, primary_id.clone()).map_err(|error| error.to_string())?;
            store.set_secondary_enabled(true, current_ms()).map_err(|error| error.to_string())?;
            store.set_secondary_last_error(None, current_ms()).map_err(|error| error.to_string())?;
            match start_secondary_runtime(state) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let mut rollback = SyncStore::open(&index_path, primary_id).map_err(|open_error| open_error.to_string())?;
                    let _ = rollback.set_secondary_last_error(Some(error.as_str()), current_ms());
                    Err(error)
                }
            }
        }
        SyncManagementRequest::Stop => {
            let was_running = state.service.is_some() || state.runtime.is_some();
            let secondary_config = SyncStore::read_secondary_config(&index_path).map_err(|error| error.to_string())?;
            let primary_config = SyncStore::read_primary_config(&index_path).map_err(|error| error.to_string())?;
            // Interrupt active sockets and release the scheduler lease before
            // retiring its outbox. A late ACK cannot race the role transition.
            let was_secondary = state.role == "secondary" || (state.role.is_empty() && secondary_config.is_some());
            stop_service(state);
            if was_secondary {
                if let Some(config) = secondary_config {
                    // State-only teardown, same seam as the primary's stop: the
                    // binding row disappears from sync_state.db in milliseconds
                    // while the ingest engine may hold the index write lock. The
                    // index-side bookkeeping — the disconnect notice that revokes
                    // this device on the primary, outbox retirement, journal
                    // release, capture mirror — runs on a background thread with
                    // the config captured here, because queue_secondary_disconnect
                    // reads the row this delete removes.
                    SyncStore::clear_secondary_config_state(&index_path, &config.primary_id, &config.device_id)
                        .map_err(|error| error.to_string())?;
                    let converge_path = index_path.clone();
                    thread::spawn(move || {
                        let started = Instant::now();
                        let outcome = (|| {
                            let mut store = match SyncStore::open(&converge_path, config.primary_id.clone()) {
                                Ok(store) => store,
                                Err(error) => return Err(format!("cannot open sync store: {error}")),
                            };
                            store.queue_secondary_disconnect(&config, current_ms())
                                .map_err(|error| format!("cannot queue the disconnect notice: {error}"))?;
                            Ok(())
                        })();
                        log_sync_action("stop_converge", started, outcome.as_ref().err().map(String::as_str));
                    });
                }
                clear_secondary_state(state);
            } else {
                // Stop & disconnect is a complete primary teardown. Do not
                // leave a disabled primary config behind for recovery: the
                // next status read must be unconfigured and the next primary
                // start must be an explicit role choice.
                cancel_pending_pairing(&index_path)?;
                clear_pending_pairing()?;
                if let Some(config) = primary_config {
                    // State-only teardown: never opens the usage index, so the
                    // stop button cannot queue behind the ingest write lock.
                    // The index-side convergence (unpaired timestamp, capture
                    // mirror, journal release) runs on a background thread and
                    // re-converges via the recovery loop if the lock wins.
                    SyncStore::clear_primary_config_state(&index_path, &config.primary_id).map_err(|error| error.to_string())?;
                    let converge_path = index_path.clone();
                    let converge_primary = config.primary_id.clone();
                    thread::spawn(move || {
                        let started = Instant::now();
                        let outcome = (|| {
                            let mut store = match SyncStore::open(&converge_path, converge_primary.clone()) {
                                Ok(store) => store,
                                Err(error) => return Err(format!("cannot open sync store: {error}")),
                            };
                            store.index().conn().execute(
                                "INSERT INTO meta(key, value) VALUES('device_sync_last_primary_unpaired_at_ms', ?1)
                                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                                [current_ms().to_string()],
                            ).map_err(|error| format!("cannot record the unpaired timestamp: {error}"))?;
                            Ok(())
                        })();
                        log_sync_action("stop_converge", started, outcome.as_ref().err().map(String::as_str));
                    });
                }
                clear_primary_state(state);
            }
            state.pending_pairing_attempted = true;
            state.invite_url = None;
            state.invite_expires_at_ms = None;
            state.discovery_error = None;
            state.phase = if was_running { SyncPhase::Cancelled } else { SyncPhase::Idle };
            Ok(())
        },
        SyncManagementRequest::SwitchToSecondary => {
            if state.service.is_some() || state.runtime.is_some() {
                return Err("stop the sync service before switching this device to secondary".into());
            }
            let Some(config) = SyncStore::read_primary_config(&index_path).map_err(|error| error.to_string())? else {
                return Err("this device has no primary configuration to switch".into());
            };
            let mut store = SyncStore::open(&index_path, config.primary_id).map_err(|error| error.to_string())?;
            store.clear_primary_config(current_ms()).map_err(|error| error.to_string())?;
            clear_pending_pairing()?;
            clear_primary_state(state);
            Ok(())
        },
        SyncManagementRequest::Unpair => {
            if state.role == "primary" || state.service.is_some() {
                return Err("stop the primary sync service before unpairing this device".into());
            }
            let config = SyncStore::read_secondary_config(&index_path).map_err(|e| e.to_string())?;
            if let Some(config) = config {
                stop_service(state);
                SyncStore::open(&index_path, config.primary_id.clone()).map_err(|error| error.to_string())?
                    .request_secondary_disconnect(current_ms()).map_err(|error| error.to_string())?;
                clear_pending_pairing()?;
            } else if read_pending_pairing()?.is_some() {
                cancel_pending_pairing(&index_path)?;
                state.pending_pairing_attempted = true;
            }
            if SyncStore::read_secondary_config(&index_path).map_err(|error| error.to_string())?.is_none() {
                clear_secondary_state(state);
            }
            Ok(())
        },
        SyncManagementRequest::CreateInvite { expires_at_ms } => {
            let primary_id = state_primary_id(state, &snapshot)?;
            let endpoint = state.endpoint.clone().ok_or_else(|| "start the primary service before creating an invite".to_string())?;
            let pin = state.certificate_pin.clone().ok_or_else(|| "primary certificate pin is unavailable".to_string())?;
            let certificate_der = state.service.as_ref().ok_or_else(|| "primary service is not running".to_string())?.certificate_der.clone();
            let mut store = SyncStore::open(&index_path, primary_id.clone()).map_err(|e| e.to_string())?;
            let expires = expires_at_ms.unwrap_or_else(|| current_ms().saturating_add(15 * 60 * 1000));
            let invite = store.create_pairing_invite(endpoint, pin, expires).map_err(|e| e.to_string())?;
            state.invite_url = Some(invite.url_with_primary_name(
                &primary_id, "localhost", &certificate_der, &usage_index::hostname(),
            ).map_err(|e| e.to_string())?);
            state.invite_expires_at_ms = Some(invite.expires_at_ms);
            Ok(())
        }
        SyncManagementRequest::Pair { invite, origin, display_name, os } => {
            let details = PairingInvite::parse_url(&invite).map_err(|e| format!("invalid pairing invite: {e}"))?;
            let certificate_der = details.certificate_der.clone().ok_or_else(|| "invite does not contain the public server certificate; ask the primary for a fresh invite".to_string())?;
            pair_secondary_with_details(state, &index_path, &snapshot, details, certificate_der, origin, display_name, os)
        }
        SyncManagementRequest::PairDiscovered { primary_id, endpoint, certificate_pin, origin, display_name, os } => {
            let pin = parse_pin(&certificate_pin)?;
            let mut transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned_hash(endpoint.clone(), "localhost", pin)).map_err(|e| e.to_string())?;
            state.interrupt.install(&transport);
            let (invite, certificate_der, primary_display_name) = transport.request_pairing_invite_with_primary_name(&primary_id, &endpoint).map_err(|e| format!("cannot obtain a secure pairing invite: {e}"))?;
            pair_secondary_with_details(state, &index_path, &snapshot, PairingInviteDetails { invite, primary_id: Some(primary_id), server_name: Some("localhost".into()), primary_display_name, certificate_der: Some(certificate_der.clone()) }, certificate_der, origin, display_name, os)
        }
        SyncManagementRequest::Approve { request_id } => {
            let primary_id = state_primary_id(state, &snapshot)?;
            SyncStore::open(&index_path, primary_id).map_err(|e| e.to_string())?.approve_pairing(&request_id, current_ms()).map(|_| ()).map_err(|e| e.to_string())
        }
        SyncManagementRequest::Reject { request_id } => {
            let primary_id = state_primary_id(state, &snapshot)?;
            SyncStore::open(&index_path, primary_id).map_err(|e| e.to_string())?.clear_pairing_request(&request_id, current_ms()).map_err(|e| e.to_string())
        }
        SyncManagementRequest::ClearExpiredRequest { request_id } => {
            let request = snapshot.pairing_requests.iter().find(|request| request.request_id == request_id)
                .ok_or_else(|| "pairing request was not found".to_string())?;
            if request.status != "expired" {
                return Err("pairing request is not expired".into());
            }
            let primary_id = state_primary_id(state, &snapshot)?;
            SyncStore::open(&index_path, primary_id).map_err(|e| e.to_string())?.reject_pairing(&request_id).map_err(|e| e.to_string())
        }
        SyncManagementRequest::Pause { device_id } => {
            let primary_id = state_primary_id(state, &snapshot)?;
            SyncStore::open(&index_path, primary_id).map_err(|e| e.to_string())?.pause_device(&device_id).map_err(|e| e.to_string())
        }
        SyncManagementRequest::Resume { device_id } => {
            let primary_id = state_primary_id(state, &snapshot)?;
            SyncStore::open(&index_path, primary_id).map_err(|e| e.to_string())?.resume_device(&device_id).map_err(|e| e.to_string())
        }
        SyncManagementRequest::Revoke { device_id } => {
            let primary_id = state_primary_id(state, &snapshot)?;
            SyncStore::open(&index_path, primary_id).map_err(|e| e.to_string())?.revoke_device(&device_id, current_ms()).map(|_| ()).map_err(|e| e.to_string())
        }
        }
    })();
    match result {
        Ok(()) => {
            if !is_status { state.last_error = None; }
            if state.phase == SyncPhase::Running { state.phase = SyncPhase::Success; }
            Ok(())
        }
        Err(error) => {
            state.last_error = Some(error.clone());
            state.phase = SyncPhase::Error;
            Err(error)
        }
    }
}

#[tauri::command]
pub async fn sync_devices(app: AppHandle, request: Option<SyncManagementRequest>, control: State<'_, SyncControl>) -> Result<SyncDevicesView, String> {
    let action = sync_action_name(request.as_ref());
    let started = Instant::now();
    let interrupts = matches!(request.as_ref(), Some(SyncManagementRequest::Stop | SyncManagementRequest::Unpair));
    if interrupts { control.interrupt.stop(); }
    let revoked_device = match request.as_ref() {
        Some(SyncManagementRequest::Revoke { device_id }) => Some(device_id.clone()),
        _ => None,
    };
    if matches!(request.as_ref(), Some(SyncManagementRequest::Stop)) {
        if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
            diagnostics.revoke_all();
        }
    }
    if matches!(request.as_ref(), Some(SyncManagementRequest::Discover)) {
        {
            let mut state = control.state.lock().map_err(|_| "sync management state is busy".to_string())?;
            if state.discovery_in_flight {
                let error = Err("mDNS discovery is already running".into());
                crate::logging::sync_event(action, false, Some("operation_busy"));
                eprintln!("tokenme sync: action={action} total_ms={}", started.elapsed().as_millis());
                log_sync_action(action, started, error.as_ref().err().map(String::as_str));
                return error;
            }
            state.discovery_in_flight = true;
            state.phase = SyncPhase::Running;
        }
        let discovery_started = Instant::now();
        let discovery = match tauri::async_runtime::spawn_blocking(discover_mdns_primaries).await {
            Ok(result) => result,
            Err(error) => Err(format!("mDNS discovery worker stopped: {error}")),
        };
        let result = (|| {
            let mut state = control.state.lock().map_err(|_| "sync management state is busy".to_string())?;
            state.discovery_in_flight = false;
            apply_discovery_result(&mut state, discovery, discovery_started);
            sync_snapshot(&mut state, Some(app.clone()), false)
        })();
        crate::logging::sync_event(action, result.is_ok(), result.as_ref().err().map(|_| "handler_failed"));
        eprintln!("tokenme sync: action={action} total_ms={}", started.elapsed().as_millis());
        log_sync_action(action, started, result.as_ref().err().map(String::as_str));
        return result;
    }
    if matches!(request.as_ref(), Some(SyncManagementRequest::StartPrimary { .. })) {
        // The full start waits behind ingest write locks for minutes on a busy
        // index. Accept immediately, flip to a visible starting state, and let
        // the blocking work finish in a worker; the sheet's existing poll
        // loop and the recovery thread publish the final state.
        {
            let mut state = control.state.lock().map_err(|_| "sync management state is busy".to_string())?;
            if state.primary_starting {
                let error = Err("primary sync is already starting".into());
                crate::logging::sync_event(action, false, Some("operation_busy"));
                log_sync_action(action, started, error.as_ref().err().map(String::as_str));
                return error;
            }
            state.primary_starting = true;
            state.last_error = None;
            state.phase = SyncPhase::Running;
        }
        let state_ref = Arc::clone(&control.state);
        let worker_app = app.clone();
        let start_request = match request.as_ref() {
            Some(SyncManagementRequest::StartPrimary { bind, advertise_host, allow_lan }) => SyncManagementRequest::StartPrimary {
                bind: bind.clone(), advertise_host: advertise_host.clone(), allow_lan: *allow_lan,
            },
            _ => unreachable!("guarded above"),
        };
        let initial_view = {
            let mut state = control.state.lock().map_err(|_| "sync management state is busy".to_string())?;
            sync_snapshot(&mut state, Some(app.clone()), false)
        }?;
        tauri::async_runtime::spawn_blocking(move || {
            let started_worker = Instant::now();
            let outcome = (|| {
                let mut state = state_ref.lock().map_err(|_| "sync management state is busy".to_string())?;
                let result = run_sync_action(&mut state, start_request, Some(worker_app.clone()));
                let view = sync_snapshot(&mut state, Some(worker_app.clone()), false);
                result?;
                view.map(|_| ())
            })();
            let error = outcome.as_ref().err().cloned();
            if let Ok(mut state) = state_ref.lock() {
                state.primary_starting = false;
                if let Some(message) = &error {
                    state.last_error = Some(message.clone());
                    state.phase = SyncPhase::Error;
                }
            }
            crate::logging::sync_event("start_primary_worker", outcome.is_ok(), outcome.as_ref().err().map(|_| "handler_failed"));
            log_sync_action("start_primary_worker", started_worker, outcome.as_ref().err().map(String::as_str));
            eprintln!("tokenme sync: action=start_primary_worker total_ms={}", started_worker.elapsed().as_millis());
        });
        crate::logging::sync_event(action, true, None);
        log_sync_action(action, started, None);
        return Ok(initial_view);
    }
    let state_ref = Arc::clone(&control.state);
    let worker_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut state = state_ref.lock().map_err(|_| "sync management state is busy".to_string())?;
        if state.discovery_in_flight && !matches!(request.as_ref(), Some(SyncManagementRequest::Status | SyncManagementRequest::Stop | SyncManagementRequest::Unpair)) {
            return Err("mDNS discovery is already running".into());
        }
        if let Some(request) = request { run_sync_action(&mut state, request, Some(worker_app.clone()))?; }
        sync_snapshot(&mut state, Some(worker_app), false)
    }).await.map_err(|_| "sync management worker stopped".to_string())?;
    if interrupts { control.interrupt.stopping.store(false, Ordering::Release); }
    crate::logging::sync_event(action, result.is_ok(), result.as_ref().err().map(|_| "handler_failed"));
    if result.is_ok() {
        if let Some(device_id) = revoked_device.as_deref() {
            if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
                diagnostics.revoke_target(device_id);
            }
        }
    }
    eprintln!("tokenme sync: action={action} total_ms={}", started.elapsed().as_millis());
    log_sync_action(action, started, result.as_ref().err().map(String::as_str));
    result
}

/// Returns the last report; `force` asks the engine for an immediate re-index.
/// The re-index never happens on this thread — the fresh report arrives through
/// the `report-updated` event.
#[tauri::command]
pub async fn get_report(app: AppHandle, force: bool) -> Result<Report, String> {
    if force {
        if let Some(channel) = app.try_state::<EngineChannel>() {
            let _ = channel.0.send(Msg::Refresh);
        }
    }
    let shared = app.state::<Shared>();
    if let Some(error) = shared.startup_error() {
        return Err(error);
    }
    shared.report().ok_or_else(|| "indexing".to_string())
}

/// The webview's detected UI language (system locale; the `?lang=` override
/// wins for QA). The Rust chrome — tray menu, tooltip, updater copy — follows
/// it; the menu rebuilds here because muda items carry their labels from
/// construction.
#[tauri::command]
pub async fn set_ui_lang(app: AppHandle, lang: String) -> Result<(), String> {
    let Some(parsed) = crate::lang::parse(&lang) else {
        return Err(format!("unknown lang: {lang}"));
    };
    if crate::lang::get() != parsed {
        crate::lang::set(parsed);
        tray::apply_lang(&app);
    }
    Ok(())
}

#[tauri::command]
pub async fn get_tray_state(app: AppHandle) -> Result<TrayState, String> {
    tray::tray_state(&app).ok_or_else(|| "indexing".to_string())
}

#[tauri::command]
pub async fn set_tray_mode(app: AppHandle, mode: TrayMode) -> Result<(), String> {
    let report = app.state::<Shared>().report();
    if let Err(e) = app.state::<Shared>().set_tray_mode(mode) {
        return Err(e);
    }
    // The menubar repaints before anything touches disk: the switch must feel
    // instant, and a slow save must never sit between the click and the pixel.
    if let Some(report) = report {
        tray::refresh(&app, &report, mode);
    }
    // Persist after the repaint: disk IO never sits between the click and the
    // menubar, and a failed write still leaves this session working.
    let settings = app.state::<Shared>().settings();
    settings.save().map_err(|e| e.to_string())?;
    Ok(())
}

/// The saved drag order of the quota section, as the panel persists it.
#[tauri::command]
pub async fn get_quota_order(app: AppHandle) -> Result<QuotaOrder, String> {
    let shared = app.state::<Shared>();
    let Ok(settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    Ok(QuotaOrder { tools: settings.quota_tools.clone(), rows: settings.quota_rows.clone() })
}

/// Persist a new drag order. The panel applies the reorder locally first, so
/// this only has to survive the next launch; a failure costs the arrangement,
/// never a report.
#[tauri::command]
pub async fn set_quota_order(app: AppHandle, order: QuotaOrder) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.quota_tools = order.tools;
    settings.quota_rows = order.rows;
    settings.clone().save().map_err(|e| e.to_string())
}

/// The drag order of the quota section: tool ids for the groups,
/// `<tool>/<row id>` for the bars inside them.
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct QuotaOrder {
    pub tools: Vec<String>,
    pub rows: Vec<String>,
}

/// Drops the price cache and re-downloads models.dev off the main thread, then
/// hands the new table to the engine so it re-summarizes without re-reading logs.
#[tauri::command]
pub async fn refresh_pricing(app: AppHandle) -> Result<PricingMeta, String> {
    let sender = app
        .try_state::<EngineChannel>()
        .map(|channel| channel.0.clone())
        .ok_or_else(|| "engine unavailable".to_string())?;
    let meta = tauri::async_runtime::spawn_blocking(move || {
        let map = PricingMap::refresh(&PricingOptions {
            offline: false,
            cache_dir: PricingOptions::default_cache_dir(),
            overrides: Default::default(),
        });
        let meta = map.meta().clone();
        let _ = sender.send(Msg::Pricing(map));
        meta
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(meta)
}

/// The macOS application icon of each tool that ships one, as a `data:` URL.
///
/// Filesystem reads only, and only ever decoration: an unreadable icon costs the
/// panel a coloured monogram, never a figure.
#[tauri::command]
pub async fn tool_icons() -> Result<BTreeMap<String, String>, String> {
    Ok(usage_core::icons::icon_data_urls())
}

/// What the settings sheet shows: the switches plus the build version.
#[derive(Debug, Clone, Serialize)]
pub struct PanelSettings {
    pub autostart: bool,
    pub refresh_secs: u64,
    pub theme: Theme,
    pub show_money: bool,
    pub quota_display_mode: QuotaDisplayMode,
    pub show_cycle_tokens: bool,
    pub ttft_enabled: bool,
    pub performance_monitoring: bool,
    pub hide_unused_tools: bool,
    pub hide_unused_quota: bool,
    pub hide_unused_estimation: bool,
    pub bubble_enabled: bool,
    pub bubble_skin: BubbleSkin,
    pub pet_companion: crate::settings::PetPreferences,
    pub host_exit_pause: bool,
    pub auto_checkin: bool,
    pub quota_polling: bool,
    pub quota_probes_off: Vec<String>,
    pub notify_tiers: NotifyTier,
    pub notify_muted: Vec<String>,
    pub plan_expiry_notify_days: Option<u8>,
    pub auto_update_check: bool,
    pub version: String,
    /// The same number `main.rs` logs at startup ("build N starting"), so the
    /// 关于 page and the log agree about which artifact is actually running.
    pub build: &'static str,
}

/// One row of the settings sheet's per-tool polling page, straight from the
/// probe registry — the page has no list of its own to fall out of step with.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeRow {
    pub id: &'static str,
    pub host_gated: bool,
    pub answered_here: bool,
    /// What the composed gate answers right now: master switch ∧ not excluded.
    pub polling: bool,
}

#[tauri::command]
pub async fn probe_tools(app: AppHandle) -> Result<Vec<ProbeRow>, String> {
    let settings = app.state::<Shared>().settings();
    Ok(usage_quota::inventory()
        .into_iter()
        .map(|p| ProbeRow {
            polling: settings.probe_allowed(p.tool),
            id: p.tool,
            host_gated: p.host_gated,
            answered_here: p.answered_here,
        })
        .collect())
}

/// Open a release page / mailto link in the user's browser. The scheme
/// whitelist is the whole of it: a panel that reads local files has no business
/// being told to launch arbitrary URLs.
#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    crate::logging::info(&format!("open_external: {url}"));
    let allowed = url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("mailto:");
    if !allowed {
        return Err(format!("unsupported url scheme: {url}"));
    }
    // The mail draft takes a beat to appear; a second click on the same
    // button while it does reads as "nothing happened" and opens a second
    // draft. Same URL within 1.5s is the same intent — swallow it.
    static LAST: std::sync::OnceLock<std::sync::Mutex<Option<(String, std::time::Instant)>>> =
        std::sync::OnceLock::new();
    let last = LAST.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut slot) = last.lock() {
        let duplicate = slot
            .as_ref()
            .is_some_and(|(prev, at)| *prev == url && at.elapsed() < std::time::Duration::from_millis(1500));
        *slot = Some((url.clone(), std::time::Instant::now()));
        if duplicate {
            return Ok(());
        }
    }
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let spawned = {
        // `cmd` is a console binary, so without this every panel link (发邮件 /
        // Releases / 日志) opens through a visible console window that the default
        // terminal paints for a moment and steals focus with.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/c", "start", "", &url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(&url).spawn();
    spawned.map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_panel_settings(app: AppHandle) -> Result<PanelSettings, String> {
    let settings = app.state::<Shared>().settings();
    Ok(PanelSettings {
        autostart: settings.autostart,
        refresh_secs: settings.refresh_secs,
        theme: settings.theme,
        show_money: settings.show_money,
        quota_display_mode: settings.quota_display_mode,
        show_cycle_tokens: settings.show_cycle_tokens,
        ttft_enabled: settings.ttft_enabled,
        performance_monitoring: settings.performance_monitoring,
        hide_unused_tools: settings.hide_unused_tools,
        hide_unused_quota: settings.hide_unused_quota,
        hide_unused_estimation: settings.hide_unused_estimation,
        bubble_enabled: settings.bubble_enabled,
        bubble_skin: settings.bubble_skin,
        pet_companion: settings.pet_companion,
        host_exit_pause: settings.host_exit_pause,
        auto_checkin: settings.auto_checkin,
        quota_polling: settings.quota_polling,
        quota_probes_off: settings.quota_probes_off.clone(),
        notify_tiers: settings.notify_tiers,
        notify_muted: settings.notify_muted.clone(),
        plan_expiry_notify_days: settings.plan_expiry_notify_days,
        auto_update_check: settings.auto_update_check,
        version: env!("CARGO_PKG_VERSION").to_string(),
        build: env!("TOKENME_BUILD_ID"),
    })
}

#[tauri::command]
pub async fn set_autostart(app: AppHandle, on: bool) -> Result<(), String> {
    tray::set_autostart(&app, on);
    Ok(())
}

/// Purely a webview concern — the panel applies it as `data-theme` itself;
/// persisting it here is what makes the choice survive a relaunch.
#[tauri::command]
pub async fn set_theme(app: AppHandle, theme: Theme) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.theme = theme;
    settings.clone().save().map_err(|e| e.to_string())?;
    // the native backing must follow, or the window strip under the sheet
    // flashes the old theme's colour for the lifetime of the panel
    #[cfg(target_os = "macos")]
    crate::panel::apply_window_background(&app, Some(theme));
    Ok(())
}

/// The page's two cold-start milestones, reported by name because they answer
/// different questions: `boot` is when the bundle executed (index.html's shell is
/// on screen), `content` is when it has figures — and only the second one retires
/// the native boot note. See `panel::mark_page_boot` / `panel::mark_content_ready`
/// for why neither may gate the window.
#[tauri::command]
pub fn panel_page_signal(app: tauri::AppHandle, stage: String) {
    match stage.as_str() {
        "boot" => crate::panel::mark_page_boot(),
        "content" => crate::panel::mark_content_ready(&app),
        other => crate::logging::error(&format!("panel: unknown page signal {other:?}")),
    }
}

/// Reveal the diagnostic log directory in the OS file manager.
#[tauri::command]
pub fn open_log_dir() -> Result<(), String> {
    let dir = crate::logging::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("explorer").arg(&dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(&dir).spawn();
    spawned.map(|_| ()).map_err(|e| e.to_string())
}

/// Whether the tools page also lists tools with zero sessions this period.
/// Off keeps the page to what actually ran; a quiet tool reappears the day it
/// bills again. Same shape as the other panel switches: persist, then let the
/// webview re-render from its own state.
#[tauri::command]
pub async fn set_hide_unused_tools(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.hide_unused_tools = on;
    settings.clone().save().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_hide_unused_quota(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.hide_unused_quota = on;
    settings.clone().save().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_hide_unused_estimation(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.hide_unused_estimation = on;
    settings.clone().save().map_err(|e| e.to_string())
}

/// The host-exit pause: stop probing a tool's quota once its application has
/// exited; the last known answer stays on screen until it runs again.
#[tauri::command]
pub async fn set_host_exit_pause(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".to_string());
    };
    settings.host_exit_pause = on;
    settings.clone().save().map_err(|e| e.to_string())
}

/// The auto-check-in guarantee: on, the host-exit pause stops applying to the
/// check-in tools (Trae CN, Qoder) — their claim is HTTPS against the account,
/// so a closed host app must not cost a day, and their rows stay live in the
/// strip. The master and per-tool switches still stand above it; this only
/// lifts the host half of the gate. The pass cache is 60 s at most, so the
/// change lands within one cadence tick.
#[tauri::command]
pub async fn set_auto_checkin(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let shared = app.state::<Shared>();
        let Ok(mut settings) = shared.settings.lock() else {
            return Err("settings busy".to_string());
        };
        settings.auto_checkin = on;
        settings.clone().save().map_err(|e| e.to_string())?;
    }
    // The switch is the probes' master gate now: it takes effect this pass,
    // not on the next settings re-read, so a fresh pass runs with it.
    usage_quota::set_auto_checkin_enabled(on);
    if let Some(channel) = app.try_state::<EngineChannel>() {
        let _ = channel.0.send(Msg::Refresh);
    }
    Ok(())
}

/// The master switch: off, no vendor request leaves on the next pass. It is
/// deliberately *not* wired to the manual refresh — that path clears the quota
/// cache, and a switch whose promise is "the numbers stay where they were" must
/// not be the thing that erases them. The pass cache is 60 s at most, so the
/// stop lands within one cadence tick; the label appears at once because the
/// frontend reads it from settings, not from the report.
#[tauri::command]
pub async fn set_quota_polling(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.quota_polling = on;
    settings.clone().save().map_err(|e| e.to_string())
}

/// One tool's own switch — the only lever for the probes with no host mapping.
/// The list is stored as exclusions (an allow-list would have to be rewritten
/// every time the registry gains a probe).
#[tauri::command]
pub async fn set_tool_polling(app: AppHandle, tool: String, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.quota_probes_off.retain(|t| *t != tool);
    if !on {
        settings.quota_probes_off.push(tool);
        settings.quota_probes_off.sort();
    }
    settings.clone().save().map_err(|e| e.to_string())
}

/// Which banner lines fire. Delivery policy only: the tier machine keeps
/// recording where each window stands, so turning this back on does not
/// retro-fire a line crossed while it was off.
#[tauri::command]
pub async fn set_notify_tiers(app: AppHandle, tiers: NotifyTier) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.notify_tiers = tiers;
    settings.clone().save().map_err(|e| e.to_string())
}

/// Which lead time should announce an expiring vendor package. The UI only
/// offers these values; reject hand-written settings that would make the
/// reminder fire at an arbitrary interval.
#[tauri::command]
pub async fn set_plan_expiry_notify_days(app: AppHandle, days: Option<u8>) -> Result<(), String> {
    if days.is_some_and(|n| !matches!(n, 1 | 3 | 5)) {
        return Err("plan expiry reminder must be 1, 3, 5, or disabled".into());
    }
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.plan_expiry_notify_days = days;
    settings.clone().save().map_err(|e| e.to_string())
}

/// One tool's banner mute, same exclusion-list shape as the polling switches.
#[tauri::command]
pub async fn set_tool_muted(app: AppHandle, tool: String, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.notify_muted.retain(|t| *t != tool);
    if on {
        settings.notify_muted.push(tool);
        settings.notify_muted.sort();
    }
    settings.clone().save().map_err(|e| e.to_string())
}

/// Dollar figures render at all — the switch the settings sheet flips.
#[tauri::command]
pub async fn set_show_money(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.show_money = on;
    settings.clone().save().map_err(|e| e.to_string())
}

/// Persist the gauge direction without waking probes or clearing cached answers.
#[tauri::command]
pub async fn set_quota_display_mode(app: AppHandle, mode: QuotaDisplayMode) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let mut settings = shared.settings.lock().map_err(|_| "settings busy")?;
    let mut next = settings.clone();
    next.quota_display_mode = mode;
    next.save().map_err(|e| e.to_string())?;
    *settings = next;
    Ok(())
}

/// Persist the optional local quota-cycle token summary without changing probes.
#[tauri::command]
pub async fn set_show_cycle_tokens(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let mut settings = shared.settings.lock().map_err(|_| "settings busy")?;
    let mut next = settings.clone();
    next.show_cycle_tokens = on;
    next.save().map_err(|e| e.to_string())?;
    *settings = next;
    Ok(())
}

#[tauri::command]
pub fn get_ttft_snapshot(app: AppHandle) -> crate::ttft::TtftSnapshot {
    app.state::<crate::ttft::TtftState>().snapshot()
}

#[tauri::command]
pub fn set_ttft_enabled(app: AppHandle, on: bool) -> Result<(), String> {
    let ttft = app.state::<crate::ttft::TtftState>();
    let shared = app.state::<Shared>();
    let mut settings = shared.settings.lock().map_err(|_| "settings busy")?;
    let previous = settings.ttft_enabled;
    if previous == on { return Ok(()); }
    ttft.set_enabled(on)?;
    let mut next = settings.clone();
    next.ttft_enabled = on;
    if let Err(error) = next.clone().save() {
        let _ = ttft.set_enabled(previous);
        return Err(error.to_string());
    }
    *settings = next;
    Ok(())
}

/// Start or stop opt-in process-family sampling; the logger writes samples to panel.log.
#[tauri::command]
pub fn set_performance_monitoring(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let mut settings = shared.settings.lock().map_err(|_| "settings busy")?;
    let previous = settings.performance_monitoring;
    if previous == on { return Ok(()); }
    crate::performance::set_enabled(on).map_err(|error| error.to_string())?;
    let mut next = settings.clone();
    next.performance_monitoring = on;
    if let Err(error) = next.clone().save() {
        let _ = crate::performance::set_enabled(previous);
        return Err(error.to_string());
    }
    *settings = next;
    Ok(())
}


/// Windows-only edge bubble. The command remains available on every target so
/// the frontend bridge stays platform-neutral; non-Windows is a no-op.
#[tauri::command]
pub async fn set_bubble_enabled(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let shared = app.state::<Shared>();
        let Ok(mut settings) = shared.settings.lock() else {
            return Err("settings busy".into());
        };
        settings.bubble_enabled = on;
        settings.clone().save().map_err(|e| e.to_string())?;
    }
    bubble::set_enabled(&app, on);
    if !on { crate::pet_notice::dismiss(&app, None); }
    Ok(())
}

/// Persist the bubble appearance and switch all listening pages immediately.
#[tauri::command]
pub async fn set_bubble_skin(app: AppHandle, skin: BubbleSkin) -> Result<(), String> {
    {
        let shared = app.state::<Shared>();
        let Ok(mut settings) = shared.settings.lock() else {
            return Err("settings busy".into());
        };
        let mut next = settings.clone();
        next.bubble_skin = skin.clone();
        next.save().map_err(|e| e.to_string())?;
        *settings = next;
    }
    app.emit("bubble-skin-changed", skin).map_err(|e| e.to_string())
}

/// Called by the Windows bubble when it is clicked.
#[tauri::command]
pub async fn set_pet_preferences(app:AppHandle, prefs:crate::settings::PetPreferences) -> Result<(), String> {
    let mut prefs = prefs;
    prefs.rest_interval_minutes = prefs.rest_interval_minutes.clamp(15,180);
    {
        let shared = app.state::<Shared>();
        let mut settings = shared.settings.lock().map_err(|_| "settings busy")?;
        let mut next = settings.clone(); next.pet_companion = prefs.clone();
        next.save().map_err(|e| e.to_string())?; *settings = next;
    }
    if crate::pet_notice::get_pet_notice(app.clone()).is_some_and(|n|
        if n.kind == crate::pet_notice::Kind::Rest { !prefs.rest_reminders } else { !prefs.greetings }) {
        crate::pet_notice::dismiss(&app, None);
    }
    app.emit("pet-preferences-changed", prefs).map_err(|e| e.to_string())
}

/// The pet's click: a toggle, not a one-way show — the second click takes
/// the panel back down (user: 点击弹出菜单，再次点击隐藏菜单).
#[tauri::command]
pub fn show_panel(app: AppHandle) {
    if crate::panel::is_panel_hidden() {
        panel::show(&app, None);
    } else {
        panel::hide(&app);
    }
}

/// Dismisses the panel while keeping the native and Rust visibility state in
/// sync, so the next tray click opens it again.
#[tauri::command]
pub fn hide_panel(app: AppHandle) {
    panel::hide(&app);
}

/// While a text field holds focus the frontend asks for a keyboard session —
/// the non-activating tray panel otherwise never owns the keys on Windows.
#[tauri::command]
pub fn panel_keyboard(app: AppHandle, on: bool) {
    panel::set_keyboard_mode(&app, on);
}

/// Hands the press to the Rust-side drag loop. `window.startDragging()` cannot
/// move this non-activating window (see `bubble::begin_drag`); non-Windows is a
/// no-op so the bridge stays platform-neutral.
#[tauri::command]
pub fn begin_bubble_drag(app: AppHandle) {
    bubble::begin_drag(&app);
}

/// The check-in button of a checkin-capable tool (Trae CN / Qoder): one
/// forced claim right now — the automatic pass may already have tried and
/// failed today; a manual click is the user asking again. The answer is
/// structured: `ok` alone flips the button into the done badge, `message`
/// (the reason it did not — 未到签到时间, 排队中 — or the success copy) rides
/// the button until the next sample refresh.
#[derive(serde::Serialize)]
pub struct CheckinResult {
    pub ok: bool,
    pub message: String,
}

#[tauri::command]
pub async fn checkin_now(tool: String) -> Result<CheckinResult, String> {
    // The registry owns the dispatch (usage_quota::CHECKIN_TOOLS): the same
    // table the engine's exemption and the strip's control read, so a claim
    // added there is claimable here without a second list. A tool outside it
    // is refused before anything touches a vendor.
    let answer = tokio::task::spawn_blocking(move || usage_quota::manual_checkin(&tool))
        .await
        .map_err(|e| format!("签到任务失败: {e}"))?;
    let (ok, message) = answer?;
    Ok(CheckinResult { ok, message })
}

/// Persist the new fallback cadence, then wake the engine so the next wait
/// uses it instead of the old value's remaining timeout.
#[tauri::command]
pub async fn set_refresh_secs(app: AppHandle, secs: u64) -> Result<(), String> {
    {
        let shared = app.state::<Shared>();
        let Ok(mut settings) = shared.settings.lock() else {
            return Err("settings busy".into());
        };
        settings.refresh_secs = secs.clamp(10, 3600);
        settings.clone().save().map_err(|e| e.to_string())?;
    }
    if let Some(channel) = app.try_state::<EngineChannel>() {
        let _ = channel.0.send(Msg::Refresh);
    }
    Ok(())
}

/// Switches the machine scope the panel folds with. The command returns as
/// soon as the engine has the message — the new report arrives through the
/// ordinary `report-updated` event, so the UI never blocks on a refold.
#[tauri::command]
pub fn set_report_scope(app: AppHandle, scope: MachineScope) -> Result<(), String> {
    // The scope normally comes from a menu built out of the report itself, but
    // this is a boundary: an origin name that `origin_of` cannot parse back
    // would silently select nothing, so reject it loudly instead.
    if let MachineScope::Origin { name } = &scope {
        if !origin_ok(name) {
            return Err(format!(
                "origin {name:?} contains ':', '/' or '\\' or a control character"
            ));
        }
    }
    let kind = match &scope {
        MachineScope::All => "all",
        MachineScope::Local => "local",
        MachineScope::Origin { .. } => "origin",
    };
    let shared = app.state::<Shared>();
    let channel = app.try_state::<EngineChannel>();
    let (changed, queued) = match shared.set_scope(scope, channel.as_ref().map(|state| &state.0)) {
        Ok(result) => result,
        Err(error) => {
            crate::logging::error(&format!("scope_request: scope={kind} outcome=error error={error}"));
            return Err(error);
        }
    };
    if !changed {
        crate::logging::info(&format!("scope_request: scope={kind} outcome=unchanged"));
        return Ok(());
    }
    if channel.is_none() {
        crate::logging::error(&format!("scope_request: scope={kind} outcome=no_engine"));
    } else if queued {
        crate::logging::info(&format!("scope_request: scope={kind} outcome=queued"));
    } else {
        crate::logging::info(&format!("scope_request: scope={kind} outcome=coalesced"));
    }
    Ok(())
}

#[cfg(test)]
mod sync_tests {
    #[test]
    fn stop_latches_before_a_pairing_transport_is_installed() {
        let interrupt = super::SyncInterrupt::default();
        interrupt.stop();
        let transport = usage_device_sync::Tls13HttpsTransport::new(usage_device_sync::HttpsTransportConfig::pinned(
            "https://127.0.0.1:1", "localhost", vec![1, 2, 3],
        )).unwrap();
        interrupt.install(&transport);
        assert!(matches!(transport.probe_tcp(), Err(usage_device_sync::TransportError::Cancelled)));
    }

    #[test]
    fn stop_interrupt_is_shared_with_the_locked_management_state() {
        let control = super::SyncControl::default();
        assert!(std::sync::Arc::ptr_eq(&control.interrupt, &control.state.lock().unwrap().interrupt));
    }

    use super::{advertised_endpoint_host, clear_primary_state, merge_discovered_primary, pairing_invite_needs_renewal, primary_endpoint, rearm_pending_pairing_retry, sync_action_name, sync_error_code, DiscoveredPrimaryView, SyncControlState, SyncManagementRequest, SyncPhase, DEFAULT_PRIMARY_BIND, MDNS_SERVICE_TYPE};
    use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
    use std::time::{Duration, Instant};

    #[test]
    fn advertised_hosts_are_safe_for_invites() {
        assert_eq!(advertised_endpoint_host("192.168.1.20", true).unwrap(), "192.168.1.20");
        assert_eq!(advertised_endpoint_host("2001:db8::20", true).unwrap(), "[2001:db8::20]");
        assert!(advertised_endpoint_host("0.0.0.0", true).is_err());
        assert!(advertised_endpoint_host("127.0.0.1", true).is_err());
        assert_eq!(advertised_endpoint_host("127.0.0.1", false).unwrap(), "127.0.0.1");
        assert_eq!(advertised_endpoint_host("localhost", false).unwrap(), "localhost");
        assert!(advertised_endpoint_host("localhost", true).is_err());
        assert!(advertised_endpoint_host("::", false).is_err());
        assert!(advertised_endpoint_host("2001:db8::20/path", true).is_err());
    }

    #[test]
    fn primary_defaults_to_a_reachable_listener() {
        assert_eq!(DEFAULT_PRIMARY_BIND, "0.0.0.0:47821");
    }

    #[test]
    fn primary_endpoint_uses_reachable_advertised_host() {
        let config = usage_device_sync::PrimarySyncConfig {
            primary_id: "primary-1".into(),
            bind_addr: "0.0.0.0:47821".into(),
            advertise_host: Some("192.168.1.20".into()),
            allow_lan: true,
            enabled: true,
            share_data_with_secondaries: false,
        };
        let endpoint = primary_endpoint(&config).expect("configured LAN primary has an endpoint");
        assert_eq!(endpoint, "https://192.168.1.20:47821");
        assert!(!endpoint.contains("localhost"));
        assert!(!endpoint.contains("0.0.0.0"));
    }

    #[test]
    fn mdns_same_id_with_different_pin_is_blocked() {
        let mut results = vec![DiscoveredPrimaryView { service_name: "one".into(), primary_id: "same".into(), display_name: "One".into(), endpoint: "https://192.168.1.2:47821".into(), certificate_pin: "a".repeat(64), identity_conflict: false }];
        merge_discovered_primary(&mut results, DiscoveredPrimaryView { service_name: "two".into(), primary_id: "same".into(), display_name: "Two".into(), endpoint: "https://192.168.1.3:47821".into(), certificate_pin: "b".repeat(64), identity_conflict: false });
        assert_eq!(results.len(), 1);
        assert!(results[0].identity_conflict);
    }

    #[test]
    fn discovery_clears_a_stale_pairing_error() {
        let mut state = SyncControlState { last_error: Some("invalid pairing invite".into()), phase: SyncPhase::Error, ..SyncControlState::default() };
        super::apply_discovery_result(&mut state, Ok(Vec::new()), Instant::now());
        assert_eq!(state.last_error, None);
        assert_eq!(state.phase, SyncPhase::Success);
    }

    #[test]
    fn recovery_tick_rearms_pending_pairing_recovery() {
        let mut state = SyncControlState { pending_pairing_attempted: true, ..SyncControlState::default() };
        rearm_pending_pairing_retry(&mut state);
        assert!(!state.pending_pairing_attempted, "refresh must allow the persisted request to retry");
    }

    #[test]
    fn stopping_primary_clears_role_and_invite_state() {
        let mut state = SyncControlState {
            role: "primary".into(),
            primary_id: Some("primary-1".into()),
            endpoint: Some("https://192.0.2.1:47821".into()),
            certificate_pin: Some("a".repeat(64)),
            invite_url: Some("tokenme://device-pair?secret=private".into()),
            invite_expires_at_ms: Some(123),
            phase: SyncPhase::Success,
            ..SyncControlState::default()
        };
        clear_primary_state(&mut state);
        assert!(state.role.is_empty());
        assert_eq!(state.primary_id, None);
        assert_eq!(state.endpoint, None);
        assert_eq!(state.certificate_pin, None);
        assert_eq!(state.invite_url, None);
        assert_eq!(state.invite_expires_at_ms, None);
        assert_eq!(state.phase, SyncPhase::Idle);
    }

    #[test]
    fn expired_pairing_invites_are_the_only_retry_errors_that_renew_invites() {
        assert!(pairing_invite_needs_renewal("pairing request failed: pairing invite expired"));
        assert!(pairing_invite_needs_renewal("pairing request failed: pairing invite is already bound to request req-1"));
        assert!(!pairing_invite_needs_renewal("pairing request failed: certificate pin mismatch"));
    }

    #[test]
    fn sync_timing_action_names_do_not_include_request_payloads() {
        let request = SyncManagementRequest::Pair {
            invite: "tokenme://device-pair?secret=private".into(),
            origin: Some("workstation".into()),
            display_name: Some("Private device".into()),
            os: Some("macOS".into()),
        };
        assert_eq!(sync_action_name(None), "snapshot");
        assert_eq!(sync_action_name(Some(&request)), "pair");
        assert_eq!(sync_action_name(Some(&SyncManagementRequest::Discover)), "discover");
    }

    #[test]
    fn sync_error_codes_keep_lan_failures_actionable_without_logging_details() {
        assert_eq!(sync_error_code("LAN/VPN access requires a non-loopback listen address"), "lan_loopback_bind");
        assert_eq!(sync_error_code("a LAN/VPN listener needs an advertise host such as 192.168.1.20"), "lan_missing_advertise_host");
        assert_eq!(sync_error_code("cannot start sync listener: address already in use"), "listener_start");
    }

    #[test]
    #[ignore = "requires a host with multicast loopback support"]
    fn mdns_register_and_browse_round_trip() {
        let port = 54_543;
        let publisher = ServiceDaemon::new_with_port(port).unwrap();
        let browser = ServiceDaemon::new_with_port(port).unwrap();
        let properties = [("v", "1"), ("proto", "tokenme-device-sync/1"), ("role", "primary"), ("id", "qa-primary"), ("name", "TokenMe QA"), ("pin", "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), ("caps", "pair,upload")];
        let service = ServiceInfo::new(MDNS_SERVICE_TYPE, "TokenMe-QA", "tokenme-qa.local.", "", 47_821, &properties[..]).unwrap().enable_addr_auto();
        publisher.register(service).unwrap();
        let events = browser.browse(MDNS_SERVICE_TYPE).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut found = false;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if let Ok(ServiceEvent::ServiceResolved(service)) = events.recv_timeout(remaining) {
                if service.get_property_val_str("id") == Some("qa-primary") && service.get_port() == 47_821 {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "mDNS publisher was not resolved by the local browser");
        let _ = browser.stop_browse(MDNS_SERVICE_TYPE);
        let _ = publisher.unregister("TokenMe-QA._tokenme-sync._tcp.local.");
        let _ = browser.shutdown();
        let _ = publisher.shutdown();
    }
}

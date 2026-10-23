//! Bounded HTTPS over TCP/TLS 1.3 transport for the strict sync protocol.
//!
//! The transport is deliberately small: it implements only the two protocol
//! endpoints needed by pairing and upload, with a bounded HTTP/1.1 envelope.
//! It does not provide relay, NAT traversal, UDP, QUIC, SSH, or loopback-TCP
//! IPC. The server binds only to the caller-selected socket address.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering as AtomicOrdering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{verify_server_cert_signed_by_trust_anchor, verify_server_name};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, ServerConfig, ServerConnection, SignatureScheme, StreamOwned};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::diagnostics::{
    DiagnosticsError, DiagnosticsGrant, DiagnosticsHost, DiagnosticsMcpRequest, DiagnosticsMcpResponse,
    DiagnosticsScope, DiagnosticsService, DIAGNOSTICS_MCP_PATH, DIAGNOSTICS_REQUEST_TIMEOUT,
    MAX_DIAGNOSTICS_BODY_BYTES, MAX_DIAGNOSTICS_RESPONSE_BYTES,
};
use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};

use crate::protocol::{
    hash_bytes, verify_private_file, signature_serde, AuthenticatedDisplayNameRequest, AuthenticatedDownloadRequest,
    AuthenticatedStatus, AuthenticatedStatusRequest, DeviceIdentity, PairingInvite, PairingRequest, PrimaryDownloadPage,
    SignedUpload, StreamOpenReceipt, MAX_AUTH_REQUEST_AGE_MS,
    MAX_AUTH_REQUEST_FUTURE_SKEW_MS, MAX_COMPRESSED_BYTES, MAX_MANIFEST_BYTES, MAX_STRING_BYTES, PROTOCOL_ID,
};
use crate::{
    DeviceStatus, ImportReceipt, PairingRequestState, SyncError, SyncResult, SyncStore,
    protocol::identity_bytes,
};

const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_RESPONSE_BYTES: usize = MAX_DIAGNOSTICS_RESPONSE_BYTES;
const DEFAULT_BODY_BYTES: usize = 128 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(test)]
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_STAGING_QUOTA_BYTES: usize = 512 * 1024 * 1024;
const MAX_GLOBAL_CONCURRENT: usize = 16;
const MAX_CONTROL_BODY_BYTES: usize = 64 * 1024;
const MAX_PAIRING_PER_IP: usize = 5;
const PAIRING_WINDOW: Duration = Duration::from_secs(60);
const DISCOVERY_INVITE_TTL_MS: i64 = 5 * 60 * 1000;
const MAX_TRACKED_PAIRING_IPS: usize = 1024;
/// The HTTP wire contract. A peer that cannot name this major version is
/// rejected before its request body is consumed, so an upgraded primary and a
/// stale secondary fail with an actionable 426 instead of importing half a
/// package under different semantics.
const WIRE_VERSION_MAJOR: u16 = 2;
const WIRE_VERSION_MINOR: u16 = 0;
const WIRE_VERSION_HEADER: &str = "x-tokenme-wire-version";
const WIRE_REJECT_DRAIN_BYTES: usize = 128 * 1024;
const BUSY_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const DOWNLOAD_MAGIC: &[u8; 8] = b"TMPAGE01";
const DOWNLOAD_PAGE_VERSION: u8 = 1;
const DOWNLOAD_FLAG_ENABLED: u8 = 1;
const DOWNLOAD_FLAG_PREPARING: u8 = 2;
const DOWNLOAD_FLAG_COMPLETE: u8 = 4;
// Sized for the fastest legitimate cadence, not for a single device: a busy
// secondary mints ~60 nonces per minute (status plus snapshot pages) inside an
// ~11-minute retention window, so 16k entries leave room for dozens of secondaries.
const MAX_REPLAY_ENTRIES: usize = 16_384;
// A nonce is accepted for one endpoint once. Keep it at least as long as the
// signed request it belongs to can still pass the freshness check.
const REPLAY_RETENTION_MS: i64 =
    MAX_AUTH_REQUEST_AGE_MS + MAX_AUTH_REQUEST_FUTURE_SKEW_MS + 60_000;
const PAIRING_PATH: &str = "/v1/pairing/request";
const PAIRING_INVITE_PATH: &str = "/v1/pairing/invite";
const STATUS_PATH: &str = "/v1/status";
const DOWNLOAD_PATH: &str = "/v1/download";
const ROLLUP_PATH: &str = "/v1/rollup";
const DISPLAY_NAME_PATH: &str = "/v1/device/display-name";
const RECOVERY_PATH: &str = "/v1/recovery";
const DISCONNECT_PATH: &str = "/v1/disconnect";
pub const WS_SYNC_PATH: &str = "/v2/sync-ws";
const CONNECT_POLL_SLICE: Duration = Duration::from_millis(100);
// A transient accept failure (EMFILE, a peer that vanishes between SYN and
// accept) is not a reason to stop serving. Only a listener that has failed
// this many times in a row is handed back to the owner for a clean rebind.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 32;

const HEALTH_READY: u8 = 0;
const HEALTH_RUNNING: u8 = 1;
const HEALTH_STOPPING: u8 = 2;
const HEALTH_STOPPED: u8 = 3;

/// A panic in one connection poisons every interior lock it held. The service
/// recovers the guard instead of turning that one panic into a permanent
/// failure for every later request; the guarded state is either rebuilt per
/// request or covered by the store's own transaction rollback.
fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Per-connection failures are expected: SQLite can be busy, the disk can
/// reject a write, or a client can vanish mid-frame. Only a configuration
/// error means this listener cannot serve correctly.
fn connection_error_is_fatal(error: &TransportError) -> bool {
    error.is_terminal()
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport configuration is invalid: {0}")]
    Config(String),
    #[error("TLS handshake failed: {0}")]
    Tls(String),
    #[error("server certificate pin mismatch")]
    PinMismatch,
    #[error("transport I/O failed: {0}")]
    Io(#[source] io::Error),
    #[error("transport timed out")]
    Timeout,
    #[error("sync service is stopping")]
    Cancelled,
    #[error("HTTP status {status}: {message}")]
    HttpStatus { status: u16, message: String },
    #[error("transport response is invalid: {0}")]
    Protocol(String),
    #[error("sync peer requires a protocol upgrade: {0}")]
    Unsupported(String),
    #[error("transport body exceeds configured limit")]
    Limit,
    #[error("sync service is temporarily busy")]
    Busy,
    #[error("server rejected peer address {0}")]
    PeerRejected(IpAddr),
    #[error("sync state error: {0}")]
    State(#[source] SyncError),
    #[error("diagnostics request failed: {0}")]
    Diagnostics(#[source] DiagnosticsError),
}

impl TransportError {
    /// A configuration error makes a restarted listener fail the same way.
    /// Every other transport failure is scoped to one attempt and is worth a
    /// supervised retry.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Config(_))
    }
}

impl From<io::Error> for TransportError {
    fn from(error: io::Error) -> Self {
        if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
            Self::Timeout
        } else {
            Self::Io(error)
        }
    }
}

fn sync_error_code(error: &SyncError) -> &'static str {
    match error {
        SyncError::Sqlite(message) if message.contains("sqlite_code=DatabaseBusy") => "sqlite_busy",
        SyncError::Sqlite(message) if message.contains("sqlite_code=DatabaseLocked") => "sqlite_locked",
        SyncError::Sqlite(_) => "sqlite",
        SyncError::Io(..) => "io",
        _ => "sync_state",
    }
}

fn transport_error_code(error: &TransportError) -> &'static str {
    match error {
        TransportError::Config(_) => "configuration",
        TransportError::Tls(_) => "tls",
        TransportError::PinMismatch => "tls_pin",
        TransportError::Io(_) => "io",
        TransportError::Timeout => "timeout",
        TransportError::Cancelled => "cancelled",
        TransportError::HttpStatus { .. } => "http_status",
        TransportError::Protocol(_) => "protocol",
        TransportError::Unsupported(_) => "wire_version",
        TransportError::Limit => "limit",
        TransportError::Busy => "busy",
        TransportError::PeerRejected(_) => "peer_rejected",
        TransportError::State(error) => sync_error_code(error),
        TransportError::Diagnostics(_) => "diagnostics",
    }
}

#[derive(Debug, Clone)]
pub struct HttpsTransportConfig {
    /// HTTPS URL with an authority and optional path forbidden. Examples:
    /// `https://127.0.0.1:9443` and `https://[::1]:9443`.
    pub endpoint: String,
    /// DNS name used for TLS server-name verification. It may differ from the
    /// connect host when a local test uses `127.0.0.1` for a certificate issued
    /// to `localhost`.
    pub server_name: String,
    /// DER roots accepted during the TLS handshake. The `pinned` constructor
    /// puts the pinned leaf here, suitable for a self-issued LAN certificate.
    pub trust_anchors_der: Vec<Vec<u8>>,
    /// SHA-256 of the exact peer leaf DER certificate expected after the TLS
    /// handshake. This check happens before any HTTP request bytes exist.
    pub pinned_certificate_sha256: [u8; 32],
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
}

impl HttpsTransportConfig {
    pub fn pinned(endpoint: impl Into<String>, server_name: impl Into<String>, certificate_der: Vec<u8>) -> Self {
        Self {
            endpoint: endpoint.into(),
            server_name: server_name.into(),
            trust_anchors_der: vec![certificate_der.clone()],
            pinned_certificate_sha256: hash_bytes(&certificate_der),
            connect_timeout: DEFAULT_TIMEOUT,
            request_timeout: DEFAULT_TIMEOUT,
            max_response_bytes: MAX_RESPONSE_BYTES,
        }
    }

    /// Configures a client from a discovery-provided leaf hash. The first
    /// request is still TLS-pinned; the server returns the public certificate
    /// only after this handshake, so mDNS never carries the invite secret.
    pub fn pinned_hash(endpoint: impl Into<String>, server_name: impl Into<String>, certificate_pin: [u8; 32]) -> Self {
        Self {
            endpoint: endpoint.into(),
            server_name: server_name.into(),
            trust_anchors_der: Vec::new(),
            pinned_certificate_sha256: certificate_pin,
            connect_timeout: DEFAULT_TIMEOUT,
            request_timeout: DEFAULT_TIMEOUT,
            max_response_bytes: MAX_RESPONSE_BYTES,
        }
    }

    pub fn validate(&self) -> Result<ParsedEndpoint, TransportError> {
        let parsed = parse_endpoint(&self.endpoint)?;
        if self.server_name.trim().is_empty() {
            return Err(TransportError::Config("server_name must not be empty".into()));
        }
        if self.trust_anchors_der.is_empty() && self.pinned_certificate_sha256 == [0; 32] {
            return Err(TransportError::Config("at least one TLS trust anchor is required".into()));
        }
        if self.connect_timeout.is_zero() || self.request_timeout.is_zero() {
            return Err(TransportError::Config("timeouts must be positive".into()));
        }
        if self.max_response_bytes == 0 || self.max_response_bytes > MAX_RESPONSE_BYTES {
            return Err(TransportError::Config(format!("max_response_bytes must be in 1..={MAX_RESPONSE_BYTES}")));
        }
        Ok(parsed)
    }
}

#[derive(Debug, Clone)]
pub struct HttpsServerConfig {
    pub bind_addr: SocketAddr,
    pub certificate_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub allow_lan: bool,
    pub allowed_peer_ips: Vec<IpAddr>,
    pub max_body_bytes: usize,
    pub max_concurrent: usize,
    pub staging_dir: PathBuf,
    /// Maximum time a connection may make no read/write progress.
    pub idle_timeout: Duration,
    /// Absolute request deadline, including TLS, headers, staging and import.
    pub total_timeout: Duration,
    /// Aggregate bytes allowed in this server's private staging directory.
    pub staging_quota_bytes: usize,
}

impl HttpsServerConfig {
    pub fn validate(&self) -> Result<(), TransportError> {
        if !self.bind_addr.ip().is_loopback() && !self.allow_lan {
            return Err(TransportError::Config("non-loopback bind requires allow_lan=true".into()));
        }
        if self.certificate_der.is_empty() || self.private_key_der.is_empty() {
            return Err(TransportError::Config("certificate and private key DER must be present".into()));
        }
        if self.max_body_bytes == 0 || self.max_body_bytes > DEFAULT_BODY_BYTES {
            return Err(TransportError::Config(format!("max_body_bytes must be in 1..={DEFAULT_BODY_BYTES}")));
        }
        if self.max_concurrent == 0 || self.max_concurrent > MAX_GLOBAL_CONCURRENT {
            return Err(TransportError::Config(format!("max_concurrent must be in 1..={MAX_GLOBAL_CONCURRENT}")));
        }
        if self.staging_dir.as_os_str().is_empty() { return Err(TransportError::Config("staging_dir must not be empty".into())); }
        if self.idle_timeout.is_zero() || self.total_timeout.is_zero() || self.total_timeout < self.idle_timeout {
            return Err(TransportError::Config("idle_timeout must be positive and no greater than total_timeout".into()));
        }
        if self.staging_quota_bytes == 0 || self.staging_quota_bytes > DEFAULT_STAGING_QUOTA_BYTES {
            return Err(TransportError::Config(format!("staging_quota_bytes must be in 1..={DEFAULT_STAGING_QUOTA_BYTES}")));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedEndpoint {
    host_start: usize,
    host_end: usize,
    port: u16,
}

impl ParsedEndpoint {
    pub fn host<'a>(&self, endpoint: &'a str) -> &'a str {
        &endpoint[self.host_start..self.host_end]
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

pub struct Tls13HttpsTransport {
    config: HttpsTransportConfig,
    cancel: Arc<AtomicBool>,
    active_sockets: Arc<Mutex<BTreeMap<u64, TcpStream>>>,
    next_socket_id: Arc<AtomicU64>,
    ws_client: Arc<Mutex<Option<(tungstenite::protocol::WebSocket<StreamOwned<ClientConnection, TcpStream>>, ClientSocketGuard)>>>,
}

/// A TCP stream whose every read/write is bounded by one absolute request
/// deadline. `TcpStream::set_*_timeout` alone is only an idle timeout: a peer
/// that sends one byte before each idle timeout can otherwise keep a request
/// alive indefinitely. The cancellation flag is checked before every syscall
/// so a stopping runtime does not wait for the configured deadline.
struct DeadlineTcpStream {
    stream: TcpStream,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
}

impl DeadlineTcpStream {
    fn new(stream: TcpStream, deadline: Instant, cancel: Arc<AtomicBool>) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self { stream, deadline, cancel })
    }

    fn arm_read(&self) -> io::Result<()> {
        if self.cancel.load(AtomicOrdering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "transport cancelled"));
        }
        if Instant::now() >= self.deadline { return Err(io::Error::new(io::ErrorKind::TimedOut, "transport request deadline elapsed")); }
        Ok(())
    }

    fn arm_write(&self) -> io::Result<()> {
        if self.cancel.load(AtomicOrdering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "transport cancelled"));
        }
        if Instant::now() >= self.deadline { return Err(io::Error::new(io::ErrorKind::TimedOut, "transport request deadline elapsed")); }
        Ok(())
    }
}

impl Read for DeadlineTcpStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.arm_read()?;
            match self.stream.read(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && !self.cancel.load(AtomicOrdering::Acquire) && Instant::now() < self.deadline => { thread::sleep(CONNECT_POLL_SLICE.min(self.deadline.saturating_duration_since(Instant::now()))); continue; }
                result => return result,
            }
        }
    }
}

impl Write for DeadlineTcpStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.arm_write()?;
            match self.stream.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && !self.cancel.load(AtomicOrdering::Acquire) && Instant::now() < self.deadline => { thread::sleep(CONNECT_POLL_SLICE.min(self.deadline.saturating_duration_since(Instant::now()))); continue; }
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.arm_write()?;
        Ok(())
    }
}

/// Cancellation handle shared with the owner of a client runtime. Shutting
/// down the cloned TCP sockets is what wakes a blocking rustls read/write.
#[derive(Clone, Debug)]
pub struct ClientTransportCancellation {
    cancel: Arc<AtomicBool>,
    active_sockets: Arc<Mutex<BTreeMap<u64, TcpStream>>>,
}

impl ClientTransportCancellation {
    pub fn cancel(&self) {
        self.cancel.store(true, AtomicOrdering::Release);
        for stream in lock_recover(&self.active_sockets).values() { let _ = stream.shutdown(Shutdown::Both); }
    }
}

pub struct ClientSocketGuard {
    active_sockets: Arc<Mutex<BTreeMap<u64, TcpStream>>>,
    id: u64,
}

impl Drop for ClientSocketGuard {
    fn drop(&mut self) {
        lock_recover(&self.active_sockets).remove(&self.id);
    }
}

impl Tls13HttpsTransport {
    pub fn new(config: HttpsTransportConfig) -> Result<Self, TransportError> {
        config.validate()?;
        Ok(Self {
            config,
            cancel: Arc::new(AtomicBool::new(false)),
            active_sockets: Arc::new(Mutex::new(BTreeMap::new())),
            next_socket_id: Arc::new(AtomicU64::new(1)),
            ws_client: Arc::new(Mutex::new(None)),
        })
    }

    pub fn config(&self) -> &HttpsTransportConfig {
        &self.config
    }

    pub fn cancellation(&self) -> ClientTransportCancellation {
        ClientTransportCancellation { cancel: Arc::clone(&self.cancel), active_sockets: Arc::clone(&self.active_sockets) }
    }

    /// Checks that the configured endpoint accepts a TCP connection.  This is
    /// deliberately separate from [`Self::probe_tls`]: a reachable listener
    /// does not prove that its TLS certificate or pin is correct.
    pub fn probe_tcp(&self) -> Result<(), TransportError> {
        self.check_cancelled()?;
        let parsed = self.config.validate()?;
        let deadline = Instant::now().checked_add(self.config.connect_timeout).ok_or(TransportError::Timeout)?;
        let addr = resolve_connect_addr(parsed.host(&self.config.endpoint), parsed.port(), deadline, &self.cancel)?;
        let stream = connect_tcp_with_deadline(&addr, deadline, &self.cancel)?;
        drop(stream);
        self.check_cancelled()
    }

    /// Performs only the client TCP connection, TLS 1.3 handshake, and leaf
    /// certificate pin check.  It never sends HTTP bytes or enters any sync
    /// protocol endpoint.
    pub fn probe_tls(&self) -> Result<(), TransportError> {
        let (_stream, _socket) = self.connect()?;
        Ok(())
    }

    pub fn connect_ws(&self) -> Result<(tungstenite::protocol::WebSocket<StreamOwned<ClientConnection, TcpStream>>, ClientSocketGuard), TransportError> {
        self.check_cancelled()?;
        let parsed = self.config.validate()?;
        let deadline = Instant::now().checked_add(self.config.connect_timeout).ok_or(TransportError::Timeout)?;
        let addr = resolve_connect_addr(parsed.host(&self.config.endpoint), parsed.port(), deadline, &self.cancel)?;
        let tcp = connect_tcp_with_deadline(&addr, deadline, &self.cancel)?;
        let socket = self.register_socket(&tcp)?;
        tcp.set_nonblocking(false).map_err(|e| self.map_io(e))?;
        tcp.set_read_timeout(Some(Duration::from_secs(15))).map_err(|e| self.map_io(e))?;
        tcp.set_write_timeout(Some(Duration::from_secs(15))).map_err(|e| self.map_io(e))?;
        let client_config = client_config(&self.config)?;
        let server_name = ServerName::try_from(self.config.server_name.clone())
            .map_err(|_| TransportError::Config("server_name is not a valid DNS name".into()))?;
        let mut connection = ClientConnection::new(Arc::new(client_config), server_name)
            .map_err(|e| TransportError::Tls(e.to_string()))?;
        let mut tcp_stream = tcp;
        connection.complete_io(&mut tcp_stream).map_err(|e| self.map_io(e))?;
        self.check_cancelled()?;
        verify_peer_pin(&connection, self.config.pinned_certificate_sha256)?;
        let stream = StreamOwned::new(connection, tcp_stream);
        let ws_url = format!("wss://{}/v2/sync-ws", parsed.host(&self.config.endpoint));
        let (ws, _response) = tungstenite::client::client(ws_url, stream)
            .map_err(|e| TransportError::Protocol(format!("ws handshake: {e}")))?;
        Ok((ws, socket))
    }

    fn try_ws_upload(&mut self, upload: &SignedUpload) -> Result<ImportReceipt, TransportError> {
        let mut ws_guard = lock_recover(&self.ws_client);
        if ws_guard.is_none() {
            let (mut ws, socket) = self.connect_ws()?;
            let auth = crate::ws::WsSyncFrame::Auth {
                device_id: upload.envelope.device_id.clone(),
                origin: upload.envelope.origin.clone(),
                display_name: upload.envelope.origin.clone(),
                version: crate::ws::WS_PROTOCOL_VERSION.to_string(),
            };
            let auth_msg = serde_json::to_string(&auth).map_err(|e| TransportError::Protocol(e.to_string()))?;
            ws.send(tungstenite::Message::Text(auth_msg)).map_err(|e| TransportError::Protocol(e.to_string()))?;
            let ack_frame: crate::ws::WsSyncFrame = match ws.read() {
                Ok(tungstenite::Message::Text(t)) => serde_json::from_str(&t).map_err(|e| TransportError::Protocol(e.to_string()))?,
                _ => return Err(TransportError::Protocol("expected auth ack".into())),
            };
            match ack_frame {
                crate::ws::WsSyncFrame::AuthAck { status, .. } if status == "connected" => {},
                crate::ws::WsSyncFrame::AuthAck { error, .. } => {
                    return Err(TransportError::Protocol(error.unwrap_or_else(|| "auth failed".into())));
                }
                _ => return Err(TransportError::Protocol("invalid auth ack frame".into())),
            }
            *ws_guard = Some((ws, socket));
        }

        let Some((ws, _socket)) = ws_guard.as_mut() else {
            return Err(TransportError::Protocol("ws unavailable".into()));
        };

        let push_frame = crate::ws::WsSyncFrame::SyncPush {
            batch_id: format!("{}-{}", upload.envelope.device_id, upload.envelope.generation),
            envelope: upload.envelope.clone(),
            signature: upload.signature.to_vec(),
            gzip_bytes: upload.gzip_bytes.clone(),
        };
        let push_msg = serde_json::to_string(&push_frame).map_err(|e| TransportError::Protocol(e.to_string()))?;
        if let Err(e) = ws.send(tungstenite::Message::Text(push_msg)) {
            *ws_guard = None;
            return Err(TransportError::Protocol(format!("ws send failed: {e}")));
        }

        let reply = match ws.read() {
            Ok(tungstenite::Message::Text(t)) => serde_json::from_str::<crate::ws::WsSyncFrame>(&t)
                .map_err(|e| TransportError::Protocol(e.to_string()))?,
            Err(e) => {
                *ws_guard = None;
                return Err(TransportError::Protocol(format!("ws read reply failed: {e}")));
            }
            _ => {
                *ws_guard = None;
                return Err(TransportError::Protocol("unexpected non-text frame for sync ack".into()));
            }
        };

        match reply {
            crate::ws::WsSyncFrame::SyncAck { success: true, receipt: Some(receipt), .. } => {
                Ok(receipt)
            }
            crate::ws::WsSyncFrame::SyncAck { success: false, error, .. } => {
                // A rejection is a protocol-level answer, not a broken socket —
                // but the next attempt should meet the primary with a fresh
                // handshake anyway, and on a one-slot server the open leg IS
                // the blocker (new connections are shut down while slots are
                // held). Drop it so the retry cannot starve behind the leg
                // that just failed.
                *ws_guard = None;
                Err(TransportError::Protocol(error.unwrap_or_else(|| "sync failed on primary".into())))
            }
            _ => {
                *ws_guard = None;
                Err(TransportError::Protocol("expected sync ack frame with receipt".into()))
            }
        }
    }

    pub fn ws_upload(&mut self, upload: &SignedUpload) -> Result<ImportReceipt, TransportError> {
        // A cancelled transport must surface as `Cancelled` exactly like the
        // retired one-shot HTTP upload did, even when the interrupt lands in
        // the middle of a frame read instead of before a syscall.
        self.try_ws_upload(upload).map_err(|error| self.map_cancelled(error))
    }

    /// Drops the cached WebSocket, if any. The upload leg holds the socket only
    /// for the outbox drain it serves: a primary admitting one connection at a
    /// time (`max_concurrent: 1`) rejects every later HTTP request while a
    /// socket sits open, so the runtime closes the leg between passes and the
    /// next pass's status/download calls always find a free slot.
    pub fn ws_close(&mut self) {
        *lock_recover(&self.ws_client) = None;
    }

    /// Sends the pairing secret only after TLS1.3 is complete and the peer
    /// leaf certificate has matched the configured SHA-256 pin.
    pub fn send_pairing_request(&mut self, request: &PairingRequest) -> Result<PairingRequestState, TransportError> {
        let body = WirePairingRequest::from_request(request)?.encode()?;
        let response = self.request(PAIRING_PATH, body)?;
        response.json()
    }

    /// Requests a short-lived invite over a certificate-pin-only TLS channel.
    /// The primary still requires the normal signed pairing request and an
    /// explicit approval after this method returns.
    pub fn request_pairing_invite(&mut self, primary_id: &str, endpoint: &str) -> Result<(PairingInvite, Vec<u8>), TransportError> {
        let (invite, certificate_der, _, _) = self.request_pairing_invite_with_primary_name(primary_id, endpoint)?;
        Ok((invite, certificate_der))
    }

    pub fn request_pairing_invite_with_primary_name(
        &mut self,
        primary_id: &str,
        endpoint: &str,
    ) -> Result<(PairingInvite, Vec<u8>, Option<String>, Option<String>), TransportError> {
        let body = serde_json::to_vec(&WirePairingInviteRequest { primary_id: primary_id.to_string(), endpoint: endpoint.to_string() })
            .map_err(|e| TransportError::Protocol(format!("encode discovery pairing request: {e}")))?;
        let response = self.request(PAIRING_INVITE_PATH, body)?;
        response.json::<WirePairingInvite>()?.into_invite()
    }

    pub fn status(&mut self, request: &AuthenticatedStatusRequest) -> Result<AuthenticatedStatus, TransportError> {
        let body = serde_json::to_vec(request).map_err(|e| TransportError::Protocol(format!("encode status request: {e}")))?;
        let response = self.request(STATUS_PATH, body)?;
        let status: AuthenticatedStatus = response.json()?;
        if status.request_nonce != request.nonce || status.device_id != request.device_id || status.primary_id != request.primary_id {
            return Err(TransportError::Protocol("authenticated status response does not match request nonce or identity".into()));
        }
        Ok(status)
    }

    pub fn download_primary_page(&mut self, request: &AuthenticatedDownloadRequest) -> Result<PrimaryDownloadPage, TransportError> {
        let body = serde_json::to_vec(request).map_err(|e| TransportError::Protocol(format!("encode download request: {e}")))?;
        let response = self.request(DOWNLOAD_PATH, body)?;
        if response.status != 200 {
            response.json::<serde_json::Value>()?;
            return Err(TransportError::Protocol("primary download was accepted without a page body".into()));
        }
        decode_primary_download_page(&response.body, request)
    }

    /// Fetches the primary's own local-origin rollup rows as one snapshot.
    /// The answer's `origin` is the label the secondary groups the rows under
    /// (the primary's hostname, the same label its merged rows have always
    /// carried); each row's own `origin` field is empty by construction.
    pub fn rollup_snapshot(&mut self, request: &AuthenticatedRollupRequest) -> Result<RollupSnapshot, TransportError> {
        let body = serde_json::to_vec(request).map_err(|e| TransportError::Protocol(format!("encode rollup request: {e}")))?;
        let response = self.request(ROLLUP_PATH, body)?;
        if response.status != 200 {
            let message = serde_json::from_slice::<WireError>(&response.body)
                .ok()
                .map(|error| error.message)
                .unwrap_or_else(|| "rollup request rejected".into());
            return Err(TransportError::HttpStatus { status: response.status, message });
        }
        let snapshot: RollupSnapshot = response.json()?;
        if snapshot.origin.is_empty() {
            return Err(TransportError::Protocol("rollup snapshot does not declare an origin".into()));
        }
        Ok(snapshot)
    }

    pub fn update_display_name(&mut self, request: &AuthenticatedDisplayNameRequest) -> Result<(), TransportError> {
        let body = serde_json::to_vec(request).map_err(|e| TransportError::Protocol(format!("encode display-name update: {e}")))?;
        let response = self.request(DISPLAY_NAME_PATH, body)?;
        response.json::<serde_json::Value>()?;
        Ok(())
    }

    pub fn recover(&mut self, request: &AuthenticatedStatusRequest) -> Result<StreamOpenReceipt, TransportError> {
        if !request.want_recovery { return Err(TransportError::Protocol("recovery request must set want_recovery".into())); }
        let body = serde_json::to_vec(request).map_err(|e| TransportError::Protocol(format!("encode recovery request: {e}")))?;
        let response = self.request(RECOVERY_PATH, body)?;
        let receipt: StreamOpenReceipt = response.json()?;
        if receipt.primary_id != request.primary_id || receipt.device_id != request.device_id || receipt.data_epoch == 0 || receipt.expected_generation == 0 {
            return Err(TransportError::Protocol("recovery response does not match request identity".into()));
        }
        Ok(receipt)
    }

    pub fn disconnect(&mut self, request: &AuthenticatedStatusRequest) -> Result<(), TransportError> {
        let body = serde_json::to_vec(request).map_err(|e| TransportError::Protocol(format!("encode disconnect request: {e}")))?;
        let response = self.request(DISCONNECT_PATH, body)?;
        if response.status != 200 {
            let message = serde_json::from_slice::<WireError>(&response.body)
                .ok()
                .map(|error| error.message)
                .unwrap_or_else(|| "disconnect request rejected".into());
            return Err(TransportError::HttpStatus { status: response.status, message });
        }
        Ok(())
    }

    /// Sends one authenticated MCP JSON-RPC request over the same TLS 1.3 and
    /// leaf-certificate pinning path as sync. The diagnostics route is a
    /// separate application endpoint and never enters upload framing.
    pub fn diagnostics_mcp(&mut self, request: &DiagnosticsMcpRequest) -> Result<Option<DiagnosticsMcpResponse>, TransportError> {
        let body = serde_json::to_vec(request).map_err(|e| TransportError::Protocol(format!("encode diagnostics MCP request: {e}")))?;
        if body.len() > MAX_DIAGNOSTICS_BODY_BYTES {
            return Err(TransportError::Limit);
        }
        let response = self.request(DIAGNOSTICS_MCP_PATH, body)?;
        if response.status != 200 {
            let message = serde_json::from_slice::<DiagnosticsMcpResponse>(&response.body)
                .ok()
                .and_then(|value| value.error.map(|error| error.message))
                .or_else(|| serde_json::from_slice::<WireError>(&response.body).ok().map(|error| error.message))
                .unwrap_or_else(|| "diagnostics request rejected".into());
            return Err(TransportError::HttpStatus { status: response.status, message });
        }
        if response.body.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&response.body).map(Some).map_err(|e| TransportError::Protocol(format!("invalid diagnostics MCP response: {e}")))
    }

    fn request(&self, path: &str, body: Vec<u8>) -> Result<HttpResponse, TransportError> {
        if body.len() > DEFAULT_BODY_BYTES {
            return Err(TransportError::Limit);
        }
        let (mut stream, _socket) = self.connect()?;
        self.check_cancelled()?;
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{WIRE_VERSION_HEADER}: {WIRE_VERSION_MAJOR}.{WIRE_VERSION_MINOR}\r\nConnection: close\r\n\r\n",
            self.config.server_name, body.len()
        );
        stream.write_all(request.as_bytes()).map_err(|error| self.map_io(error))?;
        stream.write_all(&body).map_err(|error| self.map_io(error))?;
        stream.flush().map_err(|error| self.map_io(error))?;
        read_response(&mut stream, self.config.max_response_bytes).map_err(|error| self.map_cancelled(error))
    }

    fn connect(&self) -> Result<(StreamOwned<ClientConnection, DeadlineTcpStream>, ClientSocketGuard), TransportError> {
        self.check_cancelled()?;
        let parsed = self.config.validate()?;
        let deadline = Instant::now().checked_add(self.config.request_timeout).ok_or(TransportError::Timeout)?;
        let connect_deadline = deadline.min(Instant::now().checked_add(self.config.connect_timeout).ok_or(TransportError::Timeout)?);
        let addr = resolve_connect_addr(parsed.host(&self.config.endpoint), parsed.port(), connect_deadline, &self.cancel)?;
        let tcp = connect_tcp_with_deadline(&addr, connect_deadline, &self.cancel)?;
        let socket = self.register_socket(&tcp)?;
        let mut tcp = DeadlineTcpStream::new(tcp, deadline, Arc::clone(&self.cancel)).map_err(|error| self.map_io(error))?;
        let client_config = client_config(&self.config)?;
        let server_name = ServerName::try_from(self.config.server_name.clone())
            .map_err(|_| TransportError::Config("server_name is not a valid DNS name".into()))?;
        let mut connection = ClientConnection::new(Arc::new(client_config), server_name)
            .map_err(|e| TransportError::Tls(e.to_string()))?;
        connection.complete_io(&mut tcp).map_err(|error| self.map_io(error))?;
        self.check_cancelled()?;
        verify_peer_pin(&connection, self.config.pinned_certificate_sha256)?;
        Ok((StreamOwned::new(connection, tcp), socket))
    }

    fn register_socket(&self, stream: &TcpStream) -> Result<ClientSocketGuard, TransportError> {
        let id = self.next_socket_id.fetch_add(1, AtomicOrdering::Relaxed);
        let clone = stream.try_clone().map_err(|error| self.map_io(error))?;
        let mut active = lock_recover(&self.active_sockets);
        if self.cancel.load(AtomicOrdering::Acquire) {
            let _ = clone.shutdown(Shutdown::Both);
            return Err(TransportError::Cancelled);
        }
        active.insert(id, clone);
        Ok(ClientSocketGuard { active_sockets: Arc::clone(&self.active_sockets), id })
    }

    fn check_cancelled(&self) -> Result<(), TransportError> {
        if self.cancel.load(AtomicOrdering::Acquire) { Err(TransportError::Cancelled) } else { Ok(()) }
    }

    fn map_io(&self, error: io::Error) -> TransportError {
        if self.cancel.load(AtomicOrdering::Acquire) { TransportError::Cancelled } else { TransportError::from(error) }
    }

    fn map_cancelled(&self, error: TransportError) -> TransportError {
        if self.cancel.load(AtomicOrdering::Acquire) { TransportError::Cancelled } else { error }
    }
}

pub struct HttpsSyncServer {
    listener: TcpListener,
    context: Arc<ServerContext>,
}

/// Cloneable control handle for the diagnostics authority owned by a running
/// HTTPS server.  It intentionally exposes grant/session controls and the
/// host data setters only; the listener and sync store remain private to the
/// service loop.
#[derive(Clone)]
pub struct HttpsSyncServerHandle {
    context: Arc<ServerContext>,
}

impl HttpsSyncServerHandle {
    /// Returns a stable, non-secret service liveness state for unattended
    /// operators. `ready` means bound but not serving yet; `running` means the
    /// accept loop is active; `stopping` means active requests are being
    /// cancelled; and `stopped` means all handlers have been joined.
    pub fn health(&self) -> &'static str {
        self.context.health()
    }

    pub fn issue_diagnostics_grant(
        &self,
        debugger_device_id: &str,
        scopes: &[DiagnosticsScope],
        issued_at_ms: i64,
        ttl: Duration,
    ) -> Result<DiagnosticsGrant, TransportError> {
        let ttl_ms = i64::try_from(ttl.as_millis()).map_err(|_| TransportError::Config("diagnostics grant TTL is too large".into()))?;
        let store = lock_recover(&self.context.store);
        self.context.diagnostics.issue_grant(&store, debugger_device_id, scopes, issued_at_ms, ttl_ms).map_err(TransportError::Diagnostics)
    }

    pub fn revoke_diagnostics_grant(&self, grant_id: &str) -> Result<(), TransportError> {
        self.context.diagnostics.revoke_grant(grant_id).map_err(TransportError::Diagnostics)
    }

    pub fn revoke_all_diagnostics(&self) {
        self.context.diagnostics.revoke_all();
    }

    pub fn diagnostics_audit_events(&self) -> Result<Vec<crate::diagnostics::DiagnosticsAuditEvent>, TransportError> {
        self.context.diagnostics.audit_events().map_err(TransportError::Diagnostics)
    }

    pub fn diagnostics_host(&self) -> DiagnosticsHost {
        self.context.diagnostics.host()
    }
}

struct ServerContext {
    tls_config: Arc<ServerConfig>,
    store: Arc<Mutex<SyncStore>>,
    #[cfg(test)]
    fault_next_connection: AtomicU8,
    diagnostics: Arc<DiagnosticsService>,
    pairing_limiter: PairingRateLimiter,
    gate: Arc<ConcurrencyGate>,
    config: HttpsServerConfig,
    cancel: Arc<AtomicBool>,
    health: AtomicU8,
    active_sockets: Mutex<BTreeMap<u64, TcpStream>>,
    next_socket_id: AtomicU64,
    log_sink: Mutex<Option<Arc<dyn Fn(bool, &str) + Send + Sync>>>,
    /// Embedding-UI wake for pairing requests that persist out-of-band (fresh
    /// row, or an auto-approve landing) so the panel need not poll for them.
    /// Receives the requesting device's display name.
    pairing_notify: Mutex<Option<Arc<dyn Fn(&str, bool) + Send + Sync>>>,
    disconnect_sink: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    replay: Mutex<NonceReplayCache>,
}

struct ActiveSocketGuard<'a> {
    context: &'a ServerContext,
    id: u64,
}

impl Drop for ActiveSocketGuard<'_> {
    fn drop(&mut self) {
        self.context.unregister_socket(self.id);
    }
}

struct PairingRateLimiter {
    attempts: Mutex<BTreeMap<IpAddr, Vec<Instant>>>,
}

impl PairingRateLimiter {
    fn new() -> Self {
        Self { attempts: Mutex::new(BTreeMap::new()) }
    }

    fn allow(&self, ip: IpAddr) -> bool {
        let mut attempts = lock_recover(&self.attempts);
        let now = Instant::now();
        let cutoff = now.checked_sub(PAIRING_WINDOW).unwrap_or(now);
        attempts.retain(|_, values| {
            values.retain(|at| *at > cutoff);
            !values.is_empty()
        });
        if attempts.len() >= MAX_TRACKED_PAIRING_IPS && !attempts.contains_key(&ip) {
            return false;
        }
        let bucket = attempts.entry(ip).or_default();
        if bucket.len() >= MAX_PAIRING_PER_IP {
            return false;
        }
        bucket.push(now);
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NonceClaim {
    Fresh,
    Replay,
    Full,
}

/// One signed request may be answered once. Requests are scoped by endpoint
/// because the status/recovery handshake deliberately reuses one nonce across
/// two paths; recovery keeps its own durable replay record in the store.
#[derive(Default)]
struct NonceReplayCache {
    claims: BTreeMap<([u8; 16], &'static str), i64>,
}

impl NonceReplayCache {
    fn claim(&mut self, scope: &'static str, nonce: [u8; 16], now_ms: i64) -> NonceClaim {
        let cutoff = now_ms.saturating_sub(REPLAY_RETENTION_MS);
        self.claims.retain(|_, claimed_at| *claimed_at > cutoff);
        let key = (nonce, scope);
        if self.claims.contains_key(&key) {
            return NonceClaim::Replay;
        }
        if self.claims.len() >= MAX_REPLAY_ENTRIES {
            return NonceClaim::Full;
        }
        self.claims.insert(key, now_ms);
        NonceClaim::Fresh
    }

    fn release(&mut self, scope: &'static str, nonce: [u8; 16]) {
        self.claims.remove(&(nonce, scope));
    }
}

const NONCE_SCOPE_STATUS: &str = "status";
const NONCE_SCOPE_DOWNLOAD: &str = "download";
const NONCE_SCOPE_ROLLUP: &str = "rollup";
const NONCE_SCOPE_DISPLAY_NAME: &str = "display-name";
const NONCE_SCOPE_DISCONNECT: &str = "disconnect";

fn claim_request_nonce(
    replay: &Mutex<NonceReplayCache>,
    scope: &'static str,
    nonce: [u8; 16],
    now_ms: i64,
) -> Result<(), SyncError> {
    let mut cache = lock_recover(replay);
    match cache.claim(scope, nonce, now_ms) {
        NonceClaim::Fresh => Ok(()),
        NonceClaim::Replay => Err(SyncError::Auth("signed request was already answered".into())),
        NonceClaim::Full => Err(SyncError::Limit("sync request replay cache is full".into())),
    }
}

fn release_request_nonce(replay: &Mutex<NonceReplayCache>, scope: &'static str, nonce: [u8; 16]) {
    lock_recover(replay).release(scope, nonce);
}

/// Claims the nonce before the side effect and releases it only when the
/// action failed; a rejected or unauthenticated request cannot exhaust the
/// cache, while a retry after a transient failure stays possible.
fn with_claimed_nonce<T>(
    replay: &Mutex<NonceReplayCache>,
    scope: &'static str,
    nonce: [u8; 16],
    now_ms: i64,
    action: impl FnOnce() -> Result<T, SyncError>,
) -> Result<T, SyncError> {
    claim_request_nonce(replay, scope, nonce, now_ms)?;
    match action() {
        Ok(value) => Ok(value),
        Err(error) => {
            release_request_nonce(replay, scope, nonce);
            Err(error)
        }
    }
}

struct GateState {
    active: usize,
}

struct ConcurrencyGate {
    state: Mutex<GateState>,
    changed: Condvar,
    max_concurrent: usize,
}

struct ConnectionPermit {
    gate: Arc<ConcurrencyGate>,
}

impl ConcurrencyGate {
    fn new(config: &HttpsServerConfig) -> Self {
        Self {
            state: Mutex::new(GateState { active: 0 }),
            changed: Condvar::new(),
            max_concurrent: config.max_concurrent,
        }
    }

    fn try_acquire_connection(self: &Arc<Self>) -> Result<Option<ConnectionPermit>, TransportError> {
        let mut state = lock_recover(&self.state);
        if state.active >= self.max_concurrent {
            return Ok(None);
        }
        state.active += 1;
        Ok(Some(ConnectionPermit { gate: Arc::clone(self) }))
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let mut state = lock_recover(&self.gate.state);
        state.active = state.active.saturating_sub(1);
        self.gate.changed.notify_all();
    }
}

/// Runs one connection to completion behind a panic boundary. Only a
/// configuration error is returned to the listener; a request-scoped failure
/// or a panic closes this connection and leaves the service accepting.
fn handle_connection_isolated(
    context: &ServerContext,
    stream: TcpStream,
    peer: SocketAddr,
    permit: ConnectionPermit,
) -> Option<TransportError> {
    let started = Instant::now();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        context.handle_connection(stream, peer, permit)
    })) {
        Ok(Ok(())) => None,
        Ok(Err(error)) => {
            let code = transport_error_code(&error);
            context.log_event(true, &format!(
                "sync: action=connection outcome=error error_code={code} total_ms={}",
                started.elapsed().as_millis(),
            ));
            connection_error_is_fatal(&error).then_some(error)
        }
        Err(_) => {
            context.log_event(true, &format!(
                "sync: action=connection outcome=panic total_ms={}",
                started.elapsed().as_millis(),
            ));
            None
        }
    }
}

impl HttpsSyncServer {
    pub fn bind(config: HttpsServerConfig, store: SyncStore) -> Result<Self, TransportError> {
        Self::bind_with_host(config, store, DiagnosticsHost::default())
    }

    /// Bind a server with the embedding host's explicitly registered
    /// diagnostics data.  The host handle is shared with the caller, so the
    /// panel/runtime can append trace events and update check results while
    /// the HTTPS server is running without exposing arbitrary host access.
    /// Bind the caller-selected address exactly. A port owned by another
    /// process is a configuration error, not an invitation to pick a random
    /// port the pairing invite never advertised.
    pub fn bind_with_host(config: HttpsServerConfig, store: SyncStore, host: DiagnosticsHost) -> Result<Self, TransportError> {
        config.validate()?;
        crate::protocol::ensure_private_dir(&config.staging_dir).map_err(TransportError::State)?;
        let tls_config = server_config(&config)?;
        let listener = TcpListener::bind(config.bind_addr).map_err(|error| {
            if error.kind() == io::ErrorKind::AddrInUse {
                TransportError::Config(format!(
                    "sync port {} is already in use; stop the other TokenMe sync service or choose a different port",
                    config.bind_addr.port(),
                ))
            } else {
                TransportError::from(error)
            }
        })?;
        listener.set_nonblocking(false).map_err(TransportError::from)?;
        // A process that died mid-upload leaves `upload-*.part` behind, and
        // the in-memory quota counter cannot see those bytes. Only this server
        // can own the staging directory (the port is bound), and anything older
        // than twice the longest request cannot belong to a live upload.
        sweep_stale_staging(&config.staging_dir, config.total_timeout.saturating_mul(2), SystemTime::now());
        let diagnostics = DiagnosticsService::new_with_host(store.primary_id(), host).map_err(TransportError::Diagnostics)?;
        let context = ServerContext {
            tls_config: Arc::new(tls_config),
            store: Arc::new(Mutex::new(store)),
            #[cfg(test)]
            fault_next_connection: AtomicU8::new(0),
            diagnostics: Arc::new(diagnostics),
            pairing_limiter: PairingRateLimiter::new(),
            gate: Arc::new(ConcurrencyGate::new(&config)),
            config,
            cancel: Arc::new(AtomicBool::new(false)),
            health: AtomicU8::new(HEALTH_READY),
            active_sockets: Mutex::new(BTreeMap::new()),
            next_socket_id: AtomicU64::new(1),
            log_sink: Mutex::new(None),
            pairing_notify: Mutex::new(None),
            disconnect_sink: Mutex::new(None),
            replay: Mutex::new(NonceReplayCache::default()),
        };
        Ok(Self { listener, context: Arc::new(context) })
    }

    /// Returns a clone of the host-owned diagnostics handle used by this
    /// server.  It contains only the allow-listed log/trace/check setters.
    pub fn diagnostics_host(&self) -> DiagnosticsHost {
        self.context.diagnostics.host()
    }

    /// Routes safe runtime events to the embedding application's log.
    pub fn set_log_sink(
        &self,
        sink: impl Fn(bool, &str) + Send + Sync + 'static,
    ) -> Result<(), TransportError> {
        *lock_recover(&self.context.log_sink) = Some(Arc::new(sink));
        Ok(())
    }

    /// Registers the embedding UI's wake for pairing requests that persist
    /// out-of-band of any panel command: a fresh pending row, or an
    /// auto-approve landing on arrival. The hook receives the requesting
    /// device's display name and whether it was auto-approved, runs on the
    /// connection thread and must be cheap (the panel emits an event, maybe a
    /// toast, and returns).
    pub fn set_pairing_notify(
        &self,
        hook: impl Fn(&str, bool) + Send + Sync + 'static,
    ) -> Result<(), TransportError> {
        *lock_recover(&self.context.pairing_notify) = Some(Arc::new(hook));
        Ok(())
    }

    /// Registers a listener callback invoked when a secondary device explicitly disconnects.
    pub fn set_disconnect_sink(
        &self,
        sink: impl Fn(&str) + Send + Sync + 'static,
    ) -> Result<(), TransportError> {
        *lock_recover(&self.context.disconnect_sink) = Some(Arc::new(sink));
        Ok(())
    }

    /// Issues a separately-scoped, short-lived diagnostics grant for an
    /// already paired debugger device. The grant authority is process-local;
    /// a restart therefore does not renew old diagnostics access.
    pub fn issue_diagnostics_grant(
        &self,
        debugger_device_id: &str,
        scopes: &[DiagnosticsScope],
        issued_at_ms: i64,
        ttl: Duration,
    ) -> Result<DiagnosticsGrant, TransportError> {
        self.diagnostics_handle().issue_diagnostics_grant(debugger_device_id, scopes, issued_at_ms, ttl)
    }

    pub fn revoke_diagnostics_grant(&self, grant_id: &str) -> Result<(), TransportError> {
        self.diagnostics_handle().revoke_diagnostics_grant(grant_id)
    }

    pub fn revoke_all_diagnostics(&self) {
        self.diagnostics_handle().revoke_all_diagnostics();
    }

    pub fn diagnostics_audit_events(&self) -> Result<Vec<crate::diagnostics::DiagnosticsAuditEvent>, TransportError> {
        self.diagnostics_handle().diagnostics_audit_events()
    }

    pub fn diagnostics_handle(&self) -> HttpsSyncServerHandle {
        HttpsSyncServerHandle { context: Arc::clone(&self.context) }
    }

    /// Returns a stable liveness state without touching the sync store. This
    /// is safe to call from a status/health observer while the service loop is
    /// serving requests or joining after cancellation.
    pub fn health(&self) -> &'static str {
        self.context.health()
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        self.listener.local_addr().map_err(TransportError::from)
    }

    /// Serves exactly `connections` requests. This is useful for controlled
    /// lifecycle tests and for a caller-owned user service.
    pub fn run_connections(&self, connections: usize) -> Result<(), TransportError> {
        self.context.cancel.store(false, AtomicOrdering::Release);
        self.context.health.store(HEALTH_RUNNING, AtomicOrdering::Release);
        let result = (|| {
            for _ in 0..connections {
                let (stream, peer) = self.listener.accept().map_err(TransportError::from)?;
                let Some(permit) = self.context.gate.try_acquire_connection()? else {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                };
                if let Some(error) = handle_connection_isolated(&self.context, stream, peer, permit) {
                    return Err(error);
                }
            }
            Ok(())
        })();
        self.context.health.store(HEALTH_STOPPING, AtomicOrdering::Release);
        self.context.cancel_active();
        self.context.health.store(HEALTH_STOPPED, AtomicOrdering::Release);
        result
    }

    /// Blocking service loop. The caller owns process lifecycle and shutdown.
    pub fn run(self) -> Result<(), TransportError> {
        let stop = Arc::new(AtomicBool::new(false));
        self.run_until(&stop)
    }

    /// Run until the caller flips `stop`. Active requests retain the same
    /// total deadline and are not accepted after cancellation.
    pub fn run_until(self, stop: &AtomicBool) -> Result<(), TransportError> {
        let HttpsSyncServer { listener, context } = self;
        context.cancel.store(false, AtomicOrdering::Release);
        context.health.store(HEALTH_RUNNING, AtomicOrdering::Release);
        listener.set_nonblocking(true).map_err(TransportError::from)?;
        let (fatal_tx, fatal_rx) = mpsc::channel();
        let mut handlers: Vec<thread::JoinHandle<()>> = Vec::new();
        let mut consecutive_accept_errors = 0_u32;
        while !stop.load(AtomicOrdering::Acquire) {
            if let Ok(error) = fatal_rx.try_recv() {
                context.cancel_active();
                for handler in handlers { let _ = handler.join(); }
                context.health.store(HEALTH_STOPPED, AtomicOrdering::Release);
                return Err(error);
            }
            let mut index = 0;
            while index < handlers.len() {
                if handlers[index].is_finished() {
                    let handler = handlers.swap_remove(index);
                    let _ = handler.join();
                } else {
                    index += 1;
                }
            }
            match listener.accept() {
                Ok((stream, peer)) => {
                    consecutive_accept_errors = 0;
                    let permit = match context.gate.try_acquire_connection() {
                        Ok(Some(permit)) => permit,
                        Ok(None) => {
                            let _ = stream.shutdown(Shutdown::Both);
                            continue;
                        }
                        Err(error) => {
                            context.cancel_active();
                            for handler in handlers { let _ = handler.join(); }
                            context.health.store(HEALTH_STOPPED, AtomicOrdering::Release);
                            return Err(error);
                        }
                    };
                    let context = context.clone();
                    let fatal_tx = fatal_tx.clone();
                    handlers.push(thread::spawn(move || {
                        if let Some(error) = handle_connection_isolated(&context, stream, peer, permit) {
                            let _ = fatal_tx.send(error);
                        }
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    consecutive_accept_errors = consecutive_accept_errors.saturating_add(1);
                    context.log_event(true, &format!(
                        "sync: action=accept outcome=error error_kind={:?} consecutive={consecutive_accept_errors}",
                        error.kind(),
                    ));
                    if consecutive_accept_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                        context.cancel_active();
                        for handler in handlers { let _ = handler.join(); }
                        context.health.store(HEALTH_STOPPED, AtomicOrdering::Release);
                        return Err(TransportError::from(error));
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
        context.health.store(HEALTH_STOPPING, AtomicOrdering::Release);
        context.cancel_active();
        let mut fatal = fatal_rx.try_recv().ok();
        for handler in handlers {
            let _ = handler.join();
        }
        if fatal.is_none() { fatal = fatal_rx.try_recv().ok(); }
        context.health.store(HEALTH_STOPPED, AtomicOrdering::Release);
        match fatal { Some(error) => Err(error), None => Ok(()) }
    }
}

impl ServerContext {
    fn log_event(&self, is_error: bool, message: &str) {
        let sink = lock_recover(&self.log_sink).clone();
        if let Some(sink) = sink {
            sink(is_error, message);
        } else if is_error {
            eprintln!("tokenme sync: {message}");
        }
    }

    fn notify_disconnect(&self, device_id: &str) {
        let sink = lock_recover(&self.disconnect_sink).clone();
        if let Some(sink) = sink {
            sink(device_id);
        }
    }

    fn health(&self) -> &'static str {
        match self.health.load(AtomicOrdering::Acquire) {
            HEALTH_READY => "ready",
            HEALTH_RUNNING => "running",
            HEALTH_STOPPING => "stopping",
            HEALTH_STOPPED => "stopped",
            _ => "unknown",
        }
    }

    fn register_socket(&self, stream: &TcpStream) -> Result<ActiveSocketGuard<'_>, TransportError> {
        let id = self.next_socket_id.fetch_add(1, AtomicOrdering::Relaxed);
        let clone = stream.try_clone().map_err(TransportError::from)?;
        let mut active = lock_recover(&self.active_sockets);
        if self.cancel.load(AtomicOrdering::Acquire) {
            let _ = clone.shutdown(Shutdown::Both);
            return Err(TransportError::Cancelled);
        }
        active.insert(id, clone);
        Ok(ActiveSocketGuard { context: self, id })
    }

    fn cancel_active(&self) {
        self.diagnostics.stop();
        self.cancel.store(true, AtomicOrdering::Release);
        for stream in lock_recover(&self.active_sockets).values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn unregister_socket(&self, id: u64) {
        lock_recover(&self.active_sockets).remove(&id);
    }

    fn handle_connection(&self, stream: TcpStream, peer: SocketAddr, _connection: ConnectionPermit) -> Result<(), TransportError> {
        #[cfg(test)]
        match self.fault_next_connection.swap(0, AtomicOrdering::AcqRel) {
            1 => return Err(TransportError::State(SyncError::Protocol("injected connection fault".into()))),
            2 => panic!("injected connection panic"),
            _ => {}
        }
        if self.cancel.load(AtomicOrdering::Acquire) { return Err(TransportError::Cancelled); }
        if !self.config.allowed_peer_ips.is_empty() && !self.config.allowed_peer_ips.contains(&peer.ip()) {
            return Err(TransportError::PeerRejected(peer.ip()));
        }
        // `run_until` polls a nonblocking listener; on macOS accepted sockets
        // may inherit that flag, while the bounded TLS/HTTP reader relies on
        // socket timeouts rather than WouldBlock polling.
        stream.set_nonblocking(false).map_err(TransportError::from)?;
        let _active_socket = self.register_socket(&stream)?;
        let deadline = Instant::now().checked_add(self.config.total_timeout).ok_or(TransportError::Timeout)?;
        stream.set_read_timeout(Some(self.config.idle_timeout)).map_err(TransportError::from)?;
        stream.set_write_timeout(Some(self.config.idle_timeout)).map_err(TransportError::from)?;
        let connection = ServerConnection::new(self.tls_config.clone()).map_err(|e| TransportError::Tls(e.to_string()))?;
        let mut tls = StreamOwned::new(connection, stream);
        let head = read_request_head(&mut tls, self.config.max_body_bytes, deadline, &self.cancel)?;
        if head.is_websocket {
            let prefixed = PrefixedStream { prefix: std::io::Cursor::new(head.raw_header), inner: tls };
            let ws = tungstenite::accept(prefixed).map_err(|e| TransportError::Protocol(format!("ws accept: {e}")))?;
            self.log_event(false, "sync: action=ws_connect outcome=ok");
            let store = Arc::clone(&self.store);
            crate::ws::handle_server_websocket(ws, store, now_ms, &self.cancel).map_err(TransportError::State)?;
            return Ok(());
        }
        if head.known_path.is_some() && head.wire_version.is_none_or(|(major, _)| major != WIRE_VERSION_MAJOR) {
            self.reject_wire_version(&mut tls, &head)?;
            return Ok(());
        }
        let (status, response) = if head.known_path.is_none() {
            (404, serde_json::to_vec(&WireError { message: "unknown sync endpoint".into() }).unwrap_or_else(|_| b"{\"message\":\"not found\"}".to_vec()))
        } else if (head.path == PAIRING_PATH || head.path == PAIRING_INVITE_PATH) && !self.pairing_limiter.allow(peer.ip()) {
            (429, serde_json::to_vec(&WireError { message: "pairing rate limit exceeded; retry later".into() }).unwrap_or_else(|_| b"{\"message\":\"pairing rate limit exceeded\"}".to_vec()))
        } else if head.path == DIAGNOSTICS_MCP_PATH {
            let body = read_body(&mut tls, head.content_length, deadline, &self.cancel)?;
            let store = lock_recover(&self.store);
            let diagnostics_deadline = Instant::now()
                .checked_add(DIAGNOSTICS_REQUEST_TIMEOUT)
                .map(|value| value.min(deadline))
                .ok_or(TransportError::Timeout)?;
            match self.diagnostics.handle_json(&store, &body, now_ms(), diagnostics_deadline) {
                Ok(Some(response)) => (200, serde_json::to_vec(&response).map_err(|e| TransportError::Protocol(format!("encode diagnostics MCP response: {e}")))?),
                Ok(None) => (200, Vec::new()),
                Err(error) => {
                    let body = serde_json::to_vec(&error.rpc_response(None)).map_err(|e| TransportError::Protocol(format!("encode diagnostics MCP error: {e}")))?;
                    (error.http_status(), body)
                }
            }
        } else if head.path == PAIRING_INVITE_PATH {
            let body = read_body(&mut tls, head.content_length, deadline, &self.cancel)?;
            let request = serde_json::from_slice::<WirePairingInviteRequest>(&body)
                .map_err(|e| TransportError::Protocol(format!("invalid discovery pairing JSON: {e}")))?;
            let mut store = lock_recover(&self.store);
            match issue_pairing_invite(&mut store, request, &self.config.certificate_der) {
                Ok(body) => (200, body),
                Err(error) => (status_for_error(&error), serde_json::to_vec(&WireError { message: safe_error_message(&error) }).unwrap_or_else(|_| b"{\"message\":\"request rejected\"}".to_vec())),
            }
        } else {
            let body = read_body(&mut tls, head.content_length, deadline, &self.cancel)?;
            let request = HttpRequest { path: head.path.clone(), content_type: head.content_type, body };
            let mut store = lock_recover(&self.store);
            let dispatched = dispatch_request(&mut store, &request, &self.replay, self);
            drop(store);
            // A pairing request that persisted here (fresh pending row, or the
            // auto-approve that landed on it) is invisible to the panel until
            // its next read; wake the embedding UI so approval never waits out
            // the poll cadence.
            if head.path == PAIRING_PATH && dispatched.0 == 200 {
                if let Ok(state) = serde_json::from_slice::<PairingRequestState>(&dispatched.1) {
                    if state.status == "pending" || state.status == "paired" {
                        let (display_name, auto_approved) = (state.display_name.clone(), state.status == "paired");
                        if let Some(hook) = lock_recover(&self.pairing_notify).clone() { hook(&display_name, auto_approved); }
                    }
                }
            }
            dispatched
        };
        let content_type = if status == 200 && head.path == DOWNLOAD_PATH { "application/octet-stream" } else { "application/json" };
        write_response(&mut tls, status, content_type, &response, self.config.max_body_bytes)?;
        tls.conn.send_close_notify();
        let _ = tls.flush();
        let _ = tls.get_mut().shutdown(Shutdown::Both);
        Ok(())
    }

    fn reject_wire_version(&self, tls: &mut StreamOwned<ServerConnection, TcpStream>, head: &HttpRequestHead) -> Result<(), TransportError> {
        let detail = match head.wire_version {
            Some((major, minor)) => format!("peer announced sync protocol {major}.{minor}"),
            None => "peer did not announce a sync protocol version".into(),
        };
        let message = format!(
            "this TokenMe requires sync protocol {WIRE_VERSION_MAJOR}.{WIRE_VERSION_MINOR}; {detail}. Update TokenMe on the other machine."
        );
        self.log_event(false, &format!("sync: action=connection outcome=wire_rejected {detail}"));
        self.respond_and_close(tls, 426, &message, WIRE_REJECT_DRAIN_BYTES)
    }

    /// Answer, then drain a bounded amount of the already-sent body before
    /// closing: closing with unread bytes makes the kernel send RST, which can
    /// discard the response the client is waiting for.
    fn respond_and_close(&self, tls: &mut StreamOwned<ServerConnection, TcpStream>, status: u16, message: &str, drain_bytes: usize) -> Result<(), TransportError> {
        let body = serde_json::to_vec(&WireError { message: message.to_string() })
            .map_err(|error| TransportError::Protocol(format!("encode rejection: {error}")))?;
        write_response(tls, status, "application/json", &body, MAX_RESPONSE_BYTES)?;
        tls.conn.send_close_notify();
        let _ = tls.flush();
        if drain_bytes > 0 {
            let _ = tls.get_mut().set_read_timeout(Some(BUSY_DRAIN_TIMEOUT));
            let drain_until = Instant::now().checked_add(BUSY_DRAIN_TIMEOUT).unwrap_or_else(Instant::now);
            let mut remaining = drain_bytes;
            let mut sink = [0u8; 16 * 1024];
            while remaining > 0 && Instant::now() < drain_until {
                let take = sink.len().min(remaining);
                match tls.get_mut().read(&mut sink[..take]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => remaining -= n,
                }
            }
        }
        let _ = tls.get_mut().shutdown(Shutdown::Both);
        Ok(())
    }
}

#[derive(Debug)]
struct HttpRequest {
    path: String,
    content_type: String,
    body: Vec<u8>,
}

/// Binary downlink framing. The page carries a complete immutable package, so
/// base64-in-JSON would inflate every page by a third before decompression and
/// blur the difference between the control answer and the payload. Fields are
/// length-prefixed with an explicit absent marker; the decoder rejects any
/// trailing byte so a framing change cannot be silently truncated.
fn encode_primary_download_page(page: PrimaryDownloadPage) -> Result<Vec<u8>, SyncError> {
    if page.preparing && page.complete {
        return Err(SyncError::Protocol("primary download page cannot be both preparing and complete".into()));
    }
    let package = match (page.enabled, page.package.as_ref()) {
        (true, Some(package)) => Some(package),
        (false, None) => None,
        _ => return Err(SyncError::Protocol("primary download page sharing state is inconsistent".into())),
    };
    let mut body = Vec::with_capacity(96 + package.map_or(0, |package| package.manifest_bytes.len() + package.gzip_bytes.len()));
    body.extend_from_slice(DOWNLOAD_MAGIC);
    body.push(DOWNLOAD_PAGE_VERSION);
    let flags = (if page.enabled { DOWNLOAD_FLAG_ENABLED } else { 0 })
        | (if page.preparing { DOWNLOAD_FLAG_PREPARING } else { 0 })
        | (if page.complete { DOWNLOAD_FLAG_COMPLETE } else { 0 });
    body.push(flags);
    body.extend_from_slice(&page.request_nonce);
    body.extend_from_slice(&page.next_dirty_revision.to_be_bytes());
    body.extend_from_slice(&package.map_or([0u8; 32], |package| hash_bytes(&package.gzip_bytes)));
    match page.next_identity.as_deref() {
        Some(identity) => {
            if identity.is_empty() || identity.len() > MAX_STRING_BYTES {
                return Err(SyncError::Protocol("primary download identity cursor is invalid".into()));
            }
            let len = u32::try_from(identity.len()).map_err(|_| SyncError::Limit("download identity is too large".into()))?;
            body.extend_from_slice(&len.to_be_bytes());
            body.extend_from_slice(identity.as_bytes());
        }
        None => body.extend_from_slice(&u32::MAX.to_be_bytes()),
    }
    match package {
        Some(package) => {
            let manifest_len = u32::try_from(package.manifest_bytes.len())
                .map_err(|_| SyncError::Limit("download manifest is too large".into()))?;
            body.extend_from_slice(&manifest_len.to_be_bytes());
            body.extend_from_slice(&package.manifest_bytes);
            body.extend_from_slice(&(package.gzip_bytes.len() as u64).to_be_bytes());
            body.extend_from_slice(&package.gzip_bytes);
        }
        None => {
            body.extend_from_slice(&u32::MAX.to_be_bytes());
            body.extend_from_slice(&u64::MAX.to_be_bytes());
        }
    }
    Ok(body)
}

fn decode_primary_download_page(body: &[u8], request: &AuthenticatedDownloadRequest) -> Result<PrimaryDownloadPage, TransportError> {
    let mut offset = 0usize;
    let mut take = |len: usize| -> Result<&[u8], TransportError> {
        let end = offset.checked_add(len).ok_or(TransportError::Limit)?;
        let slice = body.get(offset..end).ok_or_else(|| TransportError::Protocol("primary download page is truncated".into()))?;
        offset = end;
        Ok(slice)
    };
    if take(8)? != DOWNLOAD_MAGIC {
        return Err(TransportError::Protocol("primary download page has an unknown magic".into()));
    }
    let version = take(1)?[0];
    if version != DOWNLOAD_PAGE_VERSION {
        return Err(TransportError::Unsupported(format!(
            "peer sent download page version {version}; this build requires {DOWNLOAD_PAGE_VERSION} — update TokenMe on the other machine"
        )));
    }
    let flags = take(1)?[0];
    if flags & !(DOWNLOAD_FLAG_ENABLED | DOWNLOAD_FLAG_PREPARING | DOWNLOAD_FLAG_COMPLETE) != 0 {
        return Err(TransportError::Protocol("primary download page has unknown flags".into()));
    }
    let enabled = flags & DOWNLOAD_FLAG_ENABLED != 0;
    let preparing = flags & DOWNLOAD_FLAG_PREPARING != 0;
    let complete = flags & DOWNLOAD_FLAG_COMPLETE != 0;
    let request_nonce: [u8; 16] = take(16)?.try_into().map_err(|_| TransportError::Protocol("primary download nonce is invalid".into()))?;
    if request_nonce != request.nonce {
        return Err(TransportError::Protocol("primary download response does not match request nonce".into()));
    }
    let next_dirty_revision = u64::from_be_bytes(take(8)?.try_into().map_err(|_| TransportError::Protocol("primary download cursor is invalid".into()))?);
    if next_dirty_revision > i64::MAX as u64 {
        return Err(TransportError::Protocol("primary download cursor is too large".into()));
    }
    let gzip_sha256: [u8; 32] = take(32)?.try_into().map_err(|_| TransportError::Protocol("primary download package hash is invalid".into()))?;
    let identity_len = u32::from_be_bytes(take(4)?.try_into().map_err(|_| TransportError::Protocol("primary download identity length is invalid".into()))?);
    let next_identity = if identity_len == u32::MAX {
        None
    } else {
        let len = usize::try_from(identity_len).map_err(|_| TransportError::Limit)?;
        if len > MAX_STRING_BYTES { return Err(TransportError::Limit); }
        let identity = std::str::from_utf8(take(len)?).map_err(|_| TransportError::Protocol("primary download identity is not UTF-8".into()))?;
        crate::protocol::valid_text("download identity cursor", identity, false)
            .map_err(|error| TransportError::Protocol(error.to_string()))?;
        Some(identity.to_string())
    };
    let manifest_len = u32::from_be_bytes(take(4)?.try_into().map_err(|_| TransportError::Protocol("primary download manifest length is invalid".into()))?);
    let manifest_bytes = if manifest_len == u32::MAX {
        None
    } else {
        let len = usize::try_from(manifest_len).map_err(|_| TransportError::Limit)?;
        if len == 0 || len > MAX_MANIFEST_BYTES { return Err(TransportError::Limit); }
        Some(take(len)?.to_vec())
    };
    let gzip_len = u64::from_be_bytes(take(8)?.try_into().map_err(|_| TransportError::Protocol("primary download package length is invalid".into()))?);
    let gzip_bytes = if gzip_len == u64::MAX {
        None
    } else {
        let len = usize::try_from(gzip_len).map_err(|_| TransportError::Limit)?;
        if len > MAX_COMPRESSED_BYTES { return Err(TransportError::Limit); }
        Some(take(len)?.to_vec())
    };
    if offset != body.len() {
        return Err(TransportError::Protocol("primary download page has trailing bytes".into()));
    }
    let identity_regressed = match (&request.after_identity, &next_identity) {
        (Some(_), None) => true,
        (Some(old), Some(new)) => new < old,
        _ => false,
    };
    if identity_regressed || next_dirty_revision < request.after_dirty_revision {
        return Err(TransportError::Protocol("primary download cursor moved backwards".into()));
    }
    let package = match (enabled, manifest_bytes, gzip_bytes) {
        (true, Some(manifest_bytes), Some(gzip_bytes)) => {
            if hash_bytes(&gzip_bytes) != gzip_sha256 {
                return Err(TransportError::Protocol("primary download package hash mismatch".into()));
            }
            let package = crate::NetworkPackage { manifest_bytes, gzip_bytes };
            let (manifest, _) = package.validate().map_err(|error| TransportError::Protocol(error.to_string()))?;
            if manifest.origin.is_empty() || !usage_core::origin_ok(&manifest.origin) || manifest.schema_version != usage_index::INDEX_SCHEMA_VERSION
                || manifest.snapshot_complete != complete
            {
                return Err(TransportError::Protocol("primary download package identity or completion marker is invalid".into()));
            }
            Some(package)
        }
        (false, None, None) => {
            if gzip_sha256 != [0u8; 32] || next_identity != request.after_identity || next_dirty_revision != request.after_dirty_revision {
                return Err(TransportError::Protocol("disabled primary download response contains data or changes its cursor".into()));
            }
            if !complete { return Err(TransportError::Protocol("disabled primary download response is incomplete".into())); }
            if preparing { return Err(TransportError::Protocol("disabled primary download response is still preparing".into())); }
            None
        }
        _ => return Err(TransportError::Protocol("primary download page mixes package and cursor state".into())),
    };
    if preparing && complete {
        return Err(TransportError::Protocol("primary download response cannot be both preparing and complete".into()));
    }
    let progressed = next_identity != request.after_identity || next_dirty_revision > request.after_dirty_revision;
    if enabled && !progressed && !complete && !preparing {
        return Err(TransportError::Protocol("primary download page did not advance its cursor".into()));
    }
    Ok(PrimaryDownloadPage {
        request_nonce,
        enabled,
        preparing,
        package,
        next_identity,
        next_dirty_revision,
        complete,
    })
}

#[derive(Debug)]
struct HttpRequestHead {
    path: String,
    content_type: String,
    content_length: usize,
    known_path: Option<()>,
    wire_version: Option<(u16, u16)>,
    is_websocket: bool,
    raw_header: Vec<u8>,
}

struct PrefixedStream<S> {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: S,
}

impl<S: Read> Read for PrefixedStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if (self.prefix.position() as usize) < self.prefix.get_ref().len() {
            let n = self.prefix.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
        }
        self.inner.read(buf)
    }
}

impl<S: Write> Write for PrefixedStream<S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct LimitedReader<'a, R> {
    inner: &'a mut R,
    remaining: usize,
    deadline: Instant,
    cancel: &'a AtomicBool,
}

impl<'a, R: Read> LimitedReader<'a, R> {
    fn new(inner: &'a mut R, remaining: usize, deadline: Instant, cancel: &'a AtomicBool) -> Self { Self { inner, remaining, deadline, cancel } }

    fn read_exact(&mut self, target: &mut [u8]) -> Result<(), TransportError> {
        if target.len() > self.remaining { return Err(TransportError::Protocol("HTTP body ended before the announced content-length".into())); }
        let mut offset = 0;
        while offset < target.len() {
            if self.cancel.load(AtomicOrdering::Acquire) { return Err(TransportError::Cancelled); }
            if Instant::now() >= self.deadline { return Err(TransportError::Timeout); }
            let n = self.inner.read(&mut target[offset..]).map_err(TransportError::from)?;
            if n == 0 { return Err(TransportError::Protocol("unexpected end of HTTP body".into())); }
            offset += n;
            self.remaining -= n;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

struct HttpResponseHead {
    status: u16,
    content_length: Option<usize>,
    wire_version: Option<(u16, u16)>,
}

impl HttpResponse {
    fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, TransportError> {
        if self.status != 200 {
            let message = serde_json::from_slice::<WireError>(&self.body).map(|e| e.message).unwrap_or_else(|_| "server rejected request".into());
            return Err(TransportError::HttpStatus { status: self.status, message });
        }
        serde_json::from_slice(&self.body).map_err(|e| TransportError::Protocol(format!("invalid JSON response: {e}")))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WirePairingRequest {
    request_id: String,
    invite_id: String,
    invite_secret_b64: String,
    device_id: String,
    origin: String,
    display_name: String,
    os: String,
    public_key_b64: String,
    signature_b64: String,
}

impl WirePairingRequest {
    fn from_request(request: &PairingRequest) -> Result<Self, TransportError> {
        let b64 = base64::engine::general_purpose::STANDARD;
        Ok(Self { request_id: request.request_id.clone(), invite_id: request.invite_id.clone(), invite_secret_b64: b64.encode(&request.invite_secret), device_id: request.device_id.clone(), origin: request.origin.clone(), display_name: request.display_name.clone(), os: request.os.clone(), public_key_b64: b64.encode(request.public_key), signature_b64: b64.encode(request.signature) })
    }

    fn encode(&self) -> Result<Vec<u8>, TransportError> {
        serde_json::to_vec(self).map_err(|e| TransportError::Protocol(format!("encode pairing request: {e}")))
    }

    fn into_request(self) -> Result<PairingRequest, TransportError> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let invite_secret = b64.decode(self.invite_secret_b64).map_err(|e| TransportError::Protocol(format!("invite secret encoding: {e}")))?;
        let public_key: [u8; 32] = b64.decode(self.public_key_b64).map_err(|e| TransportError::Protocol(format!("public key encoding: {e}")))?.try_into().map_err(|_| TransportError::Protocol("public key must be 32 bytes".into()))?;
        let signature: [u8; 64] = b64.decode(self.signature_b64).map_err(|e| TransportError::Protocol(format!("signature encoding: {e}")))?.try_into().map_err(|_| TransportError::Protocol("signature must be 64 bytes".into()))?;
        Ok(PairingRequest { request_id: self.request_id, invite_id: self.invite_id, invite_secret, device_id: self.device_id, origin: self.origin, display_name: self.display_name, os: self.os, public_key, signature })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WirePairingInviteRequest {
    primary_id: String,
    endpoint: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct WirePairingInvite {
    invite_id: String,
    endpoint: String,
    certificate_pin: String,
    expires_at_ms: i64,
    secret_b64: String,
    certificate_der_b64: String,
    #[serde(default)]
    primary_display_name: Option<String>,
    /// The primary's OS, added so the secondary can render the platform icon
    /// for its primary. Optional on the wire: pairings against an older
    /// primary simply leave it unset.
    #[serde(default)]
    primary_os: Option<String>,
}

impl WirePairingInvite {
    fn from_invite(invite: &PairingInvite, certificate_der: &[u8], primary_display_name: String, primary_os: String) -> Self {
        let b64 = base64::engine::general_purpose::STANDARD;
        Self {
            invite_id: invite.invite_id.clone(), endpoint: invite.endpoint.clone(),
            certificate_pin: invite.certificate_pin.clone(), expires_at_ms: invite.expires_at_ms,
            secret_b64: b64.encode(&invite.secret), certificate_der_b64: b64.encode(certificate_der),
            primary_display_name: Some(primary_display_name), primary_os: Some(primary_os),
        }
    }

    fn into_invite(self) -> Result<(PairingInvite, Vec<u8>, Option<String>, Option<String>), TransportError> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let secret = b64.decode(self.secret_b64).map_err(|e| TransportError::Protocol(format!("pairing invite secret encoding: {e}")))?;
        let certificate_der = b64.decode(self.certificate_der_b64).map_err(|e| TransportError::Protocol(format!("pairing certificate encoding: {e}")))?;
        let pin = parse_pairing_pin(&self.certificate_pin)?;
        if secret.len() != 32 || certificate_der.is_empty() || hash_bytes(&certificate_der) != pin {
            return Err(TransportError::Protocol("discovery pairing response failed certificate or secret validation".into()));
        }
        crate::protocol::valid_text("invite_id", &self.invite_id, false).map_err(|e| TransportError::Protocol(e.to_string()))?;
        crate::protocol::valid_text("endpoint", &self.endpoint, false).map_err(|e| TransportError::Protocol(e.to_string()))?;
        if let Some(name) = self.primary_display_name.as_deref() {
            crate::protocol::valid_text("primary_display_name", name, false).map_err(|e| TransportError::Protocol(e.to_string()))?;
        }
        if let Some(os) = self.primary_os.as_deref() {
            crate::protocol::valid_text("primary_os", os, false).map_err(|e| TransportError::Protocol(e.to_string()))?;
        }
        parse_endpoint(&self.endpoint)?;
        crate::protocol::validate_certificate_pin(&self.certificate_pin).map_err(|e| TransportError::Protocol(e.to_string()))?;
        Ok((PairingInvite { invite_id: self.invite_id, endpoint: self.endpoint, certificate_pin: self.certificate_pin, expires_at_ms: self.expires_at_ms, secret }, certificate_der, self.primary_display_name, self.primary_os))
    }
}

fn parse_pairing_pin(value: &str) -> Result<[u8; 32], TransportError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TransportError::Protocol("pairing response certificate pin is invalid".into()));
    }
    let mut pin = [0u8; 32];
    for (index, slot) in pin.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| TransportError::Protocol("pairing response certificate pin is invalid".into()))?;
    }
    Ok(pin)
}

#[derive(Debug, Serialize, Deserialize)]
struct WireError {
    message: String,
}

fn issue_pairing_invite(store: &mut SyncStore, request: WirePairingInviteRequest, certificate_der: &[u8]) -> Result<Vec<u8>, SyncError> {
    if request.primary_id != store.primary_id() {
        return Err(SyncError::Auth("primary identity does not match the discovered service".into()));
    }
    let pin = hex_bytes(&hash_bytes(certificate_der));
    let invite = store.create_pairing_invite(request.endpoint, pin, now_ms().saturating_add(DISCOVERY_INVITE_TTL_MS))?;
    serde_json::to_vec(&WirePairingInvite::from_invite(&invite, certificate_der, usage_index::hostname(), std::env::consts::OS.into())).map_err(|e| SyncError::Protocol(format!("encode discovery pairing response: {e}")))
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The rollup endpoint signs under its own domain so a captured download or
/// status request cannot be replayed against it (and the other way around).
const ROLLUP_DOMAIN: &[u8] = b"tokenme-device-sync/rollup/v1";

/// Authenticated request for the primary's own rollup rows. Same identity
/// fields as the download request minus the paging cursors: the snapshot is
/// the whole local-origin rollup table, not a page, so there is no cursor to
/// sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedRollupRequest {
    pub protocol_id: String,
    pub primary_id: String,
    pub device_id: String,
    pub origin: String,
    pub nonce: [u8; 16],
    pub issued_at_ms: i64,
    #[serde(with = "signature_serde")]
    pub signature: [u8; 64],
}

impl AuthenticatedRollupRequest {
    pub fn new_at(identity: &DeviceIdentity, primary_id: impl Into<String>, issued_at_ms: i64) -> SyncResult<Self> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| SyncError::Crypto(format!("OS randomness unavailable: {e}")))?;
        let mut request = Self {
            protocol_id: PROTOCOL_ID.into(),
            primary_id: primary_id.into(),
            device_id: identity.device_id.clone(),
            origin: identity.origin.clone(),
            nonce,
            issued_at_ms,
            signature: [0; 64],
        };
        request.validate_fields()?;
        request.signature = identity.signing_key.sign(&request.signing_bytes()).to_bytes();
        Ok(request)
    }

    pub fn verify(&self, public_key: &VerifyingKey) -> SyncResult<()> {
        self.validate_fields()?;
        public_key.verify(&self.signing_bytes(), &Signature::from_bytes(&self.signature))
            .map_err(|_| SyncError::Auth("authenticated rollup signature verification failed".into()))
    }

    fn validate_fields(&self) -> SyncResult<()> {
        if self.protocol_id != PROTOCOL_ID {
            return Err(SyncError::Auth("invalid authenticated rollup request".into()));
        }
        crate::protocol::valid_text("primary_id", &self.primary_id, false)?;
        crate::protocol::valid_text("device_id", &self.device_id, false)?;
        crate::protocol::valid_text("origin", &self.origin, false)?;
        if !usage_core::origin_ok(&self.origin) { return Err(SyncError::Auth("invalid authenticated rollup origin".into())); }
        Ok(())
    }

    pub(crate) fn is_fresh(&self, now_ms: i64) -> bool {
        self.issued_at_ms > now_ms.saturating_sub(MAX_AUTH_REQUEST_AGE_MS)
            && self.issued_at_ms <= now_ms.saturating_add(MAX_AUTH_REQUEST_FUTURE_SKEW_MS)
    }

    fn signing_bytes(&self) -> Vec<u8> {
        identity_bytes(ROLLUP_DOMAIN, &[
            self.protocol_id.as_bytes(), self.primary_id.as_bytes(), self.device_id.as_bytes(),
            self.origin.as_bytes(), &self.nonce, &self.issued_at_ms.to_be_bytes(),
        ])
    }
}

/// The primary's own local-origin rollup rows. `origin` declares the label the
/// secondary must group the rows under (the primary's hostname — consistent
/// with what its merged rows historically carried); each row's own `origin`
/// field is empty by construction and the secondary overrides it with the
/// declared label on import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollupSnapshot {
    pub origin: String,
    pub rows: Vec<RollupRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollupRow {
    pub day: String,
    pub origin: String,
    pub tool: String,
    pub session: String,
    pub project: String,
    pub model: String,
    pub meter: String,
    pub in_tok: f64,
    pub cc_tok: f64,
    pub cr_tok: f64,
    pub out_tok: f64,
    pub reason_tok: f64,
    pub credits: f64,
    pub n: i64,
    pub nonzero: i64,
    pub min_ts: i64,
    pub max_ts: i64,
}

/// Serves the primary's own rollup rows after the same identity checks
/// `SyncStore::primary_download_page` runs before serving: right primary,
/// registered device, matching origin, valid signature, fresh request, paired
/// status. The read touches only the index, so no cursor state is written.
fn primary_rollup_snapshot(store: &mut SyncStore, request: &AuthenticatedRollupRequest, now: i64) -> SyncResult<RollupSnapshot> {
    if request.primary_id != store.primary_id() {
        return Err(SyncError::Auth("rollup targets another primary".into()));
    }
    let Some(record) = store.device(&request.device_id)? else {
        return Err(SyncError::NotPaired(request.device_id.clone()));
    };
    if record.origin != request.origin {
        return Err(SyncError::Auth("rollup origin does not match paired device".into()));
    }
    request.verify(&crate::protocol::public_key_from_bytes(&record.public_key)?)?;
    if !request.is_fresh(now) {
        return Err(SyncError::Auth("rollup request is expired or from the future".into()));
    }
    if record.status != DeviceStatus::Paired {
        return Err(SyncError::NotPaired(request.device_id.clone()));
    }
    // `event_rollup.origin` is the materialized ORIGIN_SQL of each event's
    // source (usage_index folds it in at ingest time), so the primary's own
    // rows are exactly `origin = ''`; merged bundle imports carry their
    // bundle origin and never travel this wire.
    let mut statement = store.index().conn().prepare(
        "SELECT day, origin, tool, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts FROM event_rollup WHERE origin = ''",
    ).map_err(|e| SyncError::Sqlite(e.to_string()))?;
    let rows = statement.query_map([], |row| Ok(RollupRow {
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
    })).map_err(|e| SyncError::Sqlite(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| SyncError::Sqlite(e.to_string()))?;
    Ok(RollupSnapshot { origin: usage_index::hostname(), rows })
}

fn dispatch_request(store: &mut SyncStore, request: &HttpRequest, replay: &Mutex<NonceReplayCache>, context: &ServerContext) -> (u16, Vec<u8>) {
    let now = now_ms();
    let result: Result<Vec<u8>, SyncError> = match (request.path.as_str(), request.content_type.as_str()) {
        (PAIRING_PATH, "application/json") => serde_json::from_slice::<WirePairingRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid pairing JSON: {e}")))
            .and_then(|wire| wire.into_request().map_err(|e| SyncError::Protocol(e.to_string())))
            .and_then(|pairing| store.receive_pairing_request(&pairing, now).map(|state| serde_json::to_vec(&state).map_err(|e| SyncError::Protocol(e.to_string())).unwrap_or_default())),
        (STATUS_PATH, "application/json") => serde_json::from_slice::<AuthenticatedStatusRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid status request: {e}")))
            .and_then(|status| with_claimed_nonce(replay, NONCE_SCOPE_STATUS, status.nonce, now, || {
                store.authenticated_status(&status, now).and_then(|value| serde_json::to_vec(&value).map_err(|e| SyncError::Protocol(e.to_string())))
            })),
        (DOWNLOAD_PATH, "application/json") => serde_json::from_slice::<AuthenticatedDownloadRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid download request: {e}")))
            .and_then(|download| with_claimed_nonce(replay, NONCE_SCOPE_DOWNLOAD, download.nonce, now, || {
                store.primary_download_page(&download, now, 30).and_then(encode_primary_download_page)
            })),
        (ROLLUP_PATH, "application/json") => serde_json::from_slice::<AuthenticatedRollupRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid rollup request: {e}")))
            .and_then(|rollup| with_claimed_nonce(replay, NONCE_SCOPE_ROLLUP, rollup.nonce, now, || {
                primary_rollup_snapshot(store, &rollup, now).and_then(|snapshot| serde_json::to_vec(&snapshot).map_err(|e| SyncError::Protocol(e.to_string())))
            })),
        (DISPLAY_NAME_PATH, "application/json") => serde_json::from_slice::<AuthenticatedDisplayNameRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid display-name update: {e}")))
            .and_then(|update| with_claimed_nonce(replay, NONCE_SCOPE_DISPLAY_NAME, update.nonce, now, || {
                store.update_display_name(&update, now).and_then(|_| serde_json::to_vec(&serde_json::json!({})).map_err(|e| SyncError::Protocol(e.to_string())))
            })),
        (RECOVERY_PATH, "application/json") => serde_json::from_slice::<AuthenticatedStatusRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid recovery request: {e}")))
            .and_then(|recovery| store.recover_stream(&recovery, now).and_then(|value| serde_json::to_vec(&value).map_err(|e| SyncError::Protocol(e.to_string())))),
        (DISCONNECT_PATH, "application/json") => serde_json::from_slice::<AuthenticatedStatusRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid disconnect request: {e}")))
            .and_then(|disconnect| with_claimed_nonce(replay, NONCE_SCOPE_DISCONNECT, disconnect.nonce, now, || {
                store.disconnect_device(&disconnect, now).and_then(|_| {
                    context.notify_disconnect(&disconnect.device_id);
                    serde_json::to_vec(&serde_json::json!({})).map_err(|e| SyncError::Protocol(e.to_string()))
                })
            })),
        _ => Err(SyncError::NotFound("HTTP path".into())),
    };
    match result {
        Ok(body) => (200, body),
        Err(error) => (status_for_error(&error), serde_json::to_vec(&WireError { message: safe_error_message(&error) }).unwrap_or_else(|_| b"{\"message\":\"request rejected\"}".to_vec())),
    }
}

fn safe_error_message(error: &SyncError) -> String {
    match error {
        SyncError::Auth(_) | SyncError::Security(_) => "authentication or security policy rejected the request".into(),
        SyncError::NotPaired(_) | SyncError::Revoked(_) => "device is not authorized".into(),
        SyncError::Sqlite(_) | SyncError::Io(_, _) => "temporary sync service failure".into(),
        other => other.to_string(),
    }
}

fn status_for_error(error: &SyncError) -> u16 {
    match error {
        SyncError::Auth(_) | SyncError::NotPaired(_) => 401,
        SyncError::Revoked(_) | SyncError::Security(_) => 403,
        SyncError::Conflict(_) | SyncError::ReconcileRequired | SyncError::EpochStale | SyncError::GenerationGap { .. } | SyncError::StreamClosed(_) => 409,
        SyncError::Limit(_) => 413,
        SyncError::Unsupported(_) => 426,
        SyncError::NotFound(_) => 404,
        SyncError::Sqlite(_) | SyncError::Io(_, _) => 503,
        _ => 400,
    }
}

fn client_config(config: &HttpsTransportConfig) -> Result<ClientConfig, TransportError> {
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    if config.trust_anchors_der.is_empty() {
        return Ok(builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinOnlyVerifier { expected: config.pinned_certificate_sha256 }))
            .with_no_client_auth());
    }
    let mut roots = RootCertStore::empty();
    for der in &config.trust_anchors_der {
        roots.add(CertificateDer::from(der.clone())).map_err(|e| TransportError::Config(format!("invalid TLS trust anchor: {e}")))?;
    }
    Ok(builder.with_root_certificates(roots).with_no_client_auth())
}

#[derive(Debug)]
struct PinOnlyVerifier {
    expected: [u8; 32],
}

impl ServerCertVerifier for PinOnlyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if hash_bytes(end_entity.as_ref()) != self.expected {
            return Err(rustls::Error::General("server certificate pin mismatch".into()));
        }
        // Pin equality alone accepts any self-signed leaf, including one whose
        // validity window has passed or that names another host. The pin is the
        // trust anchor, so verify the leaf's own chain, validity and name.
        let parsed = ParsedCertificate::try_from(end_entity)
            .map_err(|error| rustls::Error::General(format!("pinned server certificate is unreadable: {error}")))?;
        let mut roots = RootCertStore::empty();
        roots.add(end_entity.clone())
            .map_err(|error| rustls::Error::General(format!("pinned server certificate is not a trust anchor: {error}")))?;
        verify_server_cert_signed_by_trust_anchor(
            &parsed,
            &roots,
            intermediates,
            now,
            rustls::crypto::ring::default_provider().signature_verification_algorithms.all,
        )
        .map_err(|error| rustls::Error::General(format!("pinned server certificate failed validation: {error}")))?;
        verify_server_name(&parsed, server_name)
            .map_err(|error| rustls::Error::General(format!("pinned server certificate does not name the endpoint: {error}")))?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &rustls::crypto::ring::default_provider().signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &rustls::crypto::ring::default_provider().signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

fn server_config(config: &HttpsServerConfig) -> Result<ServerConfig, TransportError> {
    let certs = vec![CertificateDer::from(config.certificate_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(config.private_key_der.clone()));
    rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| TransportError::Tls(e.to_string()))
}

fn verify_peer_pin(connection: &ClientConnection, expected: [u8; 32]) -> Result<(), TransportError> {
    let certificates = connection.peer_certificates().ok_or(TransportError::PinMismatch)?;
    let leaf = certificates.first().ok_or(TransportError::PinMismatch)?;
    if hash_bytes(leaf.as_ref()) != expected {
        return Err(TransportError::PinMismatch);
    }
    Ok(())
}

pub(crate) fn parse_endpoint(endpoint: &str) -> Result<ParsedEndpoint, TransportError> {
    let prefix = "https://";
    if !endpoint.starts_with(prefix) {
        return Err(TransportError::Config("endpoint must use https://".into()));
    }
    let authority_start = prefix.len();
    if endpoint[authority_start..].contains('/') || endpoint[authority_start..].contains('?') || endpoint[authority_start..].contains('#') {
        return Err(TransportError::Config("endpoint must not contain a path, query, or fragment".into()));
    }
    let authority = &endpoint[authority_start..];
    if authority.is_empty() {
        return Err(TransportError::Config("endpoint authority is empty".into()));
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(close) = rest.find(']') else { return Err(TransportError::Config("invalid IPv6 endpoint".into())); };
        let port_part = &rest[close + 1..];
        let Some(port_text) = port_part.strip_prefix(':') else { return Err(TransportError::Config("endpoint must include a port".into())); };
        let port = port_text.parse::<u16>().map_err(|_| TransportError::Config("endpoint port is invalid".into()))?;
        if port == 0 { return Err(TransportError::Config("endpoint port must be nonzero".into())); }
        return Ok(ParsedEndpoint { host_start: authority_start + 1, host_end: authority_start + 1 + close, port });
    }
    let Some((host, port_text)) = authority.rsplit_once(':') else { return Err(TransportError::Config("endpoint must include a port".into())); };
    if host.is_empty() || host.contains(':') { return Err(TransportError::Config("endpoint host is invalid".into())); }
    let port = port_text.parse::<u16>().map_err(|_| TransportError::Config("endpoint port is invalid".into()))?;
    if port == 0 { return Err(TransportError::Config("endpoint port must be nonzero".into())); }
    Ok(ParsedEndpoint { host_start: authority_start, host_end: authority_start + host.len(), port })
}

fn resolve_connect_addr(host: &str, port: u16, deadline: Instant, cancel: &AtomicBool) -> Result<SocketAddr, TransportError> {
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(address, port));
    }
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port));
    }
    // DNS and mDNS resolution is delegated to one bounded helper so a broken
    // resolver cannot hold an unattended worker past its request deadline.
    let host = host.to_owned();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = (host.as_str(), port).to_socket_addrs()
            .map(|addresses| addresses.collect::<Vec<_>>())
            .map_err(|error| error.to_string());
        let _ = sender.send(result);
    });
    loop {
        if cancel.load(AtomicOrdering::Acquire) { return Err(TransportError::Cancelled); }
        let remaining = deadline.checked_duration_since(Instant::now()).ok_or(TransportError::Timeout)?;
        match receiver.recv_timeout(remaining.min(CONNECT_POLL_SLICE)) {
            Ok(Ok(addresses)) => return addresses.into_iter().next().ok_or_else(|| TransportError::Config("endpoint host has no addresses".into())),
            Ok(Err(error)) => return Err(TransportError::Config(format!("endpoint host resolution failed: {error}"))),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(TransportError::Config("endpoint host resolution stopped".into())),
        }
    }
}

fn connect_tcp_with_deadline(addr: &SocketAddr, deadline: Instant, cancel: &AtomicBool) -> Result<TcpStream, TransportError> {
    if cancel.load(AtomicOrdering::Acquire) { return Err(TransportError::Cancelled); }
    let remaining = deadline.checked_duration_since(Instant::now()).ok_or(TransportError::Timeout)?;
    match TcpStream::connect_timeout(addr, remaining) {
        Ok(stream) => Ok(stream),
        Err(error) => {
            if cancel.load(AtomicOrdering::Acquire) {
                Err(TransportError::Cancelled)
            } else {
                Err(TransportError::from(error))
            }
        }
    }
}

fn read_request_head<S: Read>(stream: &mut S, max_body: usize, deadline: Instant, cancel: &AtomicBool) -> Result<HttpRequestHead, TransportError> {
    let header = read_until_deadline(stream, b"\r\n\r\n", MAX_HEADER_BYTES, deadline, Some(cancel))?;
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    let parsed = request.parse(&header).map_err(|e| TransportError::Protocol(format!("invalid HTTP request: {e}")))?;
    if parsed != httparse::Status::Complete(header.len()) {
        return Err(TransportError::Protocol("incomplete HTTP request headers".into()));
    }
    let method = request.method.unwrap_or_default();
    let path = request.path.unwrap_or_default();
    let is_ws = method.eq_ignore_ascii_case("GET") && (path == WS_SYNC_PATH || path == "/v1/sync/ws");
    if !is_ws && (method != "POST" || request.version != Some(1) || path.is_empty()) {
        return Err(TransportError::Protocol("only HTTP/1.1 POST or WebSocket GET is supported".into()));
    }
    let known_path = match path {
        WS_SYNC_PATH | "/v1/sync/ws" | PAIRING_PATH | PAIRING_INVITE_PATH | STATUS_PATH | DOWNLOAD_PATH | ROLLUP_PATH | DISPLAY_NAME_PATH | RECOVERY_PATH | DISCONNECT_PATH | DIAGNOSTICS_MCP_PATH => Some(()),
        _ => None,
    };
    let mut content_length = None;
    let mut transfer_encoding = false;
    let mut content_type = None;
    let mut host = None;
    let mut wire_version = None;
    for header in request.headers.iter() {
        let name = header.name;
        let value = std::str::from_utf8(header.value).map_err(|_| TransportError::Protocol("HTTP header value is not UTF-8".into()))?;
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() { return Err(TransportError::Protocol("duplicate content-length header".into())); }
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) { return Err(TransportError::Protocol("invalid content-length".into())); }
            content_length = Some(value.parse::<usize>().map_err(|_| TransportError::Protocol("invalid content-length".into()))?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            transfer_encoding = true;
        } else if name.eq_ignore_ascii_case("content-type") {
            if content_type.is_some() { return Err(TransportError::Protocol("duplicate content-type header".into())); }
            content_type = Some(value.trim().to_string());
        } else if name.eq_ignore_ascii_case("host") {
            if host.is_some() || value.trim().is_empty() { return Err(TransportError::Protocol("request lacks a valid host".into())); }
            host = Some(value.trim().to_string());
        } else if name.eq_ignore_ascii_case(WIRE_VERSION_HEADER) {
            if wire_version.is_some() { return Err(TransportError::Protocol("duplicate wire version header".into())); }
            wire_version = Some(parse_wire_version(value.trim())?);
        }
    }
    if transfer_encoding || host.is_none() {
        return Err(TransportError::Protocol("chunked transfer or missing host is not supported".into()));
    }
    let length = content_length.unwrap_or(0);
    let content_type = content_type.unwrap_or_default().split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    if known_path.is_some() && !is_ws {
        if content_length.is_none() { return Err(TransportError::Protocol("known endpoint lacks content-length".into())); }
        let limit = if path == DIAGNOSTICS_MCP_PATH {
            max_body.min(MAX_DIAGNOSTICS_BODY_BYTES)
        } else {
            MAX_CONTROL_BODY_BYTES
        };
        if length > limit { return Err(TransportError::Limit); }
        if content_type != "application/json" { return Err(TransportError::Protocol(format!("unexpected content-type for {path}"))); }
    }
    if Instant::now() >= deadline { return Err(TransportError::Timeout); }
    Ok(HttpRequestHead { path: path.to_string(), content_type, content_length: length, known_path, wire_version, is_websocket: is_ws, raw_header: header })
}

fn parse_wire_version(value: &str) -> Result<(u16, u16), TransportError> {
    let (major, minor) = value.split_once('.').ok_or_else(|| TransportError::Protocol("wire version header is not major.minor".into()))?;
    let component = |text: &str| -> Result<u16, TransportError> {
        if text.is_empty() || text.len() > 5 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(TransportError::Protocol("wire version header is invalid".into()));
        }
        text.parse::<u16>().map_err(|_| TransportError::Protocol("wire version header is invalid".into()))
    };
    Ok((component(major)?, component(minor)?))
}

fn read_body<S: Read>(stream: &mut S, length: usize, deadline: Instant, cancel: &AtomicBool) -> Result<Vec<u8>, TransportError> {
    let mut reader = LimitedReader::new(stream, length, deadline, cancel);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(body)
}

fn sweep_stale_staging(directory: &Path, stale_after: Duration, now: SystemTime) -> usize {
    let Ok(entries) = fs::read_dir(directory) else { return 0; };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue; };
        if !name.starts_with("upload-") || !name.ends_with(".part") { continue; }
        let Ok(metadata) = entry.metadata() else { continue; };
        if !metadata.is_file() { continue; }
        let Ok(modified) = metadata.modified() else { continue; };
        if now.duration_since(modified).is_ok_and(|age| age > stale_after)
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

fn read_response<S: Read>(stream: &mut S, max_response: usize) -> Result<HttpResponse, TransportError> {
    loop {
        let head = read_response_head(stream)?;
        if head.status == 100 { continue; }
        return read_response_body(stream, head, max_response);
    }
}

fn read_response_head<S: Read>(stream: &mut S) -> Result<HttpResponseHead, TransportError> {
    let header = read_until(stream, b"\r\n\r\n", MAX_HEADER_BYTES)?;
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);
    let parsed = response.parse(&header).map_err(|e| TransportError::Protocol(format!("invalid HTTP response: {e}")))?;
    if parsed != httparse::Status::Complete(header.len()) { return Err(TransportError::Protocol("incomplete HTTP response headers".into())); }
    if response.version != Some(1) { return Err(TransportError::Protocol("unsupported HTTP response version".into())); }
    let status = response.code.ok_or_else(|| TransportError::Protocol("missing HTTP status".into()))?;
    let mut content_length = None;
    let mut transfer_encoding = false;
    let mut wire_version = None;
    for header in response.headers.iter() {
        if header.name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() { return Err(TransportError::Protocol("duplicate response content-length header".into())); }
            let value = std::str::from_utf8(header.value).map_err(|_| TransportError::Protocol("invalid response content-length".into()))?;
            content_length = Some(value.trim().parse::<usize>().map_err(|_| TransportError::Protocol("invalid response content-length".into()))?);
        } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
            transfer_encoding = true;
        } else if header.name.eq_ignore_ascii_case(WIRE_VERSION_HEADER) {
            let value = std::str::from_utf8(header.value).map_err(|_| TransportError::Protocol("invalid response wire version".into()))?;
            wire_version = Some(parse_wire_version(value.trim())?);
        }
    }
    if transfer_encoding { return Err(TransportError::Protocol("chunked responses are not supported".into())); }
    if status == 100 && content_length.unwrap_or(0) != 0 {
        return Err(TransportError::Protocol("100 Continue response must not contain a body".into()));
    }
    Ok(HttpResponseHead { status, content_length, wire_version })
}

fn read_response_body<S: Read>(stream: &mut S, head: HttpResponseHead, max_response: usize) -> Result<HttpResponse, TransportError> {
    let length = head.content_length.ok_or_else(|| TransportError::Protocol("response lacks content-length".into()))?;
    if length > max_response { return Err(TransportError::Limit); }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).map_err(TransportError::from)?;
    validate_response_wire(head.status, head.wire_version, &body)?;
    Ok(HttpResponse { status: head.status, body })
}

fn validate_response_wire(status: u16, wire_version: Option<(u16, u16)>, body: &[u8]) -> Result<(), TransportError> {
    if status == 100 { return Ok(()); }
    if status == 426 {
        let message = serde_json::from_slice::<WireError>(body).map(|error| error.message)
            .unwrap_or_else(|_| "update TokenMe on the other machine".into());
        return Err(TransportError::Unsupported(message));
    }
    match wire_version {
        Some((major, _)) if major == WIRE_VERSION_MAJOR => Ok(()),
        Some((major, minor)) => Err(TransportError::Unsupported(format!(
            "peer speaks sync protocol {major}.{minor}; this build requires {WIRE_VERSION_MAJOR}.{WIRE_VERSION_MINOR} — update TokenMe on the other machine"
        ))),
        None => Err(TransportError::Unsupported(format!(
            "peer did not announce sync protocol {WIRE_VERSION_MAJOR}.{WIRE_VERSION_MINOR}; update TokenMe on the other machine"
        ))),
    }
}

fn write_response<S: Write>(stream: &mut S, status: u16, content_type: &str, body: &[u8], max_body: usize) -> Result<(), TransportError> {
    if body.len() > max_body { return Err(TransportError::Limit); }
    let reason = match status { 200 => "OK", 400 => "Bad Request", 401 => "Unauthorized", 403 => "Forbidden", 408 => "Request Timeout", 409 => "Conflict", 413 => "Payload Too Large", 426 => "Upgrade Required", 429 => "Too Many Requests", 503 => "Service Unavailable", _ => "Error" };
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n{WIRE_VERSION_HEADER}: {WIRE_VERSION_MAJOR}.{WIRE_VERSION_MINOR}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).map_err(TransportError::from)?;
    stream.write_all(body).map_err(TransportError::from)?;
    stream.flush().map_err(TransportError::from)
}

fn read_until<S: Read>(stream: &mut S, delimiter: &[u8], max: usize) -> Result<Vec<u8>, TransportError> {
    read_until_deadline(stream, delimiter, max, Instant::now().checked_add(DEFAULT_TIMEOUT).ok_or(TransportError::Timeout)?, None)
}

fn read_until_deadline<S: Read>(stream: &mut S, delimiter: &[u8], max: usize, deadline: Instant, cancel: Option<&AtomicBool>) -> Result<Vec<u8>, TransportError> {
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    while !bytes.ends_with(delimiter) {
        if cancel.is_some_and(|flag| flag.load(AtomicOrdering::Acquire)) { return Err(TransportError::Cancelled); }
        if bytes.len() >= max { return Err(TransportError::Limit); }
        if Instant::now() >= deadline { return Err(TransportError::Timeout); }
        stream.read_exact(&mut one).map_err(TransportError::from)?;
        bytes.push(one[0]);
    }
    Ok(bytes)
}

pub fn load_pem_certificate(path: &Path) -> Result<Vec<u8>, TransportError> {
    let bytes = std::fs::read(path).map_err(TransportError::from)?;
    let mut reader = std::io::BufReader::new(bytes.as_slice());
    let cert = rustls_pemfile::certs(&mut reader).next().transpose().map_err(|e| TransportError::Config(format!("read certificate PEM: {e}")))?.ok_or_else(|| TransportError::Config("certificate PEM contains no certificate".into()))?;
    Ok(cert.to_vec())
}

pub fn load_pem_private_key(path: &Path) -> Result<Vec<u8>, TransportError> {
    verify_private_file(path).map_err(|error| TransportError::Config(format!("private key file is not private: {error}")))?;
    let bytes = std::fs::read(path).map_err(TransportError::from)?;
    let mut reader = std::io::BufReader::new(bytes.as_slice());
    let key = rustls_pemfile::private_key(&mut reader).map_err(|e| TransportError::Config(format!("read private key PEM: {e}")))?.ok_or_else(|| TransportError::Config("private key PEM contains no key".into()))?;
    match key {
        PrivateKeyDer::Pkcs8(key) => Ok(key.secret_pkcs8_der().to_vec()),
        _ => Err(TransportError::Config("only PKCS#8 private keys are accepted".into())),
    }
}

/// Load an existing private server certificate or create the pair once in a
/// private directory. Creation is exclusive and guarded by a private lock
/// file, so two service starters cannot silently replace each other's key.
pub fn ensure_server_certificate(cert_path: &Path, key_path: &Path, server_name: &str) -> Result<(Vec<u8>, Vec<u8>), TransportError> {
    if server_name.trim().is_empty() { return Err(TransportError::Config("server certificate name must not be empty".into())); }
    let cert_parent = cert_path.parent().unwrap_or(Path::new("."));
    let key_parent = key_path.parent().unwrap_or(Path::new("."));
    if cert_parent != key_parent { return Err(TransportError::Config("certificate and key must share one private directory".into())); }
    crate::protocol::ensure_private_dir(cert_parent).map_err(TransportError::State)?;
    for path in [cert_path, key_path] {
        if fs::symlink_metadata(path).map(|meta| meta.file_type().is_symlink()).unwrap_or(false) {
            return Err(TransportError::Config(format!("server certificate path is a symlink: {}", path.display())));
        }
    }
    let cert_exists = cert_path.exists();
    let key_exists = key_path.exists();
    if cert_exists != key_exists {
        return Err(TransportError::Config("certificate and key must either both exist or both be absent".into()));
    }
    if cert_exists {
        return Ok((load_pem_certificate(cert_path)?, load_pem_private_key(key_path)?));
    }

    let lock_path = cert_path.with_extension("generation.lock");
    if fs::symlink_metadata(&lock_path).map(|meta| meta.file_type().is_symlink()).unwrap_or(false) {
        return Err(TransportError::Config(format!("server certificate lock is a symlink: {}", lock_path.display())));
    }
    let mut lock_options = OpenOptions::new();
    lock_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options.mode(0o600);
    }
    let lock = lock_options.open(&lock_path).map_err(|error| TransportError::Config(format!("cannot lock server certificate generation: {error}")))?;
    let result = (|| {
        let generated = rcgen::generate_simple_self_signed(vec![server_name.to_string()])
            .map_err(|error| TransportError::Config(format!("generate server certificate: {error}")))?;
        write_private_file_exclusive(cert_path, generated.cert.pem().as_bytes())?;
        write_private_file_exclusive(key_path, generated.key_pair.serialize_pem().as_bytes())?;
        Ok((generated.cert.der().to_vec(), generated.key_pair.serialize_der()))
    })();
    drop(lock);
    let _ = fs::remove_file(&lock_path);
    if result.is_err() {
        let _ = fs::remove_file(cert_path);
        let _ = fs::remove_file(key_path);
    }
    result
}

fn write_private_file_exclusive(path: &Path, bytes: &[u8]) -> Result<(), TransportError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(TransportError::from)?;
    let result = (|| {
        crate::protocol::set_private_file(&file, path).map_err(TransportError::State)?;
        file.write_all(bytes).map_err(TransportError::from)?;
        file.sync_all().map_err(TransportError::from)
    })();
    drop(file);
    if result.is_err() { let _ = fs::remove_file(path); }
    result
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis().min(i64::MAX as u128) as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AuthenticatedDownloadRequest, DeviceIdentity, NetworkCounts, NetworkPackage, NetworkRow, PairingInvite, PROTOCOL_ID, UploadEnvelope};
    use crate::{DiagnosticsHost, DiagnosticsRequestAuth, PairingRequest, PrimarySyncConfig, SyncStore};
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
    use std::thread;

    struct CertFixture {
        cert_der: Vec<u8>,
        key_der: Vec<u8>,
    }

    fn cert() -> CertFixture {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let key_der = key.serialize_der();
        let params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        CertFixture { cert_der: cert.der().to_vec(), key_der }
    }

    fn server_config(cert: &CertFixture) -> HttpsServerConfig {
        HttpsServerConfig { bind_addr: "127.0.0.1:0".parse().unwrap(), certificate_der: cert.cert_der.clone(), private_key_der: cert.key_der.clone(), allow_lan: false, allowed_peer_ips: vec![], max_body_bytes: DEFAULT_BODY_BYTES, max_concurrent: 1, staging_dir: std::env::temp_dir().join(format!("tokenme-sync-test-{}-{}", std::process::id(), now_ms())), idle_timeout: DEFAULT_IDLE_TIMEOUT, total_timeout: DEFAULT_TOTAL_TIMEOUT, staging_quota_bytes: DEFAULT_STAGING_QUOTA_BYTES }
    }

    fn raw_tls_request(cert: &CertFixture, addr: SocketAddr, request: &[u8]) -> String {
        let transport = HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der.clone());
        let config = client_config(&transport).unwrap();
        let name = ServerName::try_from("localhost").unwrap().to_owned();
        let connection = ClientConnection::new(Arc::new(config), name).unwrap();
        let tcp = TcpStream::connect(addr).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut stream = StreamOwned::new(connection, tcp);
        stream.write_all(request).unwrap();
        stream.flush().unwrap();
        let mut response = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    response.extend_from_slice(&buffer[..read]);
                    if response.len() > 64 * 1024 { break; }
                }
            }
        }
        String::from_utf8_lossy(&response).into_owned()
    }

    fn package(device: &DeviceIdentity) -> (SyncStore, SignedUpload) {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let invite = store.create_pairing_invite("https://localhost:47821", "2222222222222222222222222222222222222222222222222222222222222222", 100_000).unwrap();
        let pairing = PairingRequest::new(&invite, device, "linux").unwrap();
        let state = store.receive_pairing_request(&pairing, 1).unwrap();
        store.approve_pairing(&state.request_id, 2).unwrap();
        let stream = store.open_stream(&device.device_id, &device.public_key(), 3).unwrap();
        let row = NetworkRow { key: Some("k".into()), occurrence_id: None, tool: "codex".into(), ts_ms: 10, session: "s".into(), project: None, model: None, meter: "tokens".into(), source: "local".into(), counts: NetworkCounts::all_zero(), calls: vec![] };
        let pkg = NetworkPackage::from_rows(device.origin.clone(), usage_index::INDEX_SCHEMA_VERSION, 0, 100, &[row], 4).unwrap();
        let upload = SignedUpload::sign(UploadEnvelope { protocol_id: PROTOCOL_ID.into(), primary_id: "primary-1".into(), device_id: device.device_id.clone(), origin: device.origin.clone(), stream_id: stream.stream_id, data_epoch: 1, generation: 1, manifest_bytes: pkg.manifest_bytes, gzip_sha256: hash_bytes(&pkg.gzip_bytes) }, &device.signing_key, pkg.gzip_bytes).unwrap();
        (store, upload)
    }

    #[test]
    fn loopback_tls13_upload_round_trip() {
        let cert = cert();
        let device = DeviceIdentity::create("linux", "Linux").unwrap();
        let (store, upload) = package(&device);
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(1));
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der)).unwrap();
        let receipt = client.ws_upload(&upload).unwrap();
        assert_eq!(receipt.inserted, 1);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn loopback_tls13_primary_download_round_trip() {
        let cert = cert();
        let device = DeviceIdentity::create("download-secondary", "Secondary").unwrap();
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let invite = store.create_pairing_invite("https://localhost:47821", &"11".repeat(32), i64::MAX).unwrap();
        let pairing = PairingRequest::new(&invite, &device, "linux").unwrap();
        let pending = store.receive_pairing_request(&pairing, 1).unwrap();
        store.approve_pairing(&pending.request_id, 2).unwrap();
        store.save_primary_config(&PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
            advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: true,
            share_data_with_secondaries: true,
        }, 3).unwrap();
        let now = now_ms();
        store.index().conn().execute(
            "INSERT INTO event(tool, ts_ms, session, meter, in_tok, source, dedupe_key)
             VALUES('codex', ?1, 'session', 'tokens', 9, 'local.jsonl', 'primary-row')",
            [now - 1_000],
        ).unwrap();

        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(1));
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der,
        )).unwrap();
        let request = AuthenticatedDownloadRequest::new_at(&device, "primary-1", None, 0, now).unwrap();
        let page = client.download_primary_page(&request).unwrap();
        let (manifest, rows) = page.package.unwrap().validate().unwrap();
        assert!(page.enabled && !page.preparing && page.complete);
        assert_eq!(manifest.rows, 1);
        assert_eq!(rows[0].key.as_deref(), Some("primary-row"));
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn tls_probe_completes_without_sending_an_http_request() {
        let cert = cert();
        let store = SyncStore::open_in_memory("primary-1").unwrap();
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(2));
        let client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der,
        )).unwrap();
        client.probe_tcp().unwrap();
        client.probe_tls().unwrap();
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn loopback_tls13_diagnostics_mcp_route_uses_a_separate_fixed_protocol() {
        let cert = cert();
        let device = DeviceIdentity::create("diagnostics-client", "Diagnostics client").unwrap();
        let (store, _) = package(&device);
        let host = DiagnosticsHost::default();
        host.set_panel_state(Some("visible".into())).unwrap();
        host.set_build_summary(Some("qa-build".into())).unwrap();
        let server = HttpsSyncServer::bind_with_host(server_config(&cert), store, host).unwrap();
        let diagnostics_handle = server.diagnostics_handle();
        let grant = diagnostics_handle
            .issue_diagnostics_grant(&device.device_id, &[DiagnosticsScope::Read], now_ms(), Duration::from_secs(60))
            .unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(4));
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()),
            "localhost",
            cert.cert_der,
        ))
        .unwrap();
        let make_request = |session_id: Option<String>, method: &str, params: serde_json::Value, request_id: &str| {
            DiagnosticsMcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(serde_json::json!(request_id)),
                method: method.into(),
                auth: DiagnosticsRequestAuth::new_signed(
                    &grant,
                    &device,
                    session_id,
                    method,
                    &params,
                    request_id,
                    now_ms(),
                    now_ms() + 1_000,
                )
                .unwrap(),
                params,
            }
        };
        let initialized = client
            .diagnostics_mcp(&make_request(None, "initialize", serde_json::json!({}), "mcp-init"))
            .unwrap()
            .unwrap();
        let session = initialized.result.unwrap()["sessionId"].as_str().unwrap().to_string();
        let listed = client
            .diagnostics_mcp(&make_request(Some(session.clone()), "tools/list", serde_json::json!({}), "mcp-list"))
            .unwrap()
            .unwrap();
        assert_eq!(listed.result.unwrap()["tools"].as_array().unwrap().len(), crate::diagnostics::diagnostics_tool_registry().len());
        let called = client
            .diagnostics_mcp(&make_request(
                Some(session.clone()),
                "tools/call",
                serde_json::json!({ "name": "device.get_status" }),
                "mcp-call",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(called.result.unwrap()["structuredContent"]["primary_id"], "primary-1");
        let panel = client
            .diagnostics_mcp(&make_request(
                Some(session),
                "tools/call",
                serde_json::json!({ "name": "app.get_panel_state" }),
                "mcp-panel",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(panel.result.unwrap()["structuredContent"]["state"], "visible");
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn discovery_can_issue_a_pinned_short_lived_invite_without_broadcasting_secret() {
        let cert = cert();
        let primary = SyncStore::open_in_memory("primary-1").unwrap();
        let server = HttpsSyncServer::bind(server_config(&cert), primary).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(2));
        let endpoint = format!("https://127.0.0.1:{}", addr.port());
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned_hash(endpoint.clone(), "localhost", hash_bytes(&cert.cert_der))).unwrap();
        let (invite, returned_cert) = client.request_pairing_invite("primary-1", &endpoint).unwrap();
        assert_eq!(returned_cert, cert.cert_der);
        assert_eq!(hash_bytes(&returned_cert), hash_bytes(&cert.cert_der));
        assert_eq!(invite.endpoint, endpoint);
        assert_eq!(invite.secret.len(), 32);
        assert!(invite.expires_at_ms > now_ms());
        let identity = DeviceIdentity::create("discovered-secondary", "Secondary").unwrap();
        let request = PairingRequest::new(&invite, &identity, "linux").unwrap();
        let mut pairing_client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(endpoint.clone(), "localhost", returned_cert)).unwrap();
        let state = pairing_client.send_pairing_request(&request).unwrap();
        assert_eq!(state.status, "pending");
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn independent_sender_outbox_uploads_over_tls_and_ack_clears_only_matching_entry() {
        let cert = cert();
        let device = DeviceIdentity::create("independent-sender", "Sender").unwrap();

        // The primary and sender deliberately use separate SQLite stores. The
        // sender has no device_registry row; ACK validation must use the
        // authenticated receipt origin, not a primary-only registry lookup.
        let mut primary = SyncStore::open_in_memory("primary-1").unwrap();
        let invite = primary.create_pairing_invite("https://localhost:47821", &"6".repeat(64), i64::MAX).unwrap();
        let request = PairingRequest::new(&invite, &device, "linux").unwrap();
        let state = primary.receive_pairing_request(&request, 1).unwrap();
        primary.approve_pairing(&state.request_id, 2).unwrap();
        let stream = primary.open_stream(&device.device_id, &device.public_key(), 3).unwrap();

        let row = NetworkRow { key: Some("independent-key".into()), occurrence_id: None, tool: "codex".into(), ts_ms: 10, session: "sender-session".into(), project: None, model: None, meter: "tokens".into(), source: "sender.jsonl".into(), counts: NetworkCounts::all_zero(), calls: vec![] };
        let package = NetworkPackage::from_rows(device.origin.clone(), usage_index::INDEX_SCHEMA_VERSION, 0, 100, &[row], 4).unwrap();
        let mut sender = SyncStore::open_in_memory("primary-1").unwrap();
        let (outbox_id, _) = sender.enqueue_outbox_auto(&device.device_id, &device.origin, &stream.stream_id, stream.data_epoch, package.manifest_bytes, package.gzip_bytes, &device.signing_key, &[], 0).unwrap();
        sender.acquire_scheduler_lease("sender-runtime", 0, 10_000).unwrap();
        let entry = sender.claim_outbox("sender-runtime", 1, 1).unwrap().pop().unwrap();

        let server = HttpsSyncServer::bind(server_config(&cert), primary).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(1));
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der)).unwrap();
        let receipt = client.ws_upload(&entry.signed_upload()).unwrap();
        sender.ack_outbox("sender-runtime", outbox_id, &receipt, 2).unwrap();
        assert_eq!(sender.outbox_summary().unwrap().pending, 0);
        assert_eq!(sender.outbox_summary().unwrap().in_flight, 0);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn known_path_without_wire_version_is_rejected_before_its_body_is_read() {
        let cert = cert();
        let store = SyncStore::open_in_memory("primary-1").unwrap();
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(1));
        // Announce a body but never send it: the 426 must not wait for it.
        let response = raw_tls_request(&cert, addr, b"POST /v1/status HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 426"), "{response}");
        assert!(response.to_ascii_lowercase().contains("requires sync protocol 2.0"), "{response}");
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn a_known_path_with_a_foreign_wire_major_is_rejected_with_the_peer_version() {
        let cert = cert();
        let store = SyncStore::open_in_memory("primary-1").unwrap();
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(1));
        let response = raw_tls_request(&cert, addr, b"POST /v1/status HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 0\r\nx-tokenme-wire-version: 3.1\r\nConnection: close\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 426"), "{response}");
        assert!(response.contains("peer announced sync protocol 3.1"), "{response}");
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn response_wire_version_mismatch_becomes_an_actionable_upgrade_error() {
        let body = br#"{"message":"update TokenMe on the other machine"}"#;
        assert!(validate_response_wire(200, Some((WIRE_VERSION_MAJOR, 0)), body).is_ok());
        assert!(matches!(validate_response_wire(200, Some((3, 0)), body), Err(TransportError::Unsupported(message)) if message.contains("3.0")));
        assert!(matches!(validate_response_wire(200, None, body), Err(TransportError::Unsupported(message)) if message.contains("did not announce")));
        assert!(matches!(validate_response_wire(426, None, body), Err(TransportError::Unsupported(message)) if message.contains("other machine")));
    }

    #[test]
    fn a_signed_request_nonce_is_answered_only_once() {
        let cert = cert();
        let device = DeviceIdentity::create("replay-secondary", "Replay").unwrap();
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let invite = store.create_pairing_invite("https://localhost:47821", &"22".repeat(32), i64::MAX).unwrap();
        let pairing = PairingRequest::new(&invite, &device, "linux").unwrap();
        let pending = store.receive_pairing_request(&pairing, 1).unwrap();
        store.approve_pairing(&pending.request_id, 2).unwrap();
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(3));
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der)).unwrap();
        let request = AuthenticatedStatusRequest::new_at(&device, "primary-1", false, now_ms()).unwrap();
        assert_eq!(client.status(&request).unwrap().status, "paired");
        let replay = client.status(&request).unwrap_err();
        assert!(matches!(replay, TransportError::HttpStatus { status: 401, .. }), "{replay:?}");
        let next = AuthenticatedStatusRequest::new_at(&device, "primary-1", false, now_ms()).unwrap();
        assert_eq!(client.status(&next).unwrap().status, "paired");
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn nonce_cache_releases_failed_actions_but_never_admits_a_replay() {
        let mut cache = NonceReplayCache::default();
        let nonce = [7u8; 16];
        assert_eq!(cache.claim("status", nonce, 1_000), NonceClaim::Fresh);
        assert_eq!(cache.claim("status", nonce, 1_000), NonceClaim::Replay);
        // The same bytes are a different request on another endpoint.
        assert_eq!(cache.claim("download", nonce, 1_000), NonceClaim::Fresh);
        cache.release("status", nonce);
        assert_eq!(cache.claim("status", nonce, 1_000), NonceClaim::Fresh);
        // Entries age out after the signed-request freshness window.
        assert_eq!(cache.claim("download", [8u8; 16], 1_000 + REPLAY_RETENTION_MS + 1), NonceClaim::Fresh);
        assert_eq!(cache.claim("status", nonce, 1_000 + REPLAY_RETENTION_MS + 1), NonceClaim::Fresh);
    }

    #[test]
    fn download_page_wire_is_binary_and_rejects_trailing_or_future_frames() {
        let device = DeviceIdentity::create("download-wire", "Wire").unwrap();
        let request = AuthenticatedDownloadRequest::new_at(&device, "primary-1", None, 0, now_ms()).unwrap();
        let row = NetworkRow { key: Some("key:p".into()), occurrence_id: None, tool: "codex".into(), ts_ms: 10, session: "s".into(), project: None, model: None, meter: "tokens".into(), source: "local".into(), counts: NetworkCounts::all_zero(), calls: vec![] };
        let package = NetworkPackage::from_rows_with_snapshot_complete(device.origin.clone(), usage_index::INDEX_SCHEMA_VERSION, 0, 100, &[row], 4, true).unwrap();
        let page = PrimaryDownloadPage { request_nonce: request.nonce, enabled: true, preparing: false, package: Some(package.clone()), next_identity: Some("key:p".into()), next_dirty_revision: 4, complete: true };
        let encoded = encode_primary_download_page(page).unwrap();
        assert!(encoded.starts_with(DOWNLOAD_MAGIC));
        assert_ne!(encoded.first(), Some(&b'{'));
        assert!(encoded.windows(package.gzip_bytes.len()).any(|window| window == package.gzip_bytes.as_slice()));
        let decoded = decode_primary_download_page(&encoded, &request).unwrap();
        assert!(decoded.enabled && decoded.complete && !decoded.preparing);
        assert_eq!(decoded.request_nonce, request.nonce);
        assert_eq!(decoded.next_identity.as_deref(), Some("key:p"));
        assert_eq!(decoded.next_dirty_revision, 4);
        assert_eq!(decoded.package, Some(package));
        let mut future = encoded.clone();
        future[8] = DOWNLOAD_PAGE_VERSION + 1;
        assert!(matches!(decode_primary_download_page(&future, &request), Err(TransportError::Unsupported(_))));
        let mut trailing = encoded;
        trailing.push(0);
        assert!(matches!(decode_primary_download_page(&trailing, &request), Err(TransportError::Protocol(message)) if message.contains("trailing")));
    }

    #[test]
    fn a_request_scoped_store_failure_closes_only_that_connection() {
        let cert = cert();
        let device = DeviceIdentity::create("linux", "Linux").unwrap();
        let (store, upload) = package(&device);
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        server.context.fault_next_connection.store(1, AtomicOrdering::Release);
        let handle = thread::spawn(move || server.run_connections(2));

        let mut faulted = TcpStream::connect(addr).unwrap();
        let _ = faulted.write_all(b"GET /v1/status HTTP/1.1\r\n\r\n");
        let _ = faulted.shutdown(Shutdown::Both);
        thread::sleep(Duration::from_millis(25));

        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der)).unwrap();
        let receipt = client.ws_upload(&upload).unwrap();
        assert_eq!(receipt.inserted, 1);
        assert!(handle.join().unwrap().is_ok(), "a request-scoped store failure must not stop the listener");
    }

    #[test]
    fn a_panicking_connection_does_not_stop_the_following_valid_upload() {
        let cert = cert();
        let device = DeviceIdentity::create("linux", "Linux").unwrap();
        let (store, upload) = package(&device);
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        server.context.fault_next_connection.store(2, AtomicOrdering::Release);
        let handle = thread::spawn(move || server.run_connections(2));

        let mut faulted = TcpStream::connect(addr).unwrap();
        let _ = faulted.write_all(b"GET /v1/status HTTP/1.1\r\n\r\n");
        let _ = faulted.shutdown(Shutdown::Both);
        thread::sleep(Duration::from_millis(25));

        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der)).unwrap();
        let receipt = client.ws_upload(&upload).unwrap();
        assert_eq!(receipt.inserted, 1);
        assert!(handle.join().unwrap().is_ok(), "a panicking handler must not stop the listener");
    }

    #[test]
    fn a_poisoned_store_lock_recovers_instead_of_stopping_the_listener() {
        let cert = cert();
        let device = DeviceIdentity::create("linux", "Linux").unwrap();
        let (store, upload) = package(&device);
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        let store_lock = Arc::clone(&server.context.store);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store_lock.lock().unwrap();
            panic!("poison the store lock");
        }));
        assert!(store_lock.is_poisoned());
        let handle = thread::spawn(move || server.run_connections(1));

        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der)).unwrap();
        let receipt = client.ws_upload(&upload).unwrap();
        assert_eq!(receipt.inserted, 1);
        assert!(handle.join().unwrap().is_ok(), "a poisoned lock must not take the listener down");
    }

    #[test]
    fn bad_connection_does_not_stop_the_following_valid_upload() {
        let cert = cert();
        let device = DeviceIdentity::create("linux", "Linux").unwrap();
        let (store, upload) = package(&device);
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(2));

        let mut bad = TcpStream::connect(addr).unwrap();
        bad.write_all(b"not TLS or HTTP").unwrap();
        let _ = bad.shutdown(Shutdown::Both);
        thread::sleep(Duration::from_millis(25));

        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der)).unwrap();
        let receipt = client.ws_upload(&upload).unwrap();
        assert_eq!(receipt.inserted, 1);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn cancelling_a_blackhole_status_closes_the_client_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bytes = Vec::new();
            let _ = socket.read_to_end(&mut bytes);
        });
        let device = DeviceIdentity::create("cancel-status", "Linux").unwrap();
        let request = crate::AuthenticatedStatusRequest::new_at(&device, "primary-1", true, now_ms()).unwrap();
        let cert = cert();
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der,
        )).unwrap();
        let cancellation = client.cancellation();
        let (tx, rx) = std::sync::mpsc::channel();
        let join = thread::spawn(move || tx.send(client.status(&request)).unwrap());
        thread::sleep(Duration::from_millis(50));
        cancellation.cancel();
        let result = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(result, Err(TransportError::Cancelled)), "result: {result:?}");
        join.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn cancelling_a_blackhole_upload_closes_the_client_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bytes = Vec::new();
            let _ = socket.read_to_end(&mut bytes);
        });
        let device = DeviceIdentity::create("cancel-upload", "Linux").unwrap();
        let (_, upload) = package(&device);
        let cert = cert();
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der,
        )).unwrap();
        let cancellation = client.cancellation();
        let (tx, rx) = std::sync::mpsc::channel();
        let join = thread::spawn(move || tx.send(client.ws_upload(&upload)).unwrap());
        thread::sleep(Duration::from_millis(50));
        cancellation.cancel();
        let result = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(result, Err(TransportError::Cancelled)), "result: {result:?}");
        join.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn cancellation_after_tls_handshake_unblocks_a_status_waiting_for_http_response() {
        let cert = cert();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server_cert = cert.cert_der.clone();
        let server_key = cert.key_der.clone();
        let server = thread::spawn(move || {
            let (mut tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let config = HttpsServerConfig {
                bind_addr: addr,
                certificate_der: server_cert,
                private_key_der: server_key,
                allow_lan: false,
                allowed_peer_ips: vec![],
                max_body_bytes: DEFAULT_BODY_BYTES,
                max_concurrent: 1,
                staging_dir: std::env::temp_dir().join(format!("tokenme-sync-cancel-http-{}", now_ms())),
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
                total_timeout: DEFAULT_TOTAL_TIMEOUT,
                staging_quota_bytes: DEFAULT_STAGING_QUOTA_BYTES,
            };
            let mut connection = ServerConnection::new(Arc::new(super::server_config(&config).unwrap())).unwrap();
            while connection.is_handshaking() { connection.complete_io(&mut tcp).unwrap(); }
            let mut tls = StreamOwned::new(connection, tcp);
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8 * 1024, "HTTP request header exceeded test bound");
                let count = tls.read(&mut byte).unwrap();
                assert_eq!(count, 1, "client closed before sending a complete HTTP request");
                request.push(byte[0]);
            }
            ready_tx.send(()).unwrap();
            let mut buffer = [0u8; 1024];
            loop {
                match tls.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        let device = DeviceIdentity::create("cancel-http-status", "Linux").unwrap();
        let request = crate::AuthenticatedStatusRequest::new_at(&device, "primary-1", true, now_ms()).unwrap();
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der,
        )).unwrap();
        let cancellation = client.cancellation();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let join = thread::spawn(move || result_tx.send(client.status(&request)).unwrap());
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        cancellation.cancel();
        let result = result_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(result, Err(TransportError::Cancelled)), "result: {result:?}");
        join.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn generated_server_certificate_is_private_and_reused_without_overwrite() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let cert_path = root.path().join("server-cert.pem");
        let key_path = root.path().join("server-key.pem");
        let (first_cert, first_key) = ensure_server_certificate(&cert_path, &key_path, "localhost").unwrap();
        let (second_cert, second_key) = ensure_server_certificate(&cert_path, &key_path, "localhost").unwrap();
        assert_eq!(first_cert, second_cert);
        assert_eq!(first_key, second_key);
        assert_eq!(load_pem_certificate(&cert_path).unwrap(), first_cert);
        assert_eq!(load_pem_private_key(&key_path).unwrap(), first_key);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&cert_path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn server_certificate_generation_lock_rejects_a_concurrent_creator() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let cert_path = root.path().join("server-cert.pem");
        let key_path = root.path().join("server-key.pem");
        let lock_path = cert_path.with_extension("generation.lock");
        let lock = OpenOptions::new().write(true).create_new(true).open(&lock_path).unwrap();
        let result = ensure_server_certificate(&cert_path, &key_path, "localhost");
        drop(lock);
        std::fs::remove_file(&lock_path).unwrap();
        assert!(matches!(result, Err(TransportError::Config(message)) if message.contains("lock")));
        let _ = ensure_server_certificate(&cert_path, &key_path, "localhost").unwrap();
    }

    #[test]
    fn stale_upload_staging_from_a_crash_is_swept_on_bind() {
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join("upload-0123456789abcdef.part");
        let fresh = root.path().join("upload-fedcba9876543210.part");
        let other = root.path().join("notes.txt");
        fs::write(&stale, b"crashed").unwrap();
        fs::write(&fresh, b"live").unwrap();
        fs::write(&other, b"keep").unwrap();
        OpenOptions::new().write(true).open(&stale).unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3_600)).unwrap();
        let removed = sweep_stale_staging(root.path(), Duration::from_secs(600), SystemTime::now());
        assert_eq!(removed, 1);
        assert!(!stale.exists(), "crash residue older than the request budget must not survive a restart");
        assert!(fresh.exists(), "a fresh staging file can belong to a live upload");
        assert!(other.exists(), "the sweep only owns upload-*.part");
    }

    #[test]
    fn pin_verifier_checks_validity_and_name_not_only_the_leaf_hash() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let name = ServerName::try_from("localhost").unwrap().to_owned();
        let verify = |params: CertificateParams, now: UnixTime| {
            let cert = params.self_signed(&key).unwrap();
            let verifier = PinOnlyVerifier { expected: hash_bytes(cert.der()) };
            verifier.verify_server_cert(&CertificateDer::from(cert.der().to_vec()), &[], &name, &[], now)
        };
        assert!(verify(CertificateParams::new(vec!["localhost".into()]).unwrap(), UnixTime::now()).is_ok());
        let wrong_name = CertificateParams::new(vec!["other.example".into()]).unwrap();
        let Err(error) = verify(wrong_name, UnixTime::now()) else { panic!("a pinned certificate for another host must be rejected") };
        assert!(error.to_string().contains("does not name the endpoint"), "{error}");
        let mut expired = CertificateParams::new(vec!["localhost".into()]).unwrap();
        expired.not_before = rcgen::date_time_ymd(2020, 1, 1);
        expired.not_after = rcgen::date_time_ymd(2020, 2, 1);
        let Err(error) = verify(expired, UnixTime::now()) else { panic!("a pinned certificate outside its validity window must be rejected") };
        assert!(error.to_string().contains("failed validation"), "{error}");
    }

    #[test]
    fn pin_mismatch_happens_before_any_request_bytes() {
        let actual = cert();
        let expected = cert();
        let device = DeviceIdentity::create("linux", "Linux").unwrap();
        let (store, upload) = package(&device);
        let server = HttpsSyncServer::bind(server_config(&actual), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(1));
        let mut config = HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", expected.cert_der);
        config.trust_anchors_der.push(actual.cert_der);
        let mut client = Tls13HttpsTransport::new(config).unwrap();
        assert!(matches!(client.ws_upload(&upload), Err(TransportError::PinMismatch)));
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn tcp_connect_attempt_uses_the_full_remaining_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = thread::spawn(move || listener.accept().unwrap());
        let deadline = Instant::now() + CONNECT_POLL_SLICE + Duration::from_millis(50);
        let stream = connect_tcp_with_deadline(&addr, deadline, &AtomicBool::new(false)).unwrap();
        drop(stream);
        accept.join().unwrap();
    }

    #[test]
    fn tls_probe_rejects_a_mismatched_leaf_pin() {
        let actual = cert();
        let expected = cert();
        let store = SyncStore::open_in_memory("primary-1").unwrap();
        let server = HttpsSyncServer::bind(server_config(&actual), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(1));
        let mut config = HttpsTransportConfig::pinned_hash(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", hash_bytes(&expected.cert_der),
        );
        config.trust_anchors_der.push(actual.cert_der);
        let client = Tls13HttpsTransport::new(config).unwrap();
        assert!(matches!(client.probe_tls(), Err(TransportError::PinMismatch)));
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn tls_probe_fails_when_the_listener_is_stopped() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let cert = cert();
        let client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der,
        )).unwrap();
        assert!(client.probe_tcp().is_err());
        assert!(client.probe_tls().is_err());
    }

    #[test]
    fn server_health_transitions_from_ready_to_running_to_stopped() {
        let cert = cert();
        let server = HttpsSyncServer::bind(server_config(&cert), SyncStore::open_in_memory("primary-1").unwrap()).unwrap();
        let health = server.diagnostics_handle();
        assert_eq!(health.health(), "ready");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || server.run_until(&worker_stop));
        let deadline = Instant::now() + Duration::from_secs(1);
        while health.health() == "ready" && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(health.health(), "running");
        stop.store(true, AtomicOrdering::Release);
        assert!(worker.join().unwrap().is_ok());
        assert_eq!(health.health(), "stopped");
    }

    #[test]
    fn slow_trickle_response_is_bounded_by_one_absolute_request_deadline() {
        let cert = cert();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_cert = cert.cert_der.clone();
        let server_key = cert.key_der.clone();
        let server = thread::spawn(move || {
            let (mut tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let config = HttpsServerConfig {
                bind_addr: addr,
                certificate_der: server_cert,
                private_key_der: server_key,
                allow_lan: false,
                allowed_peer_ips: vec![],
                max_body_bytes: DEFAULT_BODY_BYTES,
                max_concurrent: 1,
                staging_dir: std::env::temp_dir().join(format!("tokenme-sync-trickle-{}", now_ms())),
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
                total_timeout: DEFAULT_TOTAL_TIMEOUT,
                staging_quota_bytes: DEFAULT_STAGING_QUOTA_BYTES,
            };
            let mut connection = ServerConnection::new(Arc::new(super::server_config(&config).unwrap())).unwrap();
            while connection.is_handshaking() {
                if connection.complete_io(&mut tcp).is_err() { return; }
            }
            let mut tls = StreamOwned::new(connection, tcp);
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && request.len() < MAX_HEADER_BYTES {
                match tls.read(&mut byte) {
                    Ok(1) => request.push(byte[0]),
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
            if !request.ends_with(b"\r\n\r\n") { return; }
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n";
            if tls.write_all(response).is_err() || tls.flush().is_err() { return; }
            for byte in b"{\"x\":1234}" {
                if tls.write_all(&[*byte]).is_err() || tls.flush().is_err() { break; }
                thread::sleep(Duration::from_millis(45));
            }
        });
        let device = DeviceIdentity::create("slow-trickle", "Linux").unwrap();
        let request = AuthenticatedStatusRequest::new_at(&device, "primary-1", true, now_ms()).unwrap();
        let mut config = HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der,
        );
        config.connect_timeout = Duration::from_secs(1);
        config.request_timeout = Duration::from_millis(150);
        let mut client = Tls13HttpsTransport::new(config).unwrap();
        let started = Instant::now();
        let result = client.status(&request);
        assert!(matches!(result, Err(TransportError::Timeout)), "result: {result:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "request exceeded bounded test window: {:?}", started.elapsed());
        server.join().unwrap();
    }

    #[test]
    fn pairing_secret_is_sent_only_after_pin_check() {
        let cert = cert();
        let device = DeviceIdentity::create("linux", "Linux").unwrap();
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let invite: PairingInvite = store.create_pairing_invite("https://localhost:47821", "3333333333333333333333333333333333333333333333333333333333333333", i64::MAX).unwrap();
        let request = PairingRequest::new(&invite, &device, "linux").unwrap();
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(1));
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der)).unwrap();
        let state = client.send_pairing_request(&request).unwrap();
        assert_eq!(state.status, "pending");
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn loopback_tls13_disconnect_revokes_the_authenticated_device() {
        let cert = cert();
        let device = DeviceIdentity::create("disconnect-loopback", "Linux").unwrap();
        let (store, _upload) = package(&device);
        let server = HttpsSyncServer::bind(server_config(&cert), store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(2));
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der,
        )).unwrap();
        let request = AuthenticatedStatusRequest::new_disconnect_at(&device, "primary-1", now_ms()).unwrap();
        client.disconnect(&request).unwrap();
        let status_request = AuthenticatedStatusRequest::new_at(&device, "primary-1", false, now_ms()).unwrap();
        assert_eq!(client.status(&status_request).unwrap().status, "revoked");
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn non_loopback_bind_requires_explicit_lan_opt_in() {
        let cert = cert();
        let mut config = server_config(&cert);
        config.bind_addr = "0.0.0.0:9443".parse().unwrap();
        assert!(matches!(config.validate(), Err(TransportError::Config(message)) if message.contains("allow_lan")));
    }

    #[test]
    fn lan_opt_in_server_round_trip_uses_the_same_tls_path() {
        let cert = cert();
        let device = DeviceIdentity::create("lan-linux", "Linux").unwrap();
        let (store, upload) = package(&device);
        let mut config = server_config(&cert);
        config.bind_addr = "0.0.0.0:0".parse().unwrap();
        config.allow_lan = true;
        let server = HttpsSyncServer::bind(config, store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(1));
        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der)).unwrap();
        let receipt = client.ws_upload(&upload).unwrap();
        assert_eq!(receipt.inserted, 1);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn websocket_roundtrip_over_pinned_tls() {
        let cert = cert();
        let device = DeviceIdentity::create("ws-test", "macOS").unwrap();
        let (store, _upload) = package(&device);
        let config = server_config(&cert);
        let server = HttpsSyncServer::bind(config, store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = thread::spawn(move || server.run_connections(1));
        let client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(format!("https://127.0.0.1:{}", addr.port()), "localhost", cert.cert_der)).unwrap();
        let (mut ws, _guard) = client.connect_ws().unwrap();
        let ping = crate::ws::WsSyncFrame::Ping { client_time_ms: 12345 };
        let text = serde_json::to_string(&ping).unwrap();
        ws.send(tungstenite::Message::Text(text)).unwrap();
        let resp = ws.read().unwrap();
        match resp {
            tungstenite::Message::Text(resp_text) => {
                let frame: crate::ws::WsSyncFrame = serde_json::from_str(&resp_text).unwrap();
                match frame {
                    crate::ws::WsSyncFrame::Pong { server_time_ms } => {
                        assert!(server_time_ms > 0);
                    }
                    _ => panic!("expected pong frame"),
                }
            }
            _ => panic!("expected text frame"),
        }
        let _ = ws.close(None);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn endpoint_limits_are_explicit() {
        let cert = cert();
        let mut client = HttpsTransportConfig::pinned("http://127.0.0.1:1", "localhost", cert.cert_der);
        assert!(matches!(client.validate(), Err(TransportError::Config(message)) if message.contains("https://")));
        client.endpoint = "http://10.0.0.112:47822/mcp".into();
        assert!(client.validate().is_err(), "the standard HTTP URL must not downgrade the pinned TLS transport");
        client.endpoint = "https://127.0.0.1:1/path".into();
        assert!(client.validate().is_err());
    }
}

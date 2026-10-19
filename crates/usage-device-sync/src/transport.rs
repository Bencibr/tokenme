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
use std::time::{Duration, Instant};

use base64::Engine as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, ServerConfig, ServerConnection, SignatureScheme, StreamOwned};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::diagnostics::{
    DiagnosticsError, DiagnosticsGrant, DiagnosticsHost, DiagnosticsMcpRequest, DiagnosticsMcpResponse,
    DiagnosticsScope, DiagnosticsService, DIAGNOSTICS_MCP_PATH, DIAGNOSTICS_REQUEST_TIMEOUT,
    MAX_DIAGNOSTICS_BODY_BYTES, MAX_DIAGNOSTICS_RESPONSE_BYTES,
};
use crate::protocol::{
    hash_bytes, verify_private_file, AuthenticatedStatus, AuthenticatedStatusRequest, PairingInvite, PairingRequest,
    SignedUpload, StreamOpenReceipt, UploadEnvelope, MAX_COMPRESSED_BYTES, MAX_MANIFEST_BYTES,
    MAX_STRING_BYTES,
};
use crate::{ImportReceipt, PairingRequestState, SyncError, SyncStore};

const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_RESPONSE_BYTES: usize = MAX_DIAGNOSTICS_RESPONSE_BYTES;
const DEFAULT_BODY_BYTES: usize = 128 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_STAGING_QUOTA_BYTES: usize = 512 * 1024 * 1024;
const MAX_GLOBAL_CONCURRENT: usize = 4;
const MAX_DEVICE_CONCURRENT: usize = 1;
const MAX_CONTROL_BODY_BYTES: usize = 64 * 1024;
const MAX_PAIRING_PER_IP: usize = 5;
const PAIRING_WINDOW: Duration = Duration::from_secs(60);
const DISCOVERY_INVITE_TTL_MS: i64 = 5 * 60 * 1000;
const MAX_TRACKED_PAIRING_IPS: usize = 1024;
const UPLOAD_MAGIC: &[u8; 8] = b"TMSYNC01";
const UPLOAD_WIRE_VERSION: u8 = 1;
const UPLOAD_PATH: &str = "/v1/upload";
const PAIRING_PATH: &str = "/v1/pairing/request";
const PAIRING_INVITE_PATH: &str = "/v1/pairing/invite";
const STATUS_PATH: &str = "/v1/status";
const RECOVERY_PATH: &str = "/v1/recovery";
const DISCONNECT_PATH: &str = "/v1/disconnect";
const CONNECT_POLL_SLICE: Duration = Duration::from_millis(100);

const HEALTH_READY: u8 = 0;
const HEALTH_RUNNING: u8 = 1;
const HEALTH_STOPPING: u8 = 2;
const HEALTH_STOPPED: u8 = 3;

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
        if let Ok(active) = self.active_sockets.lock() {
            for stream in active.values() { let _ = stream.shutdown(Shutdown::Both); }
        }
    }
}

struct ClientSocketGuard {
    active_sockets: Arc<Mutex<BTreeMap<u64, TcpStream>>>,
    id: u64,
}

impl Drop for ClientSocketGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active_sockets.lock() { active.remove(&self.id); }
    }
}

/// Client-side transport contract shared by the future CLI and Panel
/// lifecycle code. The implementation below is the real TLS1.3 HTTPS path;
/// there is no NotImplemented transport in this crate anymore.
pub trait DeviceSyncTransport {
    type Error: std::error::Error + Send + Sync + 'static;

    fn upload(&mut self, upload: &SignedUpload) -> Result<ImportReceipt, Self::Error>;
}

impl Tls13HttpsTransport {
    pub fn new(config: HttpsTransportConfig) -> Result<Self, TransportError> {
        config.validate()?;
        Ok(Self {
            config,
            cancel: Arc::new(AtomicBool::new(false)),
            active_sockets: Arc::new(Mutex::new(BTreeMap::new())),
            next_socket_id: Arc::new(AtomicU64::new(1)),
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

    pub fn upload(&mut self, upload: &SignedUpload) -> Result<ImportReceipt, TransportError> {
        let prefix = encode_upload_prefix(upload)?;
        let response = self.request_upload(&prefix, &upload.gzip_bytes)?;
        response.json()
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
        let content_type = if path == UPLOAD_PATH { "application/octet-stream" } else { "application/json" };
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.config.server_name, body.len()
        );
        stream.write_all(request.as_bytes()).map_err(|error| self.map_io(error))?;
        stream.write_all(&body).map_err(|error| self.map_io(error))?;
        stream.flush().map_err(|error| self.map_io(error))?;
        read_response(&mut stream, self.config.max_response_bytes).map_err(|error| self.map_cancelled(error))
    }

    fn request_upload(&self, prefix: &[u8], gzip: &[u8]) -> Result<HttpResponse, TransportError> {
        let body_len = prefix.len().checked_add(gzip.len()).ok_or(TransportError::Limit)?;
        if body_len > DEFAULT_BODY_BYTES { return Err(TransportError::Limit); }
        let (mut stream, _socket) = self.connect()?;
        self.check_cancelled()?;
        let request = format!(
            "POST {UPLOAD_PATH} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {body_len}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
            self.config.server_name
        );
        stream.write_all(request.as_bytes()).map_err(|error| self.map_io(error))?;
        stream.write_all(prefix).map_err(|error| self.map_io(error))?;
        stream.flush().map_err(|error| self.map_io(error))?;

        let head = read_response_head(&mut stream).map_err(|error| self.map_cancelled(error))?;
        if head.status == 100 {
            self.check_cancelled()?;
            stream.write_all(gzip).map_err(|error| self.map_io(error))?;
            stream.flush().map_err(|error| self.map_io(error))?;
            read_response(&mut stream, self.config.max_response_bytes).map_err(|error| self.map_cancelled(error))
        } else {
            read_response_body(&mut stream, head, self.config.max_response_bytes).map_err(|error| self.map_cancelled(error))
        }
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
        let mut active = self.active_sockets.lock().map_err(|_| TransportError::Config("client socket lock poisoned".into()))?;
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

impl DeviceSyncTransport for Tls13HttpsTransport {
    type Error = TransportError;

    fn upload(&mut self, upload: &SignedUpload) -> Result<ImportReceipt, Self::Error> {
        Tls13HttpsTransport::upload(self, upload)
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
        let store = self.context.store.lock().map_err(|_| TransportError::State(SyncError::Security("sync store lock poisoned".into())))?;
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
    diagnostics: Arc<DiagnosticsService>,
    pairing_limiter: PairingRateLimiter,
    gate: Arc<ConcurrencyGate>,
    config: HttpsServerConfig,
    cancel: Arc<AtomicBool>,
    health: AtomicU8,
    active_sockets: Mutex<BTreeMap<u64, TcpStream>>,
    next_socket_id: AtomicU64,
    log_sink: Mutex<Option<Arc<dyn Fn(bool, &str) + Send + Sync>>>,
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
        let Ok(mut attempts) = self.attempts.lock() else { return false; };
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

struct GateState {
    active: usize,
    staged_bytes: usize,
    device_active: BTreeMap<String, usize>,
}

struct ConcurrencyGate {
    state: Mutex<GateState>,
    changed: Condvar,
    max_concurrent: usize,
    staging_quota_bytes: usize,
}

struct ConnectionPermit {
    gate: Arc<ConcurrencyGate>,
}

struct DevicePermit {
    gate: Arc<ConcurrencyGate>,
    device_id: String,
    staged_bytes: usize,
}

impl ConcurrencyGate {
    fn new(config: &HttpsServerConfig) -> Self {
        Self {
            state: Mutex::new(GateState { active: 0, staged_bytes: 0, device_active: BTreeMap::new() }),
            changed: Condvar::new(),
            max_concurrent: config.max_concurrent,
            staging_quota_bytes: config.staging_quota_bytes,
        }
    }

    fn try_acquire_connection(self: &Arc<Self>) -> Result<Option<ConnectionPermit>, TransportError> {
        let Ok(mut state) = self.state.lock() else { return Err(TransportError::Config("connection gate lock poisoned".into())); };
        if state.active >= self.max_concurrent {
            return Ok(None);
        }
        state.active += 1;
        Ok(Some(ConnectionPermit { gate: Arc::clone(self) }))
    }

    fn acquire_device(self: &Arc<Self>, device_id: &str, bytes: usize) -> Result<DevicePermit, TransportError> {
        if bytes > self.staging_quota_bytes { return Err(TransportError::Limit); }
        let Ok(mut state) = self.state.lock() else { return Err(TransportError::Config("connection gate lock poisoned".into())); };
        let active = state.device_active.get(device_id).copied().unwrap_or(0);
        if active >= MAX_DEVICE_CONCURRENT || state.staged_bytes.saturating_add(bytes) > self.staging_quota_bytes {
            return Err(TransportError::Busy);
        }
        *state.device_active.entry(device_id.to_string()).or_default() += 1;
        state.staged_bytes += bytes;
        Ok(DevicePermit { gate: Arc::clone(self), device_id: device_id.to_string(), staged_bytes: bytes })
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.active = state.active.saturating_sub(1);
            self.gate.changed.notify_all();
        }
    }
}

impl Drop for DevicePermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.staged_bytes = state.staged_bytes.saturating_sub(self.staged_bytes);
            if let Some(active) = state.device_active.get_mut(&self.device_id) {
                *active = active.saturating_sub(1);
                if *active == 0 { state.device_active.remove(&self.device_id); }
            }
            self.gate.changed.notify_all();
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
    pub fn bind_with_host(config: HttpsServerConfig, store: SyncStore, host: DiagnosticsHost) -> Result<Self, TransportError> {
        Self::bind_with_host_options(config, store, host, false)
    }

    /// Desktop services can recover from a port occupied by another process.
    /// Keep the bind interface and TLS identity, then publish the actual port.
    pub fn bind_with_host_fallback(config: HttpsServerConfig, store: SyncStore, host: DiagnosticsHost) -> Result<Self, TransportError> {
        Self::bind_with_host_options(config, store, host, true)
    }

    fn bind_with_host_options(config: HttpsServerConfig, store: SyncStore, host: DiagnosticsHost, fallback: bool) -> Result<Self, TransportError> {
        config.validate()?;
        crate::protocol::ensure_private_dir(&config.staging_dir).map_err(TransportError::State)?;
        let tls_config = server_config(&config)?;
        let listener = match TcpListener::bind(config.bind_addr) {
            Err(error) if fallback && error.kind() == io::ErrorKind::AddrInUse => TcpListener::bind(SocketAddr::new(config.bind_addr.ip(), 0)),
            result => result,
        }.map_err(TransportError::from)?;
        listener.set_nonblocking(false).map_err(TransportError::from)?;
        let diagnostics = DiagnosticsService::new_with_host(store.primary_id(), host).map_err(TransportError::Diagnostics)?;
        let context = ServerContext {
            tls_config: Arc::new(tls_config),
            store: Arc::new(Mutex::new(store)),
            diagnostics: Arc::new(diagnostics),
            pairing_limiter: PairingRateLimiter::new(),
            gate: Arc::new(ConcurrencyGate::new(&config)),
            config,
            cancel: Arc::new(AtomicBool::new(false)),
            health: AtomicU8::new(HEALTH_READY),
            active_sockets: Mutex::new(BTreeMap::new()),
            next_socket_id: AtomicU64::new(1),
            log_sink: Mutex::new(None),
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
        *self.context.log_sink.lock().map_err(|_| {
            TransportError::Config("sync log sink lock poisoned".into())
        })? = Some(Arc::new(sink));
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
                match self.context.handle_connection(stream, peer, permit) {
                    Ok(()) => {}
                    Err(TransportError::State(error)) => return Err(TransportError::State(error)),
                    Err(_) => {}
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
                        let started = Instant::now();
                        if let Err(error) = context.handle_connection(stream, peer, permit) {
                            let code = transport_error_code(&error);
                            context.log_event(true, &format!(
                                "sync: action=connection outcome=error error_code={code} total_ms={}",
                                started.elapsed().as_millis(),
                            ));
                            if matches!(error, TransportError::State(_) | TransportError::Config(_)) {
                                let _ = fatal_tx.send(error);
                            }
                        }
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    context.cancel_active();
                    for handler in handlers { let _ = handler.join(); }
                    context.health.store(HEALTH_STOPPED, AtomicOrdering::Release);
                    return Err(TransportError::from(error));
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
        let sink = self.log_sink.lock().ok().and_then(|slot| slot.clone());
        if let Some(sink) = sink {
            sink(is_error, message);
        } else if is_error {
            eprintln!("tokenme sync: {message}");
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
        let mut active = self.active_sockets.lock().map_err(|_| TransportError::Config("active socket lock poisoned".into()))?;
        if self.cancel.load(AtomicOrdering::Acquire) {
            let _ = clone.shutdown(Shutdown::Both);
            return Err(TransportError::Cancelled);
        }
        active.insert(id, clone);
        Ok(ActiveSocketGuard { context: self, id })
    }

    fn cancel_active(&self) {
        self.diagnostics.stop();
        if let Ok(active) = self.active_sockets.lock() {
            self.cancel.store(true, AtomicOrdering::Release);
            for stream in active.values() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        } else {
            self.cancel.store(true, AtomicOrdering::Release);
        }
    }

    fn unregister_socket(&self, id: u64) {
        if let Ok(mut active) = self.active_sockets.lock() {
            active.remove(&id);
        }
    }

    fn handle_connection(&self, stream: TcpStream, peer: SocketAddr, _connection: ConnectionPermit) -> Result<(), TransportError> {
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
        let (status, response) = if head.known_path.is_none() {
            (404, serde_json::to_vec(&WireError { message: "unknown sync endpoint".into() }).unwrap_or_else(|_| b"{\"message\":\"not found\"}".to_vec()))
        } else if head.path == UPLOAD_PATH {
            let metadata = read_upload_metadata(&mut tls, head.content_length, self.config.max_body_bytes, deadline, &self.cancel)?;
            let authorize_started = Instant::now();
            let authorized = self.store.lock().map_err(|_| TransportError::State(SyncError::Security("sync store lock poisoned".into())))
                .and_then(|store| store.authorize_upload_metadata(&metadata.envelope, &metadata.signature).map_err(TransportError::State));
            if let Err(error) = authorized {
                if matches!(&error, TransportError::State(SyncError::Sqlite(_))) {
                    let code = transport_error_code(&error);
                    let detail = match &error {
                        TransportError::State(SyncError::Sqlite(message)) => format!(" sqlite={message}"),
                        _ => String::new(),
                    };
                    self.log_event(true, &format!(
                        "sync: action=upload_authorize outcome=error error_code={code}{detail} total_ms={}",
                        authorize_started.elapsed().as_millis(),
                    ));
                }
                let (status, message) = match &error {
                    TransportError::State(error) => (status_for_error(error), safe_error_message(error)),
                    _ => (400, "request rejected".into()),
                };
                (status, serde_json::to_vec(&WireError { message }).unwrap_or_else(|_| b"{\"message\":\"request rejected\"}".to_vec()))
            } else {
                match self.gate.acquire_device(&metadata.envelope.device_id, metadata.gzip_len) {
                    Err(TransportError::Limit) => (413, serde_json::to_vec(&WireError { message: "sync package exceeds the staging quota".into() }).unwrap_or_else(|_| b"{\"message\":\"sync package exceeds staging quota\"}".to_vec())),
                    Err(TransportError::Busy) => (503, serde_json::to_vec(&WireError { message: "sync service is busy; retry later".into() }).unwrap_or_else(|_| b"{\"message\":\"sync service is busy; retry later\"}".to_vec())),
                    Err(error) => return Err(error),
                    Ok(_device) => {
                        if head.expect_continue { write_continue(&mut tls)?; }
                        let staged = stage_upload(&mut tls, &metadata, &self.config.staging_dir, deadline, &self.cancel)?;
                        let import_started = Instant::now();
                        let result = (|| {
                            if self.cancel.load(AtomicOrdering::Acquire) { return Err(TransportError::Cancelled); }
                            let file = OpenOptions::new().read(true).open(&staged).map_err(TransportError::from)?;
                            let mut store = self.store.lock().map_err(|_| TransportError::State(SyncError::Security("sync store lock poisoned".into())))?;
                            if self.cancel.load(AtomicOrdering::Acquire) { return Err(TransportError::Cancelled); }
                            store.import_network_reader_strict(&metadata.envelope, &metadata.signature, file, metadata.gzip_len, now_ms()).map_err(TransportError::State)
                        })();
                        let cleanup = fs::remove_file(&staged).map_err(TransportError::from);
                        if let Err(error) = cleanup { return Err(error); }
                        match result {
                            Ok(receipt) => {
                                self.log_event(false, &format!(
                                    "sync: action=upload_import outcome=ok generation={} gzip_bytes={} rows={} inserted={} updated={} deduped={} total_ms={}",
                                    receipt.generation, metadata.gzip_len, receipt.rows, receipt.inserted,
                                    receipt.updated, receipt.deduped, import_started.elapsed().as_millis(),
                                ));
                                (200, serde_json::to_vec(&receipt).map_err(|e| TransportError::Protocol(e.to_string()))?)
                            }
                            Err(TransportError::State(error)) => {
                                let code = sync_error_code(&error);
                                let detail = match &error {
                                    SyncError::Sqlite(message) => format!(" sqlite={message}"),
                                    _ => String::new(),
                                };
                                self.log_event(true, &format!(
                                    "sync: action=upload_import outcome=error error_code={code}{detail} gzip_bytes={} total_ms={}",
                                    metadata.gzip_len, import_started.elapsed().as_millis(),
                                ));
                                (status_for_error(&error), serde_json::to_vec(&WireError { message: safe_error_message(&error) }).unwrap_or_else(|_| b"{\"message\":\"request rejected\"}".to_vec()))
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
        } else if (head.path == PAIRING_PATH || head.path == PAIRING_INVITE_PATH) && !self.pairing_limiter.allow(peer.ip()) {
            (429, serde_json::to_vec(&WireError { message: "pairing rate limit exceeded; retry later".into() }).unwrap_or_else(|_| b"{\"message\":\"pairing rate limit exceeded\"}".to_vec()))
        } else if head.path == DIAGNOSTICS_MCP_PATH {
            let body = read_body(&mut tls, head.content_length, deadline, &self.cancel)?;
            let store = self.store.lock().map_err(|_| TransportError::State(SyncError::Security("sync store lock poisoned".into())))?;
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
            let mut store = self.store.lock().map_err(|_| TransportError::State(SyncError::Security("sync store lock poisoned".into())))?;
            match issue_pairing_invite(&mut store, request, &self.config.certificate_der) {
                Ok(body) => (200, body),
                Err(error) => (status_for_error(&error), serde_json::to_vec(&WireError { message: safe_error_message(&error) }).unwrap_or_else(|_| b"{\"message\":\"request rejected\"}".to_vec())),
            }
        } else {
            let body = read_body(&mut tls, head.content_length, deadline, &self.cancel)?;
            let request = HttpRequest { path: head.path, content_type: head.content_type, body };
            let mut store = self.store.lock().map_err(|_| TransportError::State(SyncError::Security("sync store lock poisoned".into())))?;
            dispatch_request(&mut store, &request)
        };
        write_response(&mut tls, status, &response, self.config.max_body_bytes)?;
        tls.conn.send_close_notify();
        let _ = tls.flush();
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

#[derive(Debug)]
struct HttpRequestHead {
    path: String,
    content_type: String,
    content_length: usize,
    known_path: Option<()>,
    expect_continue: bool,
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
        if target.len() > self.remaining { return Err(TransportError::Protocol("HTTP body ended inside upload metadata".into())); }
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

    fn read_chunk(&mut self, target: &mut [u8]) -> Result<usize, TransportError> {
        if self.remaining == 0 { return Ok(0); }
        if self.cancel.load(AtomicOrdering::Acquire) { return Err(TransportError::Cancelled); }
        if Instant::now() >= self.deadline { return Err(TransportError::Timeout); }
        let size = target.len().min(self.remaining);
        let n = self.inner.read(&mut target[..size]).map_err(TransportError::from)?;
        if n == 0 { return Err(TransportError::Protocol("unexpected end of HTTP body".into())); }
        self.remaining -= n;
        Ok(n)
    }
}

struct UploadMetadata {
    envelope: UploadEnvelope,
    signature: [u8; 64],
    gzip_len: usize,
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

struct HttpResponseHead {
    status: u16,
    content_length: Option<usize>,
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

fn encode_upload_prefix(upload: &SignedUpload) -> Result<Vec<u8>, TransportError> {
    let envelope = &upload.envelope;
    for (label, value) in [
        ("protocol_id", envelope.protocol_id.as_bytes()),
        ("primary_id", envelope.primary_id.as_bytes()),
        ("device_id", envelope.device_id.as_bytes()),
        ("origin", envelope.origin.as_bytes()),
        ("stream_id", envelope.stream_id.as_bytes()),
    ] {
        if value.is_empty() || value.len() > MAX_STRING_BYTES {
            return Err(TransportError::Limit);
        }
        let _ = label;
    }
    if envelope.manifest_bytes.is_empty() || envelope.manifest_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(TransportError::Limit);
    }
    if upload.gzip_bytes.len() > MAX_COMPRESSED_BYTES {
        return Err(TransportError::Limit);
    }
    let mut body = Vec::with_capacity(256 + envelope.manifest_bytes.len());
    body.extend_from_slice(UPLOAD_MAGIC);
    body.push(UPLOAD_WIRE_VERSION);
    for value in [
        envelope.protocol_id.as_bytes(), envelope.primary_id.as_bytes(), envelope.device_id.as_bytes(),
        envelope.origin.as_bytes(), envelope.stream_id.as_bytes(), &envelope.manifest_bytes,
    ] {
        let len = u32::try_from(value.len()).map_err(|_| TransportError::Limit)?;
        body.extend_from_slice(&len.to_be_bytes());
        body.extend_from_slice(value);
    }
    body.extend_from_slice(&envelope.data_epoch.to_be_bytes());
    body.extend_from_slice(&envelope.generation.to_be_bytes());
    body.extend_from_slice(&envelope.gzip_sha256);
    body.extend_from_slice(&upload.signature);
    body.extend_from_slice(&(upload.gzip_bytes.len() as u64).to_be_bytes());
    Ok(body)
}

fn encode_upload(upload: &SignedUpload) -> Result<Vec<u8>, TransportError> {
    let mut body = encode_upload_prefix(upload)?;
    body.extend_from_slice(&upload.gzip_bytes);
    if body.len() > DEFAULT_BODY_BYTES {
        return Err(TransportError::Limit);
    }
    Ok(body)
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
}

impl WirePairingInvite {
    fn from_invite(invite: &PairingInvite, certificate_der: &[u8]) -> Self {
        let b64 = base64::engine::general_purpose::STANDARD;
        Self {
            invite_id: invite.invite_id.clone(), endpoint: invite.endpoint.clone(),
            certificate_pin: invite.certificate_pin.clone(), expires_at_ms: invite.expires_at_ms,
            secret_b64: b64.encode(&invite.secret), certificate_der_b64: b64.encode(certificate_der),
        }
    }

    fn into_invite(self) -> Result<(PairingInvite, Vec<u8>), TransportError> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let secret = b64.decode(self.secret_b64).map_err(|e| TransportError::Protocol(format!("pairing invite secret encoding: {e}")))?;
        let certificate_der = b64.decode(self.certificate_der_b64).map_err(|e| TransportError::Protocol(format!("pairing certificate encoding: {e}")))?;
        let pin = parse_pairing_pin(&self.certificate_pin)?;
        if secret.len() != 32 || certificate_der.is_empty() || hash_bytes(&certificate_der) != pin {
            return Err(TransportError::Protocol("discovery pairing response failed certificate or secret validation".into()));
        }
        crate::protocol::valid_text("invite_id", &self.invite_id, false).map_err(|e| TransportError::Protocol(e.to_string()))?;
        crate::protocol::valid_text("endpoint", &self.endpoint, false).map_err(|e| TransportError::Protocol(e.to_string()))?;
        parse_endpoint(&self.endpoint)?;
        crate::protocol::validate_certificate_pin(&self.certificate_pin).map_err(|e| TransportError::Protocol(e.to_string()))?;
        Ok((PairingInvite { invite_id: self.invite_id, endpoint: self.endpoint, certificate_pin: self.certificate_pin, expires_at_ms: self.expires_at_ms, secret }, certificate_der))
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
    serde_json::to_vec(&WirePairingInvite::from_invite(&invite, certificate_der)).map_err(|e| SyncError::Protocol(format!("encode discovery pairing response: {e}")))
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn dispatch_request(store: &mut SyncStore, request: &HttpRequest) -> (u16, Vec<u8>) {
    let result: Result<Vec<u8>, SyncError> = match (request.path.as_str(), request.content_type.as_str()) {
        (PAIRING_PATH, "application/json") => serde_json::from_slice::<WirePairingRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid pairing JSON: {e}")))
            .and_then(|wire| wire.into_request().map_err(|e| SyncError::Protocol(e.to_string())))
            .and_then(|pairing| store.receive_pairing_request(&pairing, now_ms()).map(|state| serde_json::to_vec(&state).map_err(|e| SyncError::Protocol(e.to_string())).unwrap_or_default())),
        (STATUS_PATH, "application/json") => serde_json::from_slice::<AuthenticatedStatusRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid status request: {e}")))
            .and_then(|status| store.authenticated_status(&status, now_ms()).and_then(|value| serde_json::to_vec(&value).map_err(|e| SyncError::Protocol(e.to_string())))),
        (RECOVERY_PATH, "application/json") => serde_json::from_slice::<AuthenticatedStatusRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid recovery request: {e}")))
            .and_then(|recovery| store.recover_stream(&recovery, now_ms()).and_then(|value| serde_json::to_vec(&value).map_err(|e| SyncError::Protocol(e.to_string())))),
        (DISCONNECT_PATH, "application/json") => serde_json::from_slice::<AuthenticatedStatusRequest>(&request.body)
            .map_err(|e| SyncError::Protocol(format!("invalid disconnect request: {e}")))
            .and_then(|disconnect| store.disconnect_device(&disconnect, now_ms()).and_then(|_| serde_json::to_vec(&serde_json::json!({})).map_err(|e| SyncError::Protocol(e.to_string())))),
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
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if hash_bytes(end_entity.as_ref()) == self.expected { Ok(ServerCertVerified::assertion()) }
        else { Err(rustls::Error::General("server certificate pin mismatch".into())) }
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
    loop {
        if cancel.load(AtomicOrdering::Acquire) { return Err(TransportError::Cancelled); }
        let remaining = deadline.checked_duration_since(Instant::now()).ok_or(TransportError::Timeout)?;
        let attempt_timeout = remaining.min(CONNECT_POLL_SLICE);
        match TcpStream::connect_timeout(addr, attempt_timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => continue,
            Err(error) => {
                return if cancel.load(AtomicOrdering::Acquire) { Err(TransportError::Cancelled) } else { Err(TransportError::from(error)) };
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
    if method != "POST" || request.version != Some(1) || path.is_empty() {
        return Err(TransportError::Protocol("only HTTP/1.1 POST is supported".into()));
    }
    let known_path = match path {
        UPLOAD_PATH | PAIRING_PATH | PAIRING_INVITE_PATH | STATUS_PATH | RECOVERY_PATH | DISCONNECT_PATH | DIAGNOSTICS_MCP_PATH => Some(()),
        _ => None,
    };
    let mut content_length = None;
    let mut transfer_encoding = false;
    let mut content_type = None;
    let mut host = None;
    let mut expect_continue = false;
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
        } else if name.eq_ignore_ascii_case("expect") {
            if expect_continue || !value.trim().eq_ignore_ascii_case("100-continue") {
                return Err(TransportError::Protocol("unsupported Expect header".into()));
            }
            expect_continue = true;
        }
    }
    if transfer_encoding || host.is_none() {
        return Err(TransportError::Protocol("chunked transfer or missing host is not supported".into()));
    }
    let length = content_length.unwrap_or(0);
    let content_type = content_type.unwrap_or_default().split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    if known_path.is_some() {
        if content_length.is_none() { return Err(TransportError::Protocol("known endpoint lacks content-length".into())); }
        let limit = if path == UPLOAD_PATH {
            max_body
        } else if path == DIAGNOSTICS_MCP_PATH {
            max_body.min(MAX_DIAGNOSTICS_BODY_BYTES)
        } else {
            MAX_CONTROL_BODY_BYTES
        };
        if length > limit { return Err(TransportError::Limit); }
        let expected = if path == UPLOAD_PATH { "application/octet-stream" } else { "application/json" };
        if content_type != expected { return Err(TransportError::Protocol(format!("unexpected content-type for {path}"))); }
    }
    if Instant::now() >= deadline { return Err(TransportError::Timeout); }
    Ok(HttpRequestHead { path: path.to_string(), content_type, content_length: length, known_path, expect_continue })
}

fn read_body<S: Read>(stream: &mut S, length: usize, deadline: Instant, cancel: &AtomicBool) -> Result<Vec<u8>, TransportError> {
    let mut reader = LimitedReader::new(stream, length, deadline, cancel);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(body)
}

fn read_upload_metadata<S: Read>(stream: &mut S, body_len: usize, max_body: usize, deadline: Instant, cancel: &AtomicBool) -> Result<UploadMetadata, TransportError> {
    if body_len > max_body { return Err(TransportError::Limit); }
    let mut reader = LimitedReader::new(stream, body_len, deadline, cancel);
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != UPLOAD_MAGIC { return Err(TransportError::Protocol("unsupported upload wire magic".into())); }
    let mut version = [0u8; 1];
    reader.read_exact(&mut version)?;
    if version[0] != UPLOAD_WIRE_VERSION { return Err(TransportError::Protocol("unsupported upload wire version".into())); }
    let protocol_id = String::from_utf8(read_wire_field(&mut reader, MAX_STRING_BYTES, "protocol_id")?)
        .map_err(|_| TransportError::Protocol("protocol_id is not UTF-8".into()))?;
    let primary_id = String::from_utf8(read_wire_field(&mut reader, MAX_STRING_BYTES, "primary_id")?)
        .map_err(|_| TransportError::Protocol("primary_id is not UTF-8".into()))?;
    let device_id = String::from_utf8(read_wire_field(&mut reader, MAX_STRING_BYTES, "device_id")?)
        .map_err(|_| TransportError::Protocol("device_id is not UTF-8".into()))?;
    let origin = String::from_utf8(read_wire_field(&mut reader, MAX_STRING_BYTES, "origin")?)
        .map_err(|_| TransportError::Protocol("origin is not UTF-8".into()))?;
    let stream_id = String::from_utf8(read_wire_field(&mut reader, MAX_STRING_BYTES, "stream_id")?)
        .map_err(|_| TransportError::Protocol("stream_id is not UTF-8".into()))?;
    let manifest_bytes = read_wire_field(&mut reader, MAX_MANIFEST_BYTES, "manifest")?;
    let data_epoch = read_u64(&mut reader)?;
    let generation = read_u64(&mut reader)?;
    let mut gzip_sha256 = [0u8; 32];
    reader.read_exact(&mut gzip_sha256)?;
    let mut signature = [0u8; 64];
    reader.read_exact(&mut signature)?;
    let gzip_len = read_u64(&mut reader)?;
    let gzip_len = usize::try_from(gzip_len).map_err(|_| TransportError::Limit)?;
    if gzip_len > MAX_COMPRESSED_BYTES || gzip_len != reader.remaining { return Err(TransportError::Limit); }
    Ok(UploadMetadata { envelope: UploadEnvelope { protocol_id, primary_id, device_id, origin, stream_id, data_epoch, generation, manifest_bytes, gzip_sha256 }, signature, gzip_len })
}

fn read_wire_field<R: Read>(reader: &mut LimitedReader<'_, R>, max: usize, label: &str) -> Result<Vec<u8>, TransportError> {
    let len = usize::try_from(read_u32(reader)?).map_err(|_| TransportError::Limit)?;
    if len > max { return Err(TransportError::Limit); }
    let mut value = vec![0u8; len];
    reader.read_exact(&mut value).map_err(|error| match error {
        TransportError::Protocol(_) => TransportError::Protocol(format!("truncated upload {label}")),
        other => other,
    })?;
    Ok(value)
}

fn read_u32<R: Read>(reader: &mut LimitedReader<'_, R>) -> Result<u32, TransportError> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut LimitedReader<'_, R>) -> Result<u64, TransportError> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_be_bytes(bytes))
}

fn stage_upload<S: Read>(stream: &mut S, metadata: &UploadMetadata, directory: &Path, deadline: Instant, cancel: &AtomicBool) -> Result<PathBuf, TransportError> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|e| TransportError::Config(format!("cannot create staging name: {e}")))?;
    let name = random.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let path = directory.join(format!("upload-{name}.part"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(TransportError::from)?;
    let result = (|| {
        crate::protocol::set_private_file(&file, &path).map_err(TransportError::State)?;
        let mut reader = LimitedReader::new(stream, metadata.gzip_len, deadline, cancel);
        let mut buffer = [0u8; 64 * 1024];
        while reader.remaining > 0 {
            let n = reader.read_chunk(&mut buffer)?;
            file.write_all(&buffer[..n]).map_err(TransportError::from)?;
        }
        file.sync_all().map_err(TransportError::from)
    })();
    drop(file);
    if result.is_err() { let _ = fs::remove_file(&path); }
    result.map(|_| path)
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
    for header in response.headers.iter() {
        if header.name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() { return Err(TransportError::Protocol("duplicate response content-length header".into())); }
            let value = std::str::from_utf8(header.value).map_err(|_| TransportError::Protocol("invalid response content-length".into()))?;
            content_length = Some(value.trim().parse::<usize>().map_err(|_| TransportError::Protocol("invalid response content-length".into()))?);
        } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
            transfer_encoding = true;
        }
    }
    if transfer_encoding { return Err(TransportError::Protocol("chunked responses are not supported".into())); }
    if status == 100 && content_length.unwrap_or(0) != 0 {
        return Err(TransportError::Protocol("100 Continue response must not contain a body".into()));
    }
    Ok(HttpResponseHead { status, content_length })
}

fn read_response_body<S: Read>(stream: &mut S, head: HttpResponseHead, max_response: usize) -> Result<HttpResponse, TransportError> {
    let length = head.content_length.ok_or_else(|| TransportError::Protocol("response lacks content-length".into()))?;
    if length > max_response { return Err(TransportError::Limit); }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).map_err(TransportError::from)?;
    Ok(HttpResponse { status: head.status, body })
}

fn write_response<S: Write>(stream: &mut S, status: u16, body: &[u8], max_body: usize) -> Result<(), TransportError> {
    if body.len() > max_body { return Err(TransportError::Limit); }
    let reason = match status { 200 => "OK", 400 => "Bad Request", 401 => "Unauthorized", 403 => "Forbidden", 408 => "Request Timeout", 409 => "Conflict", 413 => "Payload Too Large", 426 => "Upgrade Required", 429 => "Too Many Requests", 503 => "Service Unavailable", _ => "Error" };
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).map_err(TransportError::from)?;
    stream.write_all(body).map_err(TransportError::from)?;
    stream.flush().map_err(TransportError::from)
}

fn write_continue<S: Write>(stream: &mut S) -> Result<(), TransportError> {
    stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").map_err(TransportError::from)?;
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
    use crate::protocol::{DeviceIdentity, NetworkCounts, NetworkPackage, NetworkRow, PairingInvite, PROTOCOL_ID, UploadEnvelope};
    use crate::{DiagnosticsHost, DiagnosticsRequestAuth, PairingRequest, SyncStore};
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
        let receipt = client.upload(&upload).unwrap();
        assert_eq!(receipt.inserted, 1);
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
        let receipt = client.upload(&entry.signed_upload()).unwrap();
        sender.ack_outbox("sender-runtime", outbox_id, &receipt, 2).unwrap();
        assert_eq!(sender.outbox_summary().unwrap().pending, 0);
        assert_eq!(sender.outbox_summary().unwrap().in_flight, 0);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn upload_wire_is_binary_and_does_not_base64_the_gzip() {
        let device = DeviceIdentity::create("linux", "Linux").unwrap();
        let (_, upload) = package(&device);
        let body = encode_upload(&upload).unwrap();
        assert!(body.starts_with(UPLOAD_MAGIC));
        assert_ne!(body.first(), Some(&b'{'));
        assert!(body.windows(upload.gzip_bytes.len()).any(|window| window == upload.gzip_bytes.as_slice()));
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
        let receipt = client.upload(&upload).unwrap();
        assert_eq!(receipt.inserted, 1);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn upload_rejection_is_returned_after_metadata_before_gzip_body() {
        let cert = cert();
        let device = DeviceIdentity::create("preflight-reject", "Linux").unwrap();
        let (_, mut upload) = package(&device);
        upload.gzip_bytes = vec![0x5a; 4 * 1024 * 1024];

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let observed_body_bytes = Arc::new(AtomicU64::new(u64::MAX));
        let observed = Arc::clone(&observed_body_bytes);
        let tls_cert = cert.cert_der.clone();
        let tls_key = cert.key_der.clone();
        let handle = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
            let config = HttpsServerConfig {
                bind_addr: addr,
                certificate_der: tls_cert,
                private_key_der: tls_key,
                allow_lan: false,
                allowed_peer_ips: vec![],
                max_body_bytes: DEFAULT_BODY_BYTES,
                max_concurrent: 1,
                staging_dir: std::env::temp_dir().join(format!("tokenme-sync-preflight-{}", now_ms())),
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
                total_timeout: DEFAULT_TOTAL_TIMEOUT,
                staging_quota_bytes: DEFAULT_STAGING_QUOTA_BYTES,
            };
            let connection = ServerConnection::new(Arc::new(super::server_config(&config).unwrap())).unwrap();
            let mut stream = StreamOwned::new(connection, tcp);
            let cancel = AtomicBool::new(false);
            let deadline = Instant::now() + Duration::from_secs(5);
            let head = read_request_head(&mut stream, DEFAULT_BODY_BYTES, deadline, &cancel).unwrap();
            assert!(head.expect_continue);
            let _metadata = read_upload_metadata(&mut stream, head.content_length, DEFAULT_BODY_BYTES, deadline, &cancel).unwrap();
            write_response(&mut stream, 403, br#"{"message":"device is not authorized"}"#, MAX_RESPONSE_BYTES).unwrap();
            let mut extra = [0u8; 8192];
            let received = match stream.read(&mut extra) {
                Ok(bytes) => bytes as u64,
                Err(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::UnexpectedEof) => 0,
                Err(error) => panic!("unexpected post-rejection read error: {error}"),
            };
            observed.store(received, AtomicOrdering::Release);
        });

        let mut client = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()),
            "localhost",
            cert.cert_der,
        )).unwrap();
        assert!(matches!(client.upload(&upload), Err(TransportError::HttpStatus { status: 403, .. })));
        handle.join().unwrap();
        assert_eq!(observed_body_bytes.load(AtomicOrdering::Acquire), 0);
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
        let join = thread::spawn(move || tx.send(client.upload(&upload)).unwrap());
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
    fn pin_mismatch_happens_before_http_request() {
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
        assert!(matches!(client.upload(&upload), Err(TransportError::PinMismatch)));
        assert!(handle.join().unwrap().is_ok());
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
        let request = AuthenticatedStatusRequest::new_at(&device, "primary-1", false, now_ms()).unwrap();
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
        let receipt = client.upload(&upload).unwrap();
        assert_eq!(receipt.inserted, 1);
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

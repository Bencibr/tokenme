//! WebSocket bidirectional sync protocol for TokenMe multi-device synchronization.
//!
//! Provides persistent long-lived full-duplex communication between Secondary
//! clients and Primary server, with heartbeat pings, symmetric rollup snapshot
//! pushes (both directions ship the same message), and automatic reconnection.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tungstenite::protocol::WebSocket;
use tungstenite::{Message, Error as WsError};

use crate::protocol::DeviceIdentity;
use crate::store::{SyncError, SyncStore};
use crate::transport::RollupSnapshot;

pub const WS_PROTOCOL_VERSION: &str = "tokenme-ws-v2";
pub const DEFAULT_WS_PING_INTERVAL: Duration = Duration::from_secs(15);
pub const DEFAULT_WS_RECONNECT_MIN: Duration = Duration::from_secs(1);
pub const DEFAULT_WS_RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Messages exchanged across the WebSocket sync channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsSyncFrame {
    /// Initial client authentication and identity announcement.
    Auth {
        device_id: String,
        origin: String,
        display_name: String,
        version: String,
    },
    /// Server authentication response.
    AuthAck {
        status: String,
        primary_id: String,
        primary_display_name: Option<String>,
        epoch: i64,
        error: Option<String>,
    },
    /// Periodic heartbeat ping.
    Ping {
        client_time_ms: i64,
    },
    /// Heartbeat pong reply.
    Pong {
        server_time_ms: i64,
    },
    /// Secondary pushing its full local-origin rollup snapshot. Both sync
    /// directions ship the same `RollupSnapshot` message; the primary applies
    /// the rows under the authenticated device's registry origin, so the
    /// snapshot's own `origin` label is advisory only.
    RollupSync {
        snapshot: RollupSnapshot,
    },
    /// Server acknowledging an applied uplink rollup. `origin` is the
    /// registry-resolved label the rows were stored under, `n` the applied
    /// row count. Failures travel as the standard `Error` frame instead.
    RollupAck {
        origin: String,
        accepted: bool,
        n: usize,
    },
    /// Client query for primary status.
    StatusRequest {
        device_id: String,
    },
    /// Server status response.
    StatusResponse {
        primary_id: String,
        status: String,
        active_stream_id: Option<String>,
        expected_generation: Option<u64>,
        server_time_ms: i64,
    },
    /// Explicit error message.
    Error {
        message: String,
    },
}

/// Server-side WebSocket connection handler.
/// Runs the persistent message loop for a connected secondary client until the socket closes.
/// `on_live` fires the moment a device authenticates; `on_dead` fires exactly
/// once when its authenticated socket dies — reset, timeout, protocol error
/// or clean close — via a drop guard, so every exit path reports.
pub fn handle_server_websocket<S>(
    mut ws: WebSocket<S>,
    store: Arc<std::sync::Mutex<SyncStore>>,
    now_ms: impl Fn() -> i64,
    stop: &AtomicBool,
    on_live: &dyn Fn(&str),
    on_dead: &dyn Fn(&str),
) -> Result<(), SyncError>
where
    S: Read + Write,
{
    struct SessionGuard<'a> {
        device: Option<String>,
        on_dead: &'a dyn Fn(&str),
    }
    impl Drop for SessionGuard<'_> {
        fn drop(&mut self) {
            if let Some(device) = &self.device {
                (self.on_dead)(device);
            }
        }
    }
    let mut session = SessionGuard { device: None, on_dead };
    let mut authenticated_device: Option<String> = None;

    while !stop.load(Ordering::Acquire) {
        let msg = match ws.read() {
            Ok(msg) => msg,
            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => break,
            Err(WsError::Io(err)) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(_) => break,
        };

        match msg {
            Message::Text(text) => {
                let frame: WsSyncFrame = match serde_json::from_str(&text) {
                    Ok(f) => f,
                    Err(e) => {
                        let err_frame = WsSyncFrame::Error { message: format!("invalid frame JSON: {e}") };
                        let _ = ws.send(Message::Text(serde_json::to_string(&err_frame).unwrap_or_default()));
                        continue;
                    }
                };

                let response = {
                    let mut s = store.lock().unwrap_or_else(|p| p.into_inner());
                    process_server_frame(&frame, &mut s, &mut authenticated_device, &now_ms)?
                };
                if let Some(device) = response.1 {
                    session.device = Some(device.clone());
                    on_live(&device);
                }
                if let Some(resp) = response.0 {
                    let resp_text = serde_json::to_string(&resp).map_err(|e| SyncError::Protocol(e.to_string()))?;
                    ws.send(Message::Text(resp_text)).map_err(|e| SyncError::Protocol(format!("ws send: {e}")))?;
                }
            }
            Message::Binary(bytes) => {
                let frame: WsSyncFrame = match serde_json::from_slice(&bytes) {
                    Ok(f) => f,
                    Err(e) => {
                        let err_frame = WsSyncFrame::Error { message: format!("invalid binary frame: {e}") };
                        let _ = ws.send(Message::Text(serde_json::to_string(&err_frame).unwrap_or_default()));
                        continue;
                    }
                };
                let response = {
                    let mut s = store.lock().unwrap_or_else(|p| p.into_inner());
                    process_server_frame(&frame, &mut s, &mut authenticated_device, &now_ms)?
                };
                if let Some(device) = response.1 {
                    session.device = Some(device.clone());
                    on_live(&device);
                }
                if let Some(resp) = response.0 {
                    let resp_text = serde_json::to_string(&resp).map_err(|e| SyncError::Protocol(e.to_string()))?;
                    ws.send(Message::Text(resp_text)).map_err(|e| SyncError::Protocol(format!("ws send: {e}")))?;
                }
            }
            Message::Ping(payload) => {
                let _ = ws.send(Message::Pong(payload));
            }
            Message::Pong(_) => {}
            Message::Close(_) => {
                let _ = ws.close(None);
                break;
            }
            Message::Frame(_) => {}
        }
    }

    Ok(())
}

/// Returns the reply frame plus the device id when this frame authenticated
/// a session — the caller owns the live-session callbacks.
fn process_server_frame(
    frame: &WsSyncFrame,
    store: &mut SyncStore,
    authenticated_device: &mut Option<String>,
    now_ms: &impl Fn() -> i64,
) -> Result<(Option<WsSyncFrame>, Option<String>), SyncError> {
    match frame {
        WsSyncFrame::Auth { device_id, origin: _, display_name: _, version: _ } => {
            let status = match store.ws_device_status(device_id)? {
                Some(status) => status,
                None => "unpaired".to_string(),
            };

            let primary_id = store.primary_id().to_string();
            let epoch = store.ws_device_epoch(device_id)?.unwrap_or(0);

            if status == "paired" {
                *authenticated_device = Some(device_id.clone());
                Ok((Some(WsSyncFrame::AuthAck {
                    status: "connected".into(),
                    primary_id,
                    primary_display_name: None,
                    epoch,
                    error: None,
                }), Some(device_id.clone())))
            } else {
                Ok((Some(WsSyncFrame::AuthAck {
                    status,
                    primary_id,
                    primary_display_name: None,
                    epoch,
                    error: Some("device is not in paired state".into()),
                }), None))
            }
        }
        WsSyncFrame::Ping { client_time_ms: _ } => {
            Ok((Some(WsSyncFrame::Pong { server_time_ms: now_ms() }), None))
        }
        WsSyncFrame::Pong { .. } => Ok((None, None)),
        WsSyncFrame::RollupSync { snapshot } => {
            let Some(device_id) = authenticated_device.as_deref() else {
                return Ok((Some(WsSyncFrame::Error {
                    message: "unauthenticated connection; send Auth first".into(),
                }), None));
            };
            // The session's Auth frame — not the frame payload — establishes
            // the sender's identity, so the rows land under the registry
            // origin of the paired device and `apply_uplink_rollup` returns
            // that resolved label for the ack. A session that authenticated
            // before a disconnect or revocation must not keep uploading:
            // re-check the registry state per apply, like the package
            // import did.
            match store.ws_device_status(device_id) {
                Ok(Some(status)) if status == "paired" => {}
                Ok(_) => return Ok((Some(WsSyncFrame::Error {
                    message: "device is not authorized to upload".into(),
                }), None)),
                Err(err) => return Ok((Some(WsSyncFrame::Error { message: err.to_string() }), None)),
            }
            match store.apply_uplink_rollup(device_id, &snapshot.rows, now_ms()) {
                Ok(origin) => Ok((Some(WsSyncFrame::RollupAck {
                    origin,
                    accepted: true,
                    n: snapshot.rows.len(),
                }), None)),
                Err(err) => Ok((Some(WsSyncFrame::Error { message: err.to_string() }), None)),
            }
        }
        WsSyncFrame::StatusRequest { device_id } => {
            let status = store.ws_device_status(device_id)?.unwrap_or_else(|| "unpaired".into());
            Ok((Some(WsSyncFrame::StatusResponse {
                primary_id: store.primary_id().to_string(),
                status,
                active_stream_id: None,
                expected_generation: None,
                server_time_ms: now_ms(),
            }), None))
        }
        WsSyncFrame::RollupAck { .. } | WsSyncFrame::AuthAck { .. } | WsSyncFrame::StatusResponse { .. } | WsSyncFrame::Error { .. } => {
            Ok((None, None))
        }
    }
}

/// Client-side WebSocket connection controller.
/// Maintains an auto-reconnecting long-lived session with exponential backoff.
pub struct WsSecondaryClient {
    pub identity: DeviceIdentity,
    pub primary_endpoint: String,
    pub stop: Arc<AtomicBool>,
}

impl WsSecondaryClient {
    pub fn new(identity: DeviceIdentity, primary_endpoint: String, stop: Arc<AtomicBool>) -> Self {
        Self { identity, primary_endpoint, stop }
    }

    /// Converts an http/https endpoint string to a ws/wss endpoint.
    pub fn websocket_url(&self) -> Result<String, String> {
        let endpoint = self.primary_endpoint.trim();
        if endpoint.starts_with("https://") {
            Ok(format!("wss://{}/v2/sync-ws", &endpoint["https://".len()..]))
        } else if endpoint.starts_with("http://") {
            Ok(format!("ws://{}/v2/sync-ws", &endpoint["http://".len()..]))
        } else if endpoint.starts_with("wss://") || endpoint.starts_with("ws://") {
            Ok(format!("{endpoint}/v2/sync-ws"))
        } else {
            Ok(format!("ws://{endpoint}/v2/sync-ws"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PairingRequest;

    #[test]
    fn websocket_url_formatting() {
        let identity = DeviceIdentity::create("dev-test", "test").unwrap();
        let stop = Arc::new(AtomicBool::new(false));

        let client1 = WsSecondaryClient::new(identity.clone(), "http://192.168.1.10:47821".into(), stop.clone());
        assert_eq!(client1.websocket_url().unwrap(), "ws://192.168.1.10:47821/v2/sync-ws");

        let client2 = WsSecondaryClient::new(identity, "https://primary.local:47821".into(), stop);
        assert_eq!(client2.websocket_url().unwrap(), "wss://primary.local:47821/v2/sync-ws");
    }

    #[test]
    fn frame_serialization_roundtrip() {
        let ping = WsSyncFrame::Ping { client_time_ms: 123456789 };
        let json = serde_json::to_string(&ping).unwrap();
        assert!(json.contains("\"type\":\"ping\""));
        assert!(json.contains("123456789"));

        let deserialized: WsSyncFrame = serde_json::from_str(&json).unwrap();
        match deserialized {
            WsSyncFrame::Ping { client_time_ms } => assert_eq!(client_time_ms, 123456789),
            _ => panic!("wrong frame"),
        }
    }

    fn rollup_row(day: &str, tool: &str, n: i64) -> crate::transport::RollupRow {
        crate::transport::RollupRow {
            day: day.into(), origin: String::new(), tool: tool.into(), session: "session".into(),
            project: "project-tokenme".into(), model: "gpt-5".into(), meter: "tokens".into(),
            in_tok: 100.0, cc_tok: 0.0, cr_tok: 0.0, out_tok: 50.0, reason_tok: 0.0, credits: 0.0,
            n, nonzero: 1, min_ts: 1_000, max_ts: 2_000,
        }
    }

    fn paired_store() -> (SyncStore, DeviceIdentity) {
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        let device = DeviceIdentity::create("rollup-secondary", "macOS").unwrap();
        let invite = store.create_pairing_invite("https://localhost:47821", &"7".repeat(64), 100_000).unwrap();
        let pairing = PairingRequest::new(&invite, &device, "test-os").unwrap();
        let state = store.receive_pairing_request(&pairing, 1).unwrap();
        store.approve_pairing(&state.request_id, 2).unwrap();
        (store, device)
    }

    #[test]
    fn rollup_frames_carry_the_snapshot_shape() {
        let frame = WsSyncFrame::RollupSync {
            snapshot: RollupSnapshot { origin: "lab-primary".into(), rows: vec![rollup_row("2026-10-22", "codex", 3)] },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"rollup_sync\""), "{json}");
        assert!(json.contains("\"origin\":\"lab-primary\""), "{json}");
        let parsed: WsSyncFrame = serde_json::from_str(&json).unwrap();
        match parsed {
            WsSyncFrame::RollupSync { snapshot } => {
                assert_eq!(snapshot.origin, "lab-primary");
                assert_eq!(snapshot.rows.len(), 1);
                assert_eq!(snapshot.rows[0].tool, "codex");
            }
            other => panic!("wrong frame: {other:?}"),
        }
        let ack_json = serde_json::to_string(&WsSyncFrame::RollupAck { origin: "lab-primary".into(), accepted: true, n: 2 }).unwrap();
        assert!(ack_json.contains("\"type\":\"rollup_ack\""), "{ack_json}");
        assert!(ack_json.contains("\"accepted\":true"), "{ack_json}");
        assert!(ack_json.contains("\"n\":2"), "{ack_json}");
    }

    #[test]
    fn stale_sync_push_frame_is_unknown_wire() {
        let stale = r#"{"type":"sync_push","batch_id":"b","envelope":null,"signature":[],"gzip_bytes":""}"#;
        assert!(serde_json::from_str::<WsSyncFrame>(stale).is_err(), "the retired package frame must no longer parse");
    }

    #[test]
    fn rollup_sync_without_auth_is_rejected_with_the_wire_error_shape() {
        let (mut store, device) = paired_store();
        let frame = WsSyncFrame::RollupSync {
            snapshot: RollupSnapshot { origin: device.origin.clone(), rows: vec![rollup_row("2026-10-22", "codex", 1)] },
        };
        let mut authenticated = None;
        let (reply, session) = process_server_frame(&frame, &mut store, &mut authenticated, &|| 1_000).unwrap();
        assert!(session.is_none());
        match reply {
            Some(WsSyncFrame::Error { message }) => assert!(message.contains("unauthenticated"), "{message}"),
            other => panic!("expected the wire error frame, got {other:?}"),
        }
    }

    #[test]
    fn authenticated_rollup_sync_applies_rows_under_the_registry_origin() {
        let (mut store, device) = paired_store();
        let rows = vec![rollup_row("2026-10-22", "codex", 3), rollup_row("2026-10-23", "claude-code", 5)];
        let frame = WsSyncFrame::RollupSync {
            snapshot: RollupSnapshot { origin: device.origin.clone(), rows },
        };
        let mut authenticated = Some(device.device_id.clone());
        let (reply, session) = process_server_frame(&frame, &mut store, &mut authenticated, &|| 1_000).unwrap();
        assert!(session.is_none(), "a rollup push does not re-authenticate the session");
        match reply {
            Some(WsSyncFrame::RollupAck { origin, accepted, n }) => {
                assert_eq!(origin, device.origin, "the ack must resolve the device's registry origin");
                assert!(accepted);
                assert_eq!(n, 2);
            }
            other => panic!("expected rollup ack, got {other:?}"),
        }
        let stored: i64 = store.index().conn().query_row(
            "SELECT count(*) FROM remote_rollup WHERE origin = ?1",
            [&device.origin], |row| row.get(0),
        ).unwrap();
        assert_eq!(stored, 2, "both rows must land under the registry origin");
    }
}

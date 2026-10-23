//! WebSocket bidirectional sync protocol for TokenMe multi-device synchronization.
//!
//! Provides persistent long-lived full-duplex communication between Secondary
//! clients and Primary server, with heartbeat pings, instantaneous delta pushes,
//! idempotent receipts, and automatic reconnection.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tungstenite::protocol::WebSocket;
use tungstenite::{Message, Error as WsError};

use crate::protocol::{DeviceIdentity, UploadEnvelope};
use crate::store::{SyncError, SyncStore};

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
    /// Client pushing a delta batch of usage events.
    SyncPush {
        batch_id: String,
        envelope: UploadEnvelope,
        signature: Vec<u8>,
        #[serde(with = "base64_serde")]
        gzip_bytes: Vec<u8>,
    },
    /// Server acknowledging a synced delta batch.
    SyncAck {
        batch_id: String,
        success: bool,
        receipt: Option<crate::store::ImportReceipt>,
        error: Option<String>,
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

mod base64_serde {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        BASE64.decode(s).map_err(serde::de::Error::custom)
    }
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
        WsSyncFrame::SyncPush { batch_id, envelope, signature, gzip_bytes } => {
            let Some(device_id) = authenticated_device.as_deref() else {
                return Ok((Some(WsSyncFrame::SyncAck {
                    batch_id: batch_id.clone(),
                    success: false,
                    receipt: None,
                    error: Some("unauthenticated connection; send Auth first".into()),
                }), None));
            };

            if envelope.device_id != device_id {
                return Ok((Some(WsSyncFrame::SyncAck {
                    batch_id: batch_id.clone(),
                    success: false,
                    receipt: None,
                    error: Some("envelope device_id does not match authenticated socket".into()),
                }), None));
            }

            let Ok(sig): Result<&[u8; 64], _> = signature.as_slice().try_into() else {
                return Ok((Some(WsSyncFrame::SyncAck {
                    batch_id: batch_id.clone(),
                    success: false,
                    receipt: None,
                    error: Some("invalid signature length, expected 64 bytes".into()),
                }), None));
            };

            let now = now_ms();
            match store.import_network_reader_strict(
                envelope,
                sig,
                std::io::Cursor::new(gzip_bytes),
                gzip_bytes.len(),
                now,
            ) {
                Ok(receipt) => Ok((Some(WsSyncFrame::SyncAck {
                    batch_id: batch_id.clone(),
                    success: true,
                    receipt: Some(receipt),
                    error: None,
                }), None)),
                Err(err) => Ok((Some(WsSyncFrame::SyncAck {
                    batch_id: batch_id.clone(),
                    success: false,
                    receipt: None,
                    error: Some(err.to_string()),
                }), None)),
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
        WsSyncFrame::SyncAck { .. } | WsSyncFrame::AuthAck { .. } | WsSyncFrame::StatusResponse { .. } | WsSyncFrame::Error { .. } => {
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
}

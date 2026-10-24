//! WebSocket bidirectional sync protocol for TokenMe multi-device synchronization.
//!
//! Provides persistent long-lived full-duplex communication between Secondary
//! clients and Primary server, with heartbeat pings, symmetric rollup snapshot
//! pushes (both directions ship the same message), and automatic reconnection.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tungstenite::protocol::WebSocket;
use tungstenite::{Message, Error as WsError};

use crate::store::{DeviceStatus, SyncError, SyncResult, SyncStore};
use crate::transport::{primary_local_rollup_snapshot, RollupSnapshot};

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
    /// Secondary asking for the primary's own local-origin rollup snapshot —
    /// the session leg of what POST /v1/rollup served. The authenticated
    /// session replaces the signed request; the reply is `RollupSnapshot`.
    RollupFetch {},
    /// Server reply to `RollupFetch`: the primary's own rollup rows under the
    /// label the secondary groups them by (the primary's hostname).
    RollupSnapshot {
        snapshot: RollupSnapshot,
    },
    /// Secondary refreshing its display name. The session's Auth frame proves
    /// the sender is the paired device, so the frame carries just the name.
    DisplayNameUpdate {
        name: String,
    },
    /// Server acknowledging an applied display-name update. Failures travel
    /// as the standard `Error` frame instead.
    DisplayNameAck {},
    /// Secondary announcing a clean disconnect ("I am leaving"). The
    /// session's Auth frame proves the sender owns the pairing, so the frame
    /// carries nothing — the primary marks the intended leave instantly, no
    /// notice queue needed for the clean case.
    DisconnectNotify {},
    /// Server acknowledging a disconnect notice. Failures travel as the
    /// standard `Error` frame instead.
    DisconnectAck {},
    /// Server-initiated, one-way advisory: this device's registry state moved
    /// out-of-band on the primary (revoke, pause, resume) or the primary's
    /// data-sharing flag flipped. Best-effort — no live socket, no delivery,
    /// and the device's next session re-syncs regardless.
    DeviceState {
        status: String,
        data_sharing: bool,
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
/// `on_disconnect` fires when an authenticated device lands a clean
/// `DisconnectNotify`. `push_rx` carries the server-initiated frames
/// (`DeviceState`) the live-socket registry queued for this connection; the
/// loop drains it between reads, so a push waits at most one poll slice.
pub fn handle_server_websocket<S>(
    mut ws: WebSocket<S>,
    store: Arc<std::sync::Mutex<SyncStore>>,
    now_ms: impl Fn() -> i64,
    stop: &AtomicBool,
    on_live: &dyn Fn(&str),
    on_dead: &dyn Fn(&str),
    on_disconnect: &dyn Fn(&str),
    push_rx: &mpsc::Receiver<WsSyncFrame>,
) -> Result<(), SyncError>
where
    S: Read + Write,
{
    fn drain_pushes<S: Read + Write>(ws: &mut WebSocket<S>, push_rx: &mpsc::Receiver<WsSyncFrame>) {
        loop {
            match push_rx.try_recv() {
                Ok(frame) => {
                    if let Ok(text) = serde_json::to_string(&frame) {
                        // Best-effort by contract: a WouldBlock here leaves the
                        // frame queued in tungstenite's write cache, and a dead
                        // socket is the next read's problem.
                        let _ = ws.send(Message::Text(text));
                    }
                }
                Err(_) => break,
            }
        }
    }
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
            Err(WsError::Io(err)) if err.kind() == std::io::ErrorKind::WouldBlock
                || err.kind() == std::io::ErrorKind::TimedOut => {
                // WouldBlock is the unix shape of an idle poll; Windows names
                // the same SO_RCVTIMEO expiry TimedOut. Both mean "nothing to
                // read": drain the push queue and keep the session alive.
                drain_pushes(&mut ws, push_rx);
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
                    process_server_frame(&frame, &mut s, &mut authenticated_device, &now_ms, on_disconnect)?
                };
                if let Some(device) = response.1 {
                    session.device = Some(device.clone());
                    on_live(&device);
                }
                if let Some(resp) = response.0 {
                    let resp_text = serde_json::to_string(&resp).map_err(|e| SyncError::Protocol(e.to_string()))?;
                    ws.send(Message::Text(resp_text)).map_err(|e| SyncError::Protocol(format!("ws send: {e}")))?;
                }
                drain_pushes(&mut ws, push_rx);
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
                    process_server_frame(&frame, &mut s, &mut authenticated_device, &now_ms, on_disconnect)?
                };
                if let Some(device) = response.1 {
                    session.device = Some(device.clone());
                    on_live(&device);
                }
                if let Some(resp) = response.0 {
                    let resp_text = serde_json::to_string(&resp).map_err(|e| SyncError::Protocol(e.to_string()))?;
                    ws.send(Message::Text(resp_text)).map_err(|e| SyncError::Protocol(format!("ws send: {e}")))?;
                }
                drain_pushes(&mut ws, push_rx);
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
/// a session — the caller owns the live-session callbacks. `on_disconnect`
/// fires only after a disconnect notice the store accepted.
fn process_server_frame(
    frame: &WsSyncFrame,
    store: &mut SyncStore,
    authenticated_device: &mut Option<String>,
    now_ms: &impl Fn() -> i64,
    on_disconnect: &dyn Fn(&str),
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
                // The registry status word rides in the error text — the
                // secondary's only carrier for it — so a refused session maps
                // back onto the status poll's taxonomy (paused is
                // recoverable, revoked is not) instead of reading as a
                // generic refusal.
                Ok((Some(WsSyncFrame::AuthAck {
                    status: status.clone(),
                    primary_id,
                    primary_display_name: None,
                    epoch,
                    error: Some(format!("device is not in paired state: registry status is {status}")),
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
                // The registry status word must ride the refusal: the
                // secondary's taxonomy maps "paused" onto the recoverable
                // arm, while a generic "not authorized" reads as revoked.
                Ok(Some(status)) => return Ok((Some(WsSyncFrame::Error {
                    message: format!("device is not authorized to upload: registry status is {status}"),
                }), None)),
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
        WsSyncFrame::RollupFetch {} => {
            let Some(device_id) = authenticated_device.as_deref() else {
                return Ok((Some(WsSyncFrame::Error {
                    message: "unauthenticated connection; send Auth first".into(),
                }), None));
            };
            // The downlink serves the primary's own rows only to a device that
            // is still paired — the same gate the retired POST /v1/rollup
            // applied after its signature checks. The registry status word
            // rides in the message: the secondary maps refused frames back
            // onto the status poll's taxonomy (paused is recoverable, revoked
            // is not).
            match store.ws_device_status(device_id) {
                Ok(Some(status)) if status == "paired" => {}
                Ok(Some(status)) => return Ok((Some(WsSyncFrame::Error {
                    message: format!("device is not authorized to fetch the primary rollup: registry status is {status}"),
                }), None)),
                Ok(None) => return Ok((Some(WsSyncFrame::Error {
                    message: "device is not in paired state".into(),
                }), None)),
                Err(err) => return Ok((Some(WsSyncFrame::Error { message: err.to_string() }), None)),
            }
            // Data sharing is the primary's answer, not the secondary's guess:
            // a configured-off primary refuses with the keyword the secondary
            // learns from, and the device stops asking until a DeviceState
            // push (or a restart) re-opens the question.
            let sharing = match store.primary_config() {
                Ok(config) => config.is_some_and(|config| config.share_data_with_secondaries),
                Err(err) => return Ok((Some(WsSyncFrame::Error { message: err.to_string() }), None)),
            };
            if !sharing {
                return Ok((Some(WsSyncFrame::Error {
                    message: "the primary is not sharing data with secondaries".into(),
                }), None));
            }
            match primary_local_rollup_snapshot(store) {
                Ok(snapshot) => Ok((Some(WsSyncFrame::RollupSnapshot { snapshot }), None)),
                Err(err) => Ok((Some(WsSyncFrame::Error { message: err.to_string() }), None)),
            }
        }
        WsSyncFrame::DisplayNameUpdate { name } => {
            let Some(device_id) = authenticated_device.as_deref() else {
                return Ok((Some(WsSyncFrame::Error {
                    message: "unauthenticated connection; send Auth first".into(),
                }), None));
            };
            // The store's `update_display_name` verifies a client-signed
            // request; the session's Auth frame replaces that proof, so this
            // is the same write with the session's authenticated device: the
            // same validity rule, the same paired gate, the same UPDATE.
            match session_update_display_name(store, device_id, name) {
                Ok(()) => Ok((Some(WsSyncFrame::DisplayNameAck {}), None)),
                Err(err) => Ok((Some(WsSyncFrame::Error { message: err.to_string() }), None)),
            }
        }
        WsSyncFrame::DisconnectNotify {} => {
            let Some(device_id) = authenticated_device.as_deref() else {
                return Ok((Some(WsSyncFrame::Error {
                    message: "unauthenticated connection; send Auth first".into(),
                }), None));
            };
            // What the POST /v1/disconnect arm marked: a registered device
            // that is not already revoked is revoked (epoch bump, stream
            // teardown, journal release) via the same primitive the primary's
            // own revoke uses; an already-revoked device gets the idempotent
            // ack the store's disconnect produced; an unknown device is an
            // error, like the store's NotPaired.
            match store.ws_device_status(device_id) {
                Ok(Some(status)) if status != "revoked" => match store.revoke_device(device_id, now_ms()) {
                    Ok(_) => {
                        on_disconnect(device_id);
                        Ok((Some(WsSyncFrame::DisconnectAck {}), None))
                    }
                    Err(err) => Ok((Some(WsSyncFrame::Error { message: err.to_string() }), None)),
                },
                Ok(Some(_)) => Ok((Some(WsSyncFrame::DisconnectAck {}), None)),
                Ok(None) => Ok((Some(WsSyncFrame::Error {
                    message: "device is not in paired state".into(),
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
        WsSyncFrame::RollupSnapshot { .. } | WsSyncFrame::DisplayNameAck {} | WsSyncFrame::DisconnectAck {} | WsSyncFrame::DeviceState { .. }
        | WsSyncFrame::RollupAck { .. } | WsSyncFrame::AuthAck { .. } | WsSyncFrame::StatusResponse { .. } | WsSyncFrame::Error { .. } => {
            Ok((None, None))
        }
    }
}

/// The session-authenticated display-name write — the same validity rule,
/// paired gate and UPDATE predicate `SyncStore::update_display_name` applies
/// to a client-signed request, with the authenticated session standing in
/// for the signature the frame no longer carries.
fn session_update_display_name(store: &mut SyncStore, device_id: &str, name: &str) -> SyncResult<()> {
    crate::protocol::valid_text("display_name", name, false)?;
    let record = store.device(device_id)?.ok_or_else(|| SyncError::NotPaired(device_id.to_string()))?;
    if record.status != DeviceStatus::Paired { return Err(SyncError::NotPaired(device_id.to_string())); }
    let changed = store.state.execute(
        "UPDATE device_registry SET display_name = ?1 WHERE device_id = ?2 AND origin = ?3 AND status = 'paired'",
        rusqlite::params![name, device_id, record.origin],
    ).map_err(|e| SyncError::Sqlite(e.to_string()))?;
    if changed != 1 { return Err(SyncError::NotPaired(device_id.to_string())); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DeviceIdentity, PairingRequest};

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
        let no_sink = |_device: &str| {};
        let (reply, session) = process_server_frame(&frame, &mut store, &mut authenticated, &|| 1_000, &no_sink).unwrap();
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
        let no_sink = |_device: &str| {};
        let (reply, session) = process_server_frame(&frame, &mut store, &mut authenticated, &|| 1_000, &no_sink).unwrap();
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

    fn no_disconnect_sink() -> impl Fn(&str) {
        |_device: &str| {}
    }

    #[test]
    fn rollup_fetch_serves_the_primary_snapshot_to_a_paired_session() {
        let (mut store, device) = paired_store();
        // The fetch is served only by a sharing primary — turn the flag on
        // like a configured primary would carry it.
        store.save_primary_config(&crate::PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
            advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: true,
            share_data_with_secondaries: true,
        }, 5).unwrap();
        let day = "2026-10-22";
        store.index().conn().execute(
            "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts)
             VALUES(?1, '', 'codex', 'primary-session', '', '', 'tokens', 9, 0, 0, 0, 0, 0, 1, 1, 1000, 2000)",
            [&day],
        ).unwrap();
        // A foreign-origin row must never travel: the snapshot is local-origin only.
        store.index().conn().execute(
            "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts)
             VALUES(?1, 'other-origin', 'codex', 's', '', '', 'tokens', 99, 0, 0, 0, 0, 0, 1, 1, 1000, 2000)",
            [&day],
        ).unwrap();
        let mut authenticated = Some(device.device_id.clone());
        let (reply, session) = process_server_frame(&WsSyncFrame::RollupFetch {}, &mut store, &mut authenticated, &|| 1_000, &no_disconnect_sink()).unwrap();
        assert!(session.is_none());
        match reply {
            Some(WsSyncFrame::RollupSnapshot { snapshot }) => {
                assert_eq!(snapshot.origin, usage_index::hostname(), "the label the secondary groups the rows under");
                assert_eq!(snapshot.rows.len(), 1, "only the primary's own local-origin rows travel");
                assert_eq!(snapshot.rows[0].in_tok, 9.0);
            }
            other => panic!("expected the snapshot frame, got {other:?}"),
        }
    }

    #[test]
    fn rollup_fetch_without_a_paired_session_is_refused() {
        let (mut store, device) = paired_store();
        let mut authenticated = None;
        let (reply, _) = process_server_frame(&WsSyncFrame::RollupFetch {}, &mut store, &mut authenticated, &|| 1_000, &no_disconnect_sink()).unwrap();
        match reply {
            Some(WsSyncFrame::Error { message }) => assert!(message.contains("unauthenticated"), "{message}"),
            other => panic!("expected the wire error frame, got {other:?}"),
        }
        // A session whose device is no longer paired must not fetch either.
        let mut authenticated = Some(device.device_id.clone());
        store.revoke_device(&device.device_id, 2_000).unwrap();
        let (reply, _) = process_server_frame(&WsSyncFrame::RollupFetch {}, &mut store, &mut authenticated, &|| 3_000, &no_disconnect_sink()).unwrap();
        match reply {
            Some(WsSyncFrame::Error { message }) => assert!(message.contains("not authorized"), "{message}"),
            other => panic!("expected the wire error frame, got {other:?}"),
        }
    }

    #[test]
    fn display_name_update_applies_for_the_session_device() {
        let (mut store, device) = paired_store();
        let mut authenticated = Some(device.device_id.clone());
        let (reply, _) = process_server_frame(
            &WsSyncFrame::DisplayNameUpdate { name: "Renamed MacBook".into() },
            &mut store, &mut authenticated, &|| 2_000, &no_disconnect_sink(),
        ).unwrap();
        assert!(matches!(reply, Some(WsSyncFrame::DisplayNameAck {})), "{reply:?}");
        let record = store.device(&device.device_id).unwrap().unwrap();
        assert_eq!(record.display_name, "Renamed MacBook");
        // An invalid name is refused and leaves the stored name alone.
        let (reply, _) = process_server_frame(
            &WsSyncFrame::DisplayNameUpdate { name: String::new() },
            &mut store, &mut authenticated, &|| 3_000, &no_disconnect_sink(),
        ).unwrap();
        assert!(matches!(reply, Some(WsSyncFrame::Error { .. })), "{reply:?}");
        assert_eq!(store.device(&device.device_id).unwrap().unwrap().display_name, "Renamed MacBook");
    }

    #[test]
    fn disconnect_notify_revokes_the_device_and_fires_the_sink() {
        let (mut store, device) = paired_store();
        let mut authenticated = Some(device.device_id.clone());
        let sink = std::sync::Mutex::new(None);
        let sink_ref = &sink;
        let on_disconnect = |device: &str| *sink_ref.lock().unwrap() = Some(device.to_string());
        let (reply, _) = process_server_frame(&WsSyncFrame::DisconnectNotify {}, &mut store, &mut authenticated, &|| 2_000, &on_disconnect).unwrap();
        assert!(matches!(reply, Some(WsSyncFrame::DisconnectAck {})), "{reply:?}");
        assert_eq!(sink.lock().unwrap().as_deref(), Some(device.device_id.as_str()), "the clean notice must wake the primary's UI");
        let status = store.ws_device_status(&device.device_id).unwrap();
        assert_eq!(status.as_deref(), Some("revoked"), "the notice must mark what the POST /v1/disconnect arm marked");
        // The notice is idempotent: a second one is an ack, not an error, and
        // the sink does not fire twice.
        let fired = std::sync::Mutex::new(0);
        let fired_ref = &fired;
        let count_sink = |_device: &str| *fired_ref.lock().unwrap() += 1;
        let (reply, _) = process_server_frame(&WsSyncFrame::DisconnectNotify {}, &mut store, &mut authenticated, &|| 3_000, &count_sink).unwrap();
        assert!(matches!(reply, Some(WsSyncFrame::DisconnectAck {})), "{reply:?}");
        assert_eq!(*fired.lock().unwrap(), 0, "an already-revoked device has nothing left to mark");
    }

    #[test]
    fn device_state_is_a_one_way_server_advisory_the_server_never_answers() {
        let (mut store, _device) = paired_store();
        let mut authenticated = Some("whatever".into());
        let (reply, session) = process_server_frame(
            &WsSyncFrame::DeviceState { status: "paused".into(), data_sharing: false },
            &mut store, &mut authenticated, &|| 1_000, &no_disconnect_sink(),
        ).unwrap();
        assert!(reply.is_none(), "a push frame is one-way: {reply:?}");
        assert!(session.is_none());
    }

    #[test]
    fn a_refused_session_error_carries_the_registry_status_word() {
        let (mut store, device) = paired_store();
        // Paused is recoverable: the word must ride so the secondary maps the
        // refusal onto the status poll's taxonomy instead of a generic error.
        store.pause_device(&device.device_id).unwrap();
        let mut authenticated = None;
        let (reply, session) = process_server_frame(
            &WsSyncFrame::Auth {
                device_id: device.device_id.clone(), origin: device.origin.clone(),
                display_name: device.origin.clone(), version: WS_PROTOCOL_VERSION.into(),
            },
            &mut store, &mut authenticated, &|| 1_000, &no_disconnect_sink(),
        ).unwrap();
        assert!(session.is_none(), "a paused device does not open a session");
        match reply {
            Some(WsSyncFrame::AuthAck { status, error, .. }) => {
                assert_eq!(status, "paused");
                let error = error.expect("the refusal explains itself");
                assert!(error.contains("paused"), "{error}");
            }
            other => panic!("expected the refused ack, got {other:?}"),
        }
        // Revoked is terminal and carries its own word the same way.
        store.resume_device(&device.device_id).unwrap();
        store.revoke_device(&device.device_id, 2_000).unwrap();
        let (reply, _) = process_server_frame(
            &WsSyncFrame::Auth {
                device_id: device.device_id.clone(), origin: device.origin.clone(),
                display_name: device.origin.clone(), version: WS_PROTOCOL_VERSION.into(),
            },
            &mut store, &mut authenticated, &|| 3_000, &no_disconnect_sink(),
        ).unwrap();
        match reply {
            Some(WsSyncFrame::AuthAck { status, error, .. }) => {
                assert_eq!(status, "revoked");
                let error = error.expect("the refusal explains itself");
                assert!(error.contains("revoked"), "{error}");
            }
            other => panic!("expected the refused ack, got {other:?}"),
        }
    }
}

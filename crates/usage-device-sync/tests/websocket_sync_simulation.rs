//! Comprehensive WebSocket multi-device sync simulation test suite.
//!
//! Validates end-to-end:
//! 1. Primary-Secondary full WebSocket cycle (Auth, Ping/Pong, RollupSync/RollupAck, StatusRequest).
//! 2. Unpaired / unauthorized secondary rejection.
//! 3. Multi-secondary concurrent WebSocket sync (multiple distinct secondary devices streaming rollups in parallel).
//! 4. Secondary reconnect and successive full-snapshot rollup pushes.
//! 5. Stale-frame rejection and full-replace rollup semantics.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use usage_device_sync::{
    deliver_disconnect_notice, hash_bytes, DeviceIdentity, DeviceStatus, HttpsServerConfig,
    HttpsSyncServer, HttpsTransportConfig, PairingRequest, RollupRow, RollupSnapshot,
    SecondarySyncConfig, SyncStore, Tls13HttpsTransport, WsSyncFrame, WS_PROTOCOL_VERSION,
};

struct CertFixture {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

fn generate_cert() -> CertFixture {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let key_der = key.serialize_der();
    let params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    let cert = params.self_signed(&key).unwrap();
    CertFixture {
        cert_der: cert.der().to_vec(),
        key_der,
    }
}

fn staging_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("tokenme-sim-{tag}-{nanos}-{}", std::process::id()))
}

fn setup_server_config(cert: &CertFixture, staging_dir: PathBuf, max_concurrent: usize) -> HttpsServerConfig {
    HttpsServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        certificate_der: cert.cert_der.clone(),
        private_key_der: cert.key_der.clone(),
        allow_lan: true,
        allowed_peer_ips: Vec::new(),
        max_body_bytes: 128 * 1024 * 1024,
        max_concurrent,
        staging_dir,
        idle_timeout: Duration::from_secs(30),
        total_timeout: Duration::from_secs(600),
        staging_quota_bytes: 512 * 1024 * 1024,
    }
}

fn pair_secondary(store: &mut SyncStore, device: &DeviceIdentity, _primary_id: &str) {
    let invite = store
        .create_pairing_invite(
            "https://localhost:47821",
            "1111222233334444555566667777888811112222333344445555666677778888",
            100_000,
        )
        .unwrap();
    let pairing = PairingRequest::new(&invite, device, "test-os").unwrap();
    let req = store.receive_pairing_request(&pairing, 1).unwrap();
    store.approve_pairing(&req.request_id, 2).unwrap();
}

/// The rows a secondary ships: its local-origin aggregate, so each row's own
/// `origin` is empty by construction and the primary resolves the label from
/// the device registry.
fn rollup_rows(count: usize) -> Vec<RollupRow> {
    rollup_rows_on("2026-10", count)
}

/// Rows sharing one day prefix but distinct day numbers, so no two rows of a
/// snapshot collide on the remote_rollup primary key.
fn rollup_rows_on(month: &str, count: usize) -> Vec<RollupRow> {
    (0..count)
        .map(|i| RollupRow {
            day: format!("{month}-{:02}", 10 + i),
            origin: String::new(),
            tool: "codex".into(),
            session: "session".into(),
            project: "project-tokenme".into(),
            model: "gpt-5".into(),
            meter: "tokens".into(),
            in_tok: 100.0,
            cc_tok: 0.0,
            cr_tok: 0.0,
            out_tok: 50.0,
            reason_tok: 0.0,
            credits: 0.0,
            n: 3,
            nonzero: 2,
            min_ts: 1000 + i as i64,
            max_ts: 2000 + i as i64,
        })
        .collect()
}

fn rollup_snapshot(origin: &str, count: usize) -> RollupSnapshot {
    RollupSnapshot { origin: origin.into(), rows: rollup_rows(count) }
}

#[test]
fn test_primary_secondary_ws_full_cycle() {
    let cert = generate_cert();
    let primary_id = "primary-node-1";
    let staging = staging_path("full-cycle");
    let mut store = SyncStore::open_in_memory(primary_id).unwrap();

    let secondary = DeviceIdentity::create("sec-node-1", "macOS-Client").unwrap();
    pair_secondary(&mut store, &secondary, primary_id);

    let config = setup_server_config(&cert, staging, 4);
    let server = HttpsSyncServer::bind(config, store).unwrap();
    let addr = server.local_addr().unwrap();

    let server_handle = thread::spawn(move || server.run_connections(1));

    let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
        format!("https://127.0.0.1:{}", addr.port()),
        "localhost",
        cert.cert_der.clone(),
    ))
    .unwrap();

    let (mut ws, _guard) = transport.connect_ws().unwrap();

    // 1. Auth frame
    let auth = WsSyncFrame::Auth {
        device_id: secondary.device_id.clone(),
        origin: secondary.origin.clone(),
        display_name: "MacBook Pro".into(),
        version: WS_PROTOCOL_VERSION.into(),
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&auth).unwrap())).unwrap();

    let ack_msg = ws.read().unwrap();
    match ack_msg {
        tungstenite::Message::Text(text) => {
            let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
            match frame {
                WsSyncFrame::AuthAck { status, primary_id: ack_prim, epoch, error, .. } => {
                    assert_eq!(status, "connected");
                    assert_eq!(ack_prim, primary_id);
                    assert_eq!(epoch, 1);
                    assert!(error.is_none());
                }
                other => panic!("expected AuthAck, got {other:?}"),
            }
        }
        other => panic!("expected text frame, got {other:?}"),
    }

    // 2. Ping / Pong heartbeat
    let client_time = 1700000000;
    ws.send(tungstenite::Message::Text(
        serde_json::to_string(&WsSyncFrame::Ping { client_time_ms: client_time }).unwrap(),
    ))
    .unwrap();

    let pong_msg = ws.read().unwrap();
    match pong_msg {
        tungstenite::Message::Text(text) => {
            let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
            match frame {
                WsSyncFrame::Pong { server_time_ms } => {
                    assert!(server_time_ms > 0);
                }
                other => panic!("expected Pong, got {other:?}"),
            }
        }
        other => panic!("expected text frame, got {other:?}"),
    }

    // 3. Symmetric rollup uplink: RollupSync / RollupAck
    let push = WsSyncFrame::RollupSync { snapshot: rollup_snapshot(&secondary.origin, 10) };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&push).unwrap())).unwrap();

    let sync_ack_msg = ws.read().unwrap();
    match sync_ack_msg {
        tungstenite::Message::Text(text) => {
            let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
            match frame {
                WsSyncFrame::RollupAck { origin, accepted, n } => {
                    assert!(accepted, "rollup push should succeed");
                    assert_eq!(origin, secondary.origin, "the ack resolves the device's registry origin");
                    assert_eq!(n, 10);
                }
                other => panic!("expected RollupAck, got {other:?}"),
            }
        }
        other => panic!("expected text frame, got {other:?}"),
    }

    // 4. StatusRequest / StatusResponse
    let status_req = WsSyncFrame::StatusRequest { device_id: secondary.device_id.clone() };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&status_req).unwrap())).unwrap();

    let status_resp_msg = ws.read().unwrap();
    match status_resp_msg {
        tungstenite::Message::Text(text) => {
            let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
            match frame {
                WsSyncFrame::StatusResponse { status, primary_id: sid, .. } => {
                    assert_eq!(status, "paired");
                    assert_eq!(sid, primary_id);
                }
                other => panic!("expected StatusResponse, got {other:?}"),
            }
        }
        other => panic!("expected text frame, got {other:?}"),
    }

    // 5. Clean close
    let _ = ws.close(None);
    assert!(server_handle.join().unwrap().is_ok());
}

#[test]
fn test_unauthorized_secondary_ws_rejected() {
    let cert = generate_cert();
    let primary_id = "primary-node-unauth";
    let staging = staging_path("unauth");
    let store = SyncStore::open_in_memory(primary_id).unwrap();

    let unauthorized = DeviceIdentity::create("sec-intruder", "Hacker-OS").unwrap();

    let config = setup_server_config(&cert, staging, 2);
    let server = HttpsSyncServer::bind(config, store).unwrap();
    let addr = server.local_addr().unwrap();

    let server_handle = thread::spawn(move || server.run_connections(1));

    let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
        format!("https://127.0.0.1:{}", addr.port()),
        "localhost",
        cert.cert_der.clone(),
    ))
    .unwrap();

    let (mut ws, _guard) = transport.connect_ws().unwrap();

    // 1. Send Auth with unapproved identity
    let auth = WsSyncFrame::Auth {
        device_id: unauthorized.device_id.clone(),
        origin: unauthorized.origin.clone(),
        display_name: "Intruder".into(),
        version: WS_PROTOCOL_VERSION.into(),
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&auth).unwrap())).unwrap();

    let ack_msg = ws.read().unwrap();
    match ack_msg {
        tungstenite::Message::Text(text) => {
            let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
            match frame {
                WsSyncFrame::AuthAck { status, error, .. } => {
                    assert_eq!(status, "unpaired");
                    assert!(error.is_some());
                }
                other => panic!("expected AuthAck, got {other:?}"),
            }
        }
        _other => panic!("expected text frame"),
    }

    let _ = ws.close(None);
    assert!(server_handle.join().unwrap().is_ok());
}

#[test]
fn test_multi_secondary_concurrent_websocket_sync() {
    let cert = generate_cert();
    let primary_id = "primary-concurrent-hub";
    let staging = staging_path("multi-sec");
    let mut store = SyncStore::open_in_memory(primary_id).unwrap();

    let num_secondaries = 4;
    let mut secondaries = Vec::new();

    let row_counts = [5, 8, 12, 15];

    for i in 0..num_secondaries {
        let sec = DeviceIdentity::create(&format!("sec-node-{i}"), &format!("OS-{i}")).unwrap();
        pair_secondary(&mut store, &sec, primary_id);
        secondaries.push(sec);
    }

    let config = setup_server_config(&cert, staging, 16);
    let server = HttpsSyncServer::bind(config, store).unwrap();
    let addr = server.local_addr().unwrap();

    // Server accepts connections from all 4 secondary devices
    let server_handle = thread::spawn(move || server.run_connections(num_secondaries));

    // Spawn 4 concurrent secondary clients
    let mut client_threads = Vec::new();

    for i in 0..num_secondaries {
        let sec = secondaries[i].clone();
        let count = row_counts[i];
        let cert_der = cert.cert_der.clone();
        let port = addr.port();

        let handle = thread::spawn(move || {
            let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
                format!("https://127.0.0.1:{port}"),
                "localhost",
                cert_der,
            ))
            .unwrap();

            let (mut ws, _guard) = transport.connect_ws().unwrap();

            // 1. Auth
            let auth = WsSyncFrame::Auth {
                device_id: sec.device_id.clone(),
                origin: sec.origin.clone(),
                display_name: format!("Worker-{i}"),
                version: WS_PROTOCOL_VERSION.into(),
            };
            ws.send(tungstenite::Message::Text(serde_json::to_string(&auth).unwrap())).unwrap();

            let ack = ws.read().unwrap();
            match ack {
                tungstenite::Message::Text(text) => {
                    let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
                    match frame {
                        WsSyncFrame::AuthAck { status, .. } => assert_eq!(status, "connected"),
                        other => panic!("client {i} expected AuthAck, got {other:?}"),
                    }
                }
                other => panic!("expected text frame, got {other:?}"),
            }

            // 2. Ping
            ws.send(tungstenite::Message::Text(
                serde_json::to_string(&WsSyncFrame::Ping { client_time_ms: 1000 + i as i64 }).unwrap(),
            ))
            .unwrap();
            let pong = ws.read().unwrap();
            assert!(matches!(pong, tungstenite::Message::Text(_)));

            // 3. RollupSync
            let push = WsSyncFrame::RollupSync { snapshot: rollup_snapshot(&sec.origin, count) };
            ws.send(tungstenite::Message::Text(serde_json::to_string(&push).unwrap())).unwrap();

            let reply = ws.read().unwrap();
            match reply {
                tungstenite::Message::Text(text) => {
                    let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
                    match frame {
                        WsSyncFrame::RollupAck { origin, accepted, n } => {
                            assert!(accepted, "secondary {i} push failed");
                            assert_eq!(origin, sec.origin, "secondary {i} rows must land under its registry origin");
                            assert_eq!(n, count);
                        }
                        other => panic!("client {i} expected RollupAck, got {other:?}"),
                    }
                }
                _other => panic!("expected text frame"),
            }

            let _ = ws.close(None);
        });

        client_threads.push(handle);
    }

    // Await all client worker threads
    for (i, h) in client_threads.into_iter().enumerate() {
        h.join().unwrap_or_else(|_| panic!("client thread {i} failed"));
    }

    // Await server completion
    assert!(server_handle.join().unwrap().is_ok());
}

#[test]
fn test_secondary_reconnect_and_sequential_batches() {
    let cert = generate_cert();
    let primary_id = "primary-reconnect-hub";
    let staging = staging_path("reconnect");
    let mut store = SyncStore::open_in_memory(primary_id).unwrap();

    let sec = DeviceIdentity::create("sec-persistent", "Desktop-Workstation").unwrap();
    pair_secondary(&mut store, &sec, primary_id);

    let config = setup_server_config(&cert, staging, 4);
    let server = HttpsSyncServer::bind(config, store).unwrap();
    let addr = server.local_addr().unwrap();

    // Server serves 2 successive connections from the same secondary
    let server_handle = thread::spawn(move || server.run_connections(2));

    // Session 1: Push snapshot 1
    {
        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()),
            "localhost",
            cert.cert_der.clone(),
        ))
        .unwrap();

        let (mut ws, _guard) = transport.connect_ws().unwrap();

        let auth = WsSyncFrame::Auth {
            device_id: sec.device_id.clone(),
            origin: sec.origin.clone(),
            display_name: "Desktop".into(),
            version: WS_PROTOCOL_VERSION.into(),
        };
        ws.send(tungstenite::Message::Text(serde_json::to_string(&auth).unwrap())).unwrap();
        let _ = ws.read().unwrap(); // AuthAck

        let push1 = WsSyncFrame::RollupSync { snapshot: rollup_snapshot(&sec.origin, 7) };
        ws.send(tungstenite::Message::Text(serde_json::to_string(&push1).unwrap())).unwrap();

        let ack1 = ws.read().unwrap();
        match ack1 {
            tungstenite::Message::Text(text) => {
                let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
                match frame {
                    WsSyncFrame::RollupAck { origin, accepted, n } => {
                        assert!(accepted);
                        assert_eq!(origin, sec.origin);
                        assert_eq!(n, 7);
                    }
                    other => panic!("expected RollupAck, got {other:?}"),
                }
            }
            _ => panic!("expected text"),
        }

        // Simulate connection drop / disconnect
        let _ = ws.close(None);
    }

    // Session 2: Reconnect on fresh WebSocket, push snapshot 2
    {
        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()),
            "localhost",
            cert.cert_der.clone(),
        ))
        .unwrap();

        let (mut ws, _guard) = transport.connect_ws().unwrap();

        let auth = WsSyncFrame::Auth {
            device_id: sec.device_id.clone(),
            origin: sec.origin.clone(),
            display_name: "Desktop".into(),
            version: WS_PROTOCOL_VERSION.into(),
        };
        ws.send(tungstenite::Message::Text(serde_json::to_string(&auth).unwrap())).unwrap();
        let _ = ws.read().unwrap(); // AuthAck

        let push2 = WsSyncFrame::RollupSync { snapshot: rollup_snapshot(&sec.origin, 9) };
        ws.send(tungstenite::Message::Text(serde_json::to_string(&push2).unwrap())).unwrap();

        let ack2 = ws.read().unwrap();
        match ack2 {
            tungstenite::Message::Text(text) => {
                let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
                match frame {
                    WsSyncFrame::RollupAck { origin, accepted, n } => {
                        assert!(accepted);
                        assert_eq!(origin, sec.origin);
                        assert_eq!(n, 9);
                    }
                    other => panic!("expected RollupAck, got {other:?}"),
                }
            }
            _ => panic!("expected text"),
        }

        let _ = ws.close(None);
    }

    assert!(server_handle.join().unwrap().is_ok());
}

#[test]
fn test_stale_sync_push_rejected_and_clean_recovery() {
    let cert = generate_cert();
    let primary_id = "primary-stale-test";
    let staging = staging_path("stale-recovery");
    let mut store = SyncStore::open_in_memory(primary_id).unwrap();

    let sec = DeviceIdentity::create("sec-stale", "Stale-Device").unwrap();
    pair_secondary(&mut store, &sec, primary_id);

    let config = setup_server_config(&cert, staging, 4);
    let server = HttpsSyncServer::bind(config, store).unwrap();
    let addr = server.local_addr().unwrap();

    let server_handle = thread::spawn(move || server.run_connections(1));

    let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
        format!("https://127.0.0.1:{}", addr.port()),
        "localhost",
        cert.cert_der.clone(),
    ))
    .unwrap();

    let (mut ws, _socket) = transport.connect_ws().unwrap();

    // 1. Authenticate over WebSocket
    let auth = WsSyncFrame::Auth {
        device_id: sec.device_id.clone(),
        origin: sec.origin.clone(),
        display_name: "Stale Device".into(),
        version: WS_PROTOCOL_VERSION.into(),
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&auth).unwrap())).unwrap();
    let auth_ack = ws.read().unwrap();
    assert!(matches!(
        serde_json::from_str::<WsSyncFrame>(&auth_ack.to_text().unwrap()).unwrap(),
        WsSyncFrame::AuthAck { status, .. } if status == "connected"
    ));

    // 2. Transmit the retired row-level package frame: unknown wire now.
    let stale_push = r#"{"type":"sync_push","batch_id":"batch-stale","envelope":null,"signature":[],"gzip_bytes":""}"#;
    ws.send(tungstenite::Message::Text(stale_push.into())).unwrap();

    // 3. Verify the primary answers the standard error shape without killing
    // the session or corrupting the database.
    let reject_ack = ws.read().unwrap();
    let reject_frame: WsSyncFrame = serde_json::from_str(reject_ack.to_text().unwrap()).unwrap();
    match reject_frame {
        WsSyncFrame::Error { message } => {
            assert!(
                message.to_ascii_lowercase().contains("invalid frame"),
                "error message should indicate an unknown frame: {message}"
            );
        }
        other => panic!("expected the wire error frame, got {other:?}"),
    }

    // 4. Recovery: the same session accepts the symmetric rollup uplink.
    let clean_push = WsSyncFrame::RollupSync { snapshot: rollup_snapshot(&sec.origin, 5) };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&clean_push).unwrap())).unwrap();

    // 5. Verify the primary accepts the recovered snapshot.
    let recovery_ack = ws.read().unwrap();
    let recovery_frame: WsSyncFrame = serde_json::from_str(recovery_ack.to_text().unwrap()).unwrap();
    match recovery_frame {
        WsSyncFrame::RollupAck { origin, accepted, n } => {
            assert!(accepted, "clean recovered snapshot must succeed");
            assert_eq!(origin, sec.origin);
            assert_eq!(n, 5);
        }
        other => panic!("expected RollupAck success, got {other:?}"),
    }

    // 6. Verify health and status over the WebSocket session remains normal
    ws.send(tungstenite::Message::Text(
        serde_json::to_string(&WsSyncFrame::StatusRequest { device_id: sec.device_id.clone() }).unwrap(),
    ))
    .unwrap();
    let status_ack = ws.read().unwrap();
    match serde_json::from_str::<WsSyncFrame>(status_ack.to_text().unwrap()).unwrap() {
        WsSyncFrame::StatusResponse { status, .. } => {
            assert_eq!(status, "paired");
        }
        other => panic!("expected StatusResponse, got {other:?}"),
    }

    let _ = ws.close(None);
    assert!(server_handle.join().unwrap().is_ok());
}

#[test]
fn test_repeated_rollup_sync_lands_rows_under_registry_origin() {
    let cert = generate_cert();
    let primary_id = "primary-replace-test";
    let temp_dir = tempfile::tempdir().unwrap();
    let index_path = temp_dir.path().join("index.db");
    let mut store = SyncStore::open(&index_path, primary_id).unwrap();

    let sec = DeviceIdentity::create("sec-replace", "Replace-Device").unwrap();
    pair_secondary(&mut store, &sec, primary_id);

    let config = setup_server_config(&cert, temp_dir.path().join("staging"), 4);
    let server = HttpsSyncServer::bind(config, store).unwrap();
    let addr = server.local_addr().unwrap();

    let server_handle = thread::spawn(move || server.run_connections(1));

    let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
        format!("https://127.0.0.1:{}", addr.port()),
        "localhost",
        cert.cert_der.clone(),
    ))
    .unwrap();

    let (mut ws, _socket) = transport.connect_ws().unwrap();

    // 1. Authenticate
    let auth = WsSyncFrame::Auth {
        device_id: sec.device_id.clone(),
        origin: sec.origin.clone(),
        display_name: "Replace Device".into(),
        version: WS_PROTOCOL_VERSION.into(),
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&auth).unwrap())).unwrap();
    let _ = ws.read().unwrap(); // AuthAck

    // 2. Push a first 3-row snapshot
    let push1 = WsSyncFrame::RollupSync {
        snapshot: RollupSnapshot { origin: sec.origin.clone(), rows: rollup_rows_on("2026-11", 3) },
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&push1).unwrap())).unwrap();
    let ack1 = ws.read().unwrap();
    assert!(matches!(
        serde_json::from_str::<WsSyncFrame>(ack1.to_text().unwrap()).unwrap(),
        WsSyncFrame::RollupAck { accepted: true, n: 3, .. }
    ));

    // 3. Push a second 2-row snapshot on disjoint days, same session
    let push2 = WsSyncFrame::RollupSync {
        snapshot: RollupSnapshot { origin: sec.origin.clone(), rows: rollup_rows_on("2026-12", 2) },
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&push2).unwrap())).unwrap();
    let ack2 = ws.read().unwrap();
    assert!(matches!(
        serde_json::from_str::<WsSyncFrame>(ack2.to_text().unwrap()).unwrap(),
        WsSyncFrame::RollupAck { accepted: true, n: 2, .. }
    ));

    // 4. Both snapshots' rows are stored under the device's REGISTRY origin
    // (never a wire-declared label), applied as INSERT OR REPLACE per row.
    let reopened = SyncStore::open(&index_path, primary_id).unwrap();
    let stored: i64 = reopened.index().conn().query_row(
        "SELECT count(*) FROM remote_rollup WHERE origin = ?1",
        [&sec.origin], |row| row.get(0),
    ).unwrap();
    assert_eq!(stored, 5, "both snapshots must land under the registry origin");
    let november: i64 = reopened.index().conn().query_row(
        "SELECT count(*) FROM remote_rollup WHERE origin = ?1 AND day LIKE '2026-11-%'",
        [&sec.origin], |row| row.get(0),
    ).unwrap();
    let december: i64 = reopened.index().conn().query_row(
        "SELECT count(*) FROM remote_rollup WHERE origin = ?1 AND day LIKE '2026-12-%'",
        [&sec.origin], |row| row.get(0),
    ).unwrap();
    assert_eq!(november, 3);
    assert_eq!(december, 2);

    let _ = ws.close(None);
    assert!(server_handle.join().unwrap().is_ok());
}

#[test]
fn test_secondary_disconnect_notifies_primary_and_halts_communication() {
    let cert = generate_cert();
    let primary_id = "primary-disc-test";
    let temp_dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let index_path = temp_dir.path().join("index.db");
    let mut store = SyncStore::open(&index_path, primary_id).unwrap();

    let id_path = temp_dir.path().join("sec_identity.json");
    let sec = DeviceIdentity::load_or_create(&id_path, "sec-disc", "Disconnect-Device").unwrap();
    pair_secondary(&mut store, &sec, primary_id);

    let config = setup_server_config(&cert, temp_dir.path().join("staging"), 4);
    let server = HttpsSyncServer::bind(config, store).unwrap();
    let addr = server.local_addr().unwrap();

    let disconnected_device = Arc::new(std::sync::Mutex::new(None));
    let disconnected_clone = Arc::clone(&disconnected_device);
    server.set_disconnect_sink(move |dev_id| {
        *disconnected_clone.lock().unwrap() = Some(dev_id.to_string());
    }).unwrap();

    let server_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_stop_clone = Arc::clone(&server_stop);
    let server_handle = thread::spawn(move || server.run_until(&server_stop_clone));

    let endpoint = format!("https://127.0.0.1:{}", addr.port());
    let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
        endpoint.clone(),
        "localhost",
        cert.cert_der.clone(),
    )).unwrap();

    let (mut ws, _socket) = transport.connect_ws().unwrap();

    // 1. Authenticate over WebSocket
    let auth = WsSyncFrame::Auth {
        device_id: sec.device_id.clone(),
        origin: sec.origin.clone(),
        display_name: "Disconnect Device".into(),
        version: WS_PROTOCOL_VERSION.into(),
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&auth).unwrap())).unwrap();
    let auth_ack = ws.read().unwrap();
    assert!(matches!(
        serde_json::from_str::<WsSyncFrame>(auth_ack.to_text().unwrap()).unwrap(),
        WsSyncFrame::AuthAck { status, .. } if status == "connected"
    ));

    // 2. Upload initial rollup snapshot before disconnect
    let push1 = WsSyncFrame::RollupSync { snapshot: rollup_snapshot(&sec.origin, 5) };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&push1).unwrap())).unwrap();
    let ack1 = ws.read().unwrap();
    assert!(matches!(
        serde_json::from_str::<WsSyncFrame>(ack1.to_text().unwrap()).unwrap(),
        WsSyncFrame::RollupAck { accepted: true, .. }
    ));

    // 3. Secondary clicks disconnect: deliver disconnect notice immediately to primary
    let sec_config = SecondarySyncConfig {
        primary_id: primary_id.into(),
        primary_display_name: Some("Primary Mac".into()), primary_os: Some("macos".into()),
        endpoint: endpoint.clone(),
        server_name: "localhost".into(),
        certificate_der: cert.cert_der.clone(),
        certificate_pin: hash_bytes(&cert.cert_der),
        device_id: sec.device_id.clone(),
        origin: sec.origin.clone(),
        identity_path: id_path,
        stream_id: None,
        data_epoch: None,
        expected_generation: None,
        enabled: true,
        capture_enabled: true,
        last_error: None,
        last_manifest_hash: None,
        last_gzip_hash: None,
        last_connected_at_ms: Some(1000),
        last_ack_generation: None,
        last_ack_receipt_hash: None,
    };

    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    deliver_disconnect_notice(&sec_config, now_ms).unwrap();

    // 4. Verify primary received disconnect notification via sink callback
    let notified = disconnected_device.lock().unwrap().clone();
    assert_eq!(notified.as_deref(), Some(sec.device_id.as_str()), "primary must fire disconnect callback");

    // 5. Verify primary state: secondary device is Revoked, stream is inactive
    let primary_reopen = SyncStore::open(&index_path, primary_id).unwrap();
    let dev = primary_reopen.device(&sec.device_id).unwrap().expect("device exists");
    assert_eq!(dev.status, DeviceStatus::Revoked, "primary must mark device revoked");

    // 6. If secondary attempts to push again after disconnect over existing socket, it must fail
    let push2 = WsSyncFrame::RollupSync { snapshot: rollup_snapshot(&sec.origin, 2) };
    let _ = ws.send(tungstenite::Message::Text(serde_json::to_string(&push2).unwrap()));
    if let Ok(ack2) = ws.read() {
        if let Ok(frame) = serde_json::from_str::<WsSyncFrame>(ack2.to_text().unwrap()) {
            match frame {
                WsSyncFrame::RollupAck { accepted, .. } => {
                    assert!(!accepted, "push after revocation must not be accepted");
                }
                WsSyncFrame::Error { message } => {
                    assert!(!message.is_empty(), "push after revocation must produce an error");
                }
                _ => {}
            }
        }
    }

    let _ = ws.close(None);
    server_stop.store(true, std::sync::atomic::Ordering::Release);
    assert!(server_handle.join().unwrap().is_ok());
}

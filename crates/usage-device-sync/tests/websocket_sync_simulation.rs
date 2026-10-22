//! Comprehensive WebSocket multi-device sync simulation test suite.
//!
//! Validates end-to-end:
//! 1. Primary-Secondary full WebSocket cycle (Auth, Ping/Pong, Delta SyncPush/Ack, StatusRequest).
//! 2. Unpaired / unauthorized secondary rejection.
//! 3. Multi-secondary concurrent WebSocket sync (multiple distinct secondary devices streaming in parallel).
//! 4. Secondary reconnect and multi-generation incremental sync.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use usage_device_sync::{
    deliver_disconnect_notice, hash_bytes, DeviceIdentity, DeviceStatus, HttpsServerConfig,
    HttpsSyncServer, HttpsTransportConfig, NetworkCounts, NetworkPackage, NetworkRow,
    PairingRequest, SecondarySyncConfig, SignedUpload, SyncStore, Tls13HttpsTransport,
    UploadEnvelope, WsSyncFrame, PROTOCOL_ID, WS_PROTOCOL_VERSION,
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

fn pair_secondary(store: &mut SyncStore, device: &DeviceIdentity, _primary_id: &str) -> String {
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
    let stream = store.open_stream(&device.device_id, &device.public_key(), 3).unwrap();
    stream.stream_id
}

fn build_signed_upload(
    device: &DeviceIdentity,
    primary_id: &str,
    stream_id: &str,
    generation: u64,
    tool: &str,
    row_count: usize,
) -> SignedUpload {
    let rows: Vec<NetworkRow> = (0..row_count)
        .map(|i| NetworkRow {
            key: Some(format!("key-{tool}-{generation}-{i}")),
            occurrence_id: None,
            tool: tool.into(),
            ts_ms: 1000 + i as i64,
            session: format!("session-{tool}-{generation}"),
            project: Some("project-tokenme".into()),
            model: Some("gpt-4o".into()),
            meter: "tokens".into(),
            source: "local".into(),
            counts: NetworkCounts::all_zero(),
            calls: vec![],
        })
        .collect();

    let min_ts = 1000;
    let max_ts = 1000 + row_count as i64 + 100;
    let pkg = NetworkPackage::from_rows(
        device.origin.clone(),
        usage_index::INDEX_SCHEMA_VERSION,
        min_ts,
        max_ts,
        &rows,
        10,
    )
    .unwrap();

    let envelope = UploadEnvelope {
        protocol_id: PROTOCOL_ID.into(),
        primary_id: primary_id.into(),
        device_id: device.device_id.clone(),
        origin: device.origin.clone(),
        stream_id: stream_id.into(),
        data_epoch: 1,
        generation,
        manifest_bytes: pkg.manifest_bytes,
        gzip_sha256: hash_bytes(&pkg.gzip_bytes),
    };

    SignedUpload::sign(envelope, &device.signing_key, pkg.gzip_bytes).unwrap()
}

#[test]
fn test_primary_secondary_ws_full_cycle() {
    let cert = generate_cert();
    let primary_id = "primary-node-1";
    let staging = staging_path("full-cycle");
    let mut store = SyncStore::open_in_memory(primary_id).unwrap();

    let secondary = DeviceIdentity::create("sec-node-1", "macOS-Client").unwrap();
    let stream_id = pair_secondary(&mut store, &secondary, primary_id);

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

    // 3. Delta SyncPush / SyncAck
    let upload = build_signed_upload(&secondary, primary_id, &stream_id, 1, "claude-code", 10);
    let push = WsSyncFrame::SyncPush {
        batch_id: format!("{}-1", secondary.device_id),
        envelope: upload.envelope,
        signature: upload.signature.to_vec(),
        gzip_bytes: upload.gzip_bytes,
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&push).unwrap())).unwrap();

    let sync_ack_msg = ws.read().unwrap();
    match sync_ack_msg {
        tungstenite::Message::Text(text) => {
            let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
            match frame {
                WsSyncFrame::SyncAck { success, receipt, error, .. } => {
                    assert!(success, "sync push should succeed: {error:?}");
                    let rc = receipt.expect("expected receipt");
                    assert_eq!(rc.rows, 10);
                    assert_eq!(rc.inserted, 10);
                    assert_eq!(rc.generation, 1);
                }
                other => panic!("expected SyncAck, got {other:?}"),
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
    let mut stream_ids = Vec::new();

    let tools = ["codex", "cline", "claude-code", "antigravity"];
    let row_counts = [5, 8, 12, 15];

    for i in 0..num_secondaries {
        let sec = DeviceIdentity::create(&format!("sec-node-{i}"), &format!("OS-{i}")).unwrap();
        let sid = pair_secondary(&mut store, &sec, primary_id);
        secondaries.push(sec);
        stream_ids.push(sid);
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
        let stream_id = stream_ids[i].clone();
        let tool = tools[i];
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

            // 3. SyncPush
            let upload = build_signed_upload(&sec, "primary-concurrent-hub", &stream_id, 1, tool, count);
            let push = WsSyncFrame::SyncPush {
                batch_id: format!("{}-{}-1", sec.device_id, tool),
                envelope: upload.envelope,
                signature: upload.signature.to_vec(),
                gzip_bytes: upload.gzip_bytes,
            };
            ws.send(tungstenite::Message::Text(serde_json::to_string(&push).unwrap())).unwrap();

            let reply = ws.read().unwrap();
            match reply {
                tungstenite::Message::Text(text) => {
                    let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
                    match frame {
                        WsSyncFrame::SyncAck { success, receipt, error, .. } => {
                            assert!(success, "secondary {i} push failed: {error:?}");
                            let r = receipt.expect("receipt missing");
                            assert_eq!(r.rows, count as u64);
                            assert_eq!(r.inserted, count as u64);
                        }
                        other => panic!("client {i} expected SyncAck, got {other:?}"),
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
    let stream_id = pair_secondary(&mut store, &sec, primary_id);

    let config = setup_server_config(&cert, staging, 4);
    let server = HttpsSyncServer::bind(config, store).unwrap();
    let addr = server.local_addr().unwrap();

    // Server serves 2 successive connections from the same secondary
    let server_handle = thread::spawn(move || server.run_connections(2));

    // Session 1: Push batch 1 (generation 1)
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

        let upload1 = build_signed_upload(&sec, primary_id, &stream_id, 1, "codex", 7);
        let push1 = WsSyncFrame::SyncPush {
            batch_id: "batch-gen-1".into(),
            envelope: upload1.envelope,
            signature: upload1.signature.to_vec(),
            gzip_bytes: upload1.gzip_bytes,
        };
        ws.send(tungstenite::Message::Text(serde_json::to_string(&push1).unwrap())).unwrap();

        let ack1 = ws.read().unwrap();
        match ack1 {
            tungstenite::Message::Text(text) => {
                let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
                match frame {
                    WsSyncFrame::SyncAck { success, receipt, .. } => {
                        assert!(success);
                        let r = receipt.unwrap();
                        assert_eq!(r.generation, 1);
                        assert_eq!(r.inserted, 7);
                    }
                    other => panic!("expected SyncAck, got {other:?}"),
                }
            }
            _ => panic!("expected text"),
        }

        // Simulate connection drop / disconnect
        let _ = ws.close(None);
    }

    // Session 2: Reconnect on fresh WebSocket, Push batch 2 (generation 2)
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

        let upload2 = build_signed_upload(&sec, primary_id, &stream_id, 2, "codex", 9);
        let push2 = WsSyncFrame::SyncPush {
            batch_id: "batch-gen-2".into(),
            envelope: upload2.envelope,
            signature: upload2.signature.to_vec(),
            gzip_bytes: upload2.gzip_bytes,
        };
        ws.send(tungstenite::Message::Text(serde_json::to_string(&push2).unwrap())).unwrap();

        let ack2 = ws.read().unwrap();
        match ack2 {
            tungstenite::Message::Text(text) => {
                let frame: WsSyncFrame = serde_json::from_str(&text).unwrap();
                match frame {
                    WsSyncFrame::SyncAck { success, receipt, .. } => {
                        assert!(success);
                        let r = receipt.unwrap();
                        assert_eq!(r.generation, 2);
                        assert_eq!(r.inserted, 9);
                    }
                    other => panic!("expected SyncAck, got {other:?}"),
                }
            }
            _ => panic!("expected text"),
        }

        let _ = ws.close(None);
    }

    assert!(server_handle.join().unwrap().is_ok());
}

#[test]
fn test_corrupted_sync_data_auto_rejection_and_clean_recovery() {
    let cert = generate_cert();
    let primary_id = "primary-corrupt-test";
    let staging = staging_path("corrupt-recovery");
    let mut store = SyncStore::open_in_memory(primary_id).unwrap();

    let sec = DeviceIdentity::create("sec-faulty", "Faulty-Device").unwrap();
    let stream_id = pair_secondary(&mut store, &sec, primary_id);

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
        display_name: "Faulty Device".into(),
        version: WS_PROTOCOL_VERSION.into(),
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&auth).unwrap())).unwrap();
    let auth_ack = ws.read().unwrap();
    assert!(matches!(
        serde_json::from_str::<WsSyncFrame>(&auth_ack.to_text().unwrap()).unwrap(),
        WsSyncFrame::AuthAck { status, .. } if status == "connected"
    ));

    // 2. Transmit deliberately corrupted data (corrupted gzip payload)
    let valid_upload = build_signed_upload(&sec, primary_id, &stream_id, 1, "cursor", 5);
    let corrupted_push = WsSyncFrame::SyncPush {
        batch_id: "batch-corrupted-1".into(),
        envelope: valid_upload.envelope.clone(),
        signature: valid_upload.signature.to_vec(),
        gzip_bytes: vec![0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0x00, 0x01, 0x02], // Garbage payload
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&corrupted_push).unwrap())).unwrap();

    // 3. Verify Primary immediately rejects corrupted batch without corrupting database
    let reject_ack = ws.read().unwrap();
    let reject_frame: WsSyncFrame = serde_json::from_str(reject_ack.to_text().unwrap()).unwrap();
    match reject_frame {
        WsSyncFrame::SyncAck { success, error, receipt, .. } => {
            assert!(!success, "corrupted batch MUST be rejected");
            assert!(receipt.is_none(), "no receipt should be generated for corrupted data");
            let err_msg = error.expect("descriptive error required");
            assert!(
                err_msg.to_ascii_lowercase().contains("gzip")
                    || err_msg.to_ascii_lowercase().contains("decompression")
                    || err_msg.to_ascii_lowercase().contains("crc")
                    || err_msg.to_ascii_lowercase().contains("invalid")
                    || err_msg.to_ascii_lowercase().contains("corrupt")
                    || err_msg.to_ascii_lowercase().contains("read package")
                    || err_msg.to_ascii_lowercase().contains("end of file"),
                "error message should indicate payload corruption: {err_msg}"
            );
        }
        other => panic!("expected SyncAck rejection, got {other:?}"),
    }

    // 4. Exception recovery: discard erroneous data, push clean uncorrupted batch
    let clean_push = WsSyncFrame::SyncPush {
        batch_id: "batch-clean-1".into(),
        envelope: valid_upload.envelope,
        signature: valid_upload.signature.to_vec(),
        gzip_bytes: valid_upload.gzip_bytes,
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&clean_push).unwrap())).unwrap();

    // 5. Verify Primary accepts the clean recovered batch
    let recovery_ack = ws.read().unwrap();
    let recovery_frame: WsSyncFrame = serde_json::from_str(recovery_ack.to_text().unwrap()).unwrap();
    match recovery_frame {
        WsSyncFrame::SyncAck { success, receipt, error, .. } => {
            assert!(success, "clean recovered batch must succeed: {error:?}");
            let r = receipt.unwrap();
            assert_eq!(r.generation, 1);
            assert_eq!(r.inserted, 5);
        }
        other => panic!("expected SyncAck success, got {other:?}"),
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
fn test_conflicting_desynchronization_triggers_reconcile() {
    let cert = generate_cert();
    let primary_id = "primary-conflict-test";
    let staging = staging_path("conflict-reconcile");
    let mut store = SyncStore::open_in_memory(primary_id).unwrap();

    let sec = DeviceIdentity::create("sec-conflict", "Conflict-Device").unwrap();
    let stream_id = pair_secondary(&mut store, &sec, primary_id);

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

    // 1. Authenticate
    let auth = WsSyncFrame::Auth {
        device_id: sec.device_id.clone(),
        origin: sec.origin.clone(),
        display_name: "Conflict Device".into(),
        version: WS_PROTOCOL_VERSION.into(),
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&auth).unwrap())).unwrap();
    let _ = ws.read().unwrap(); // AuthAck

    // 2. Upload valid generation 1
    let gen1 = build_signed_upload(&sec, primary_id, &stream_id, 1, "codex", 3);
    let push1 = WsSyncFrame::SyncPush {
        batch_id: "batch-gen-1".into(),
        envelope: gen1.envelope,
        signature: gen1.signature.to_vec(),
        gzip_bytes: gen1.gzip_bytes,
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&push1).unwrap())).unwrap();
    let ack1 = ws.read().unwrap();
    assert!(matches!(
        serde_json::from_str::<WsSyncFrame>(ack1.to_text().unwrap()).unwrap(),
        WsSyncFrame::SyncAck { success: true, .. }
    ));

    // 3. Secondary attempts to push generation 1 AGAIN with conflicting data (desync / split brain)
    let gen1_conflict = build_signed_upload(&sec, primary_id, &stream_id, 1, "cline", 8);
    let push_conflict = WsSyncFrame::SyncPush {
        batch_id: "batch-gen-1-conflict".into(),
        envelope: gen1_conflict.envelope,
        signature: gen1_conflict.signature.to_vec(),
        gzip_bytes: gen1_conflict.gzip_bytes,
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&push_conflict).unwrap())).unwrap();

    // 4. Primary rejects conflict
    let ack_conflict = ws.read().unwrap();
    let frame: WsSyncFrame = serde_json::from_str(ack_conflict.to_text().unwrap()).unwrap();
    match frame {
        WsSyncFrame::SyncAck { success, error, .. } => {
            assert!(!success, "conflicting package must be rejected");
            let err = error.expect("error required");
            assert!(
                err.contains("conflict") || err.contains("different package bytes") || err.contains("reconcile"),
                "unexpected conflict error: {err}"
            );
        }
        other => panic!("expected SyncAck, got {other:?}"),
    }

    // 5. Query status: Primary has automatically transitioned secondary to reconcile_required
    ws.send(tungstenite::Message::Text(
        serde_json::to_string(&WsSyncFrame::StatusRequest { device_id: sec.device_id.clone() }).unwrap(),
    ))
    .unwrap();
    let status_ack = ws.read().unwrap();
    match serde_json::from_str::<WsSyncFrame>(status_ack.to_text().unwrap()).unwrap() {
        WsSyncFrame::StatusResponse { status, .. } => {
            assert_eq!(status, "reconcile_required", "primary must mandate reconciliation on conflict");
        }
        other => panic!("expected StatusResponse, got {other:?}"),
    }

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
    let stream_id = pair_secondary(&mut store, &sec, primary_id);

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

    // 2. Upload initial batch before disconnect
    let gen1 = build_signed_upload(&sec, primary_id, &stream_id, 1, "claude", 5);
    let push1 = WsSyncFrame::SyncPush {
        batch_id: "batch-1".into(),
        envelope: gen1.envelope,
        signature: gen1.signature.to_vec(),
        gzip_bytes: gen1.gzip_bytes,
    };
    ws.send(tungstenite::Message::Text(serde_json::to_string(&push1).unwrap())).unwrap();
    let ack1 = ws.read().unwrap();
    assert!(matches!(
        serde_json::from_str::<WsSyncFrame>(ack1.to_text().unwrap()).unwrap(),
        WsSyncFrame::SyncAck { success: true, .. }
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
        stream_id: Some(stream_id.clone()),
        data_epoch: Some(1),
        expected_generation: Some(1),
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
    let gen2 = build_signed_upload(&sec, primary_id, &stream_id, 2, "claude", 2);
    let push2 = WsSyncFrame::SyncPush {
        batch_id: "batch-2".into(),
        envelope: gen2.envelope,
        signature: gen2.signature.to_vec(),
        gzip_bytes: gen2.gzip_bytes,
    };
    let _ = ws.send(tungstenite::Message::Text(serde_json::to_string(&push2).unwrap()));
    if let Ok(ack2) = ws.read() {
        if let Ok(frame) = serde_json::from_str::<WsSyncFrame>(ack2.to_text().unwrap()) {
            match frame {
                WsSyncFrame::SyncAck { success, error, .. } => {
                    assert!(!success, "push after revocation must not succeed");
                    assert!(error.is_some(), "push after revocation must produce error");
                }
                _ => {}
            }
        }
    }

    let _ = ws.close(None);
    server_stop.store(true, std::sync::atomic::Ordering::Release);
    assert!(server_handle.join().unwrap().is_ok());
}

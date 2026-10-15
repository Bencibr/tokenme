//! Focused probes for security and lifecycle primitives that the diagnostics
//! MCP design says it may reuse. These do not imply that a diagnostics MCP
//! endpoint or grant/session layer exists.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
    Arc,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use usage_device_sync::{
    hash_bytes, HttpsServerConfig, HttpsSyncServer, HttpsTransportConfig, SyncStore,
    Tls13HttpsTransport, TransportError,
};

struct CertificateFixture {
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
}

fn certificate() -> CertificateFixture {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let private_key_der = key.serialize_der();
    let params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    let certificate = params.self_signed(&key).unwrap();
    CertificateFixture {
        certificate_der: certificate.der().to_vec(),
        private_key_der,
    }
}

fn server_config(certificate: &CertificateFixture, staging_dir: PathBuf) -> HttpsServerConfig {
    HttpsServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        certificate_der: certificate.certificate_der.clone(),
        private_key_der: certificate.private_key_der.clone(),
        allow_lan: false,
        allowed_peer_ips: Vec::new(),
        max_body_bytes: 128 * 1024 * 1024,
        max_concurrent: 1,
        staging_dir,
        idle_timeout: Duration::from_secs(30),
        total_timeout: Duration::from_secs(600),
        staging_quota_bytes: 512 * 1024 * 1024,
    }
}

fn staging_dir(label: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("tokenme-diagnostics-mcp-{}-{label}-{suffix}", std::process::id()))
}

#[test]
fn tls_pin_and_response_limits_are_fail_closed() {
    let certificate = vec![1, 2, 3, 4];
    let mut config = HttpsTransportConfig::pinned(
        "https://127.0.0.1:1",
        "localhost",
        certificate.clone(),
    );

    assert_eq!(config.pinned_certificate_sha256, hash_bytes(&certificate));
    assert!(config.validate().is_ok());

    config.endpoint = "http://127.0.0.1:1".into();
    assert!(matches!(config.validate(), Err(TransportError::Config(message)) if message.contains("https://")));

    config.endpoint = "https://127.0.0.1:1".into();
    config.max_response_bytes = 0;
    assert!(matches!(config.validate(), Err(TransportError::Config(message)) if message.contains("max_response_bytes")));

    config.max_response_bytes = 256 * 1024 + 1;
    assert!(matches!(config.validate(), Err(TransportError::Config(message)) if message.contains("max_response_bytes")));

    config.max_response_bytes = 256 * 1024;
    config.trust_anchors_der.clear();
    config.pinned_certificate_sha256 = [0; 32];
    assert!(matches!(config.validate(), Err(TransportError::Config(message)) if message.contains("trust anchor")));
}

#[test]
fn wrong_leaf_pin_is_rejected_before_an_http_request() {
    let actual = certificate();
    let expected = certificate();
    let staging = staging_dir("pin");
    let server = HttpsSyncServer::bind(
        server_config(&actual, staging.clone()),
        SyncStore::open_in_memory("primary-1").unwrap(),
    )
    .unwrap();
    let address = server.local_addr().unwrap();
    let endpoint = format!("https://127.0.0.1:{}", address.port());
    let handle = thread::spawn(move || server.run_connections(1));

    let mut config = HttpsTransportConfig::pinned(
        endpoint.clone(),
        "localhost",
        expected.certificate_der,
    );
    // Trusting the actual certificate lets the handshake reach the explicit
    // leaf-pin check; the request must still fail before HTTP bytes exist.
    config.trust_anchors_der.push(actual.certificate_der);
    let mut client = Tls13HttpsTransport::new(config).unwrap();
    assert!(matches!(
        client.request_pairing_invite("primary-1", &endpoint),
        Err(TransportError::PinMismatch)
    ));
    assert!(handle.join().unwrap().is_ok());
    std::fs::remove_dir_all(staging).unwrap();
}

#[test]
fn stopping_the_existing_sync_listener_joins_the_service_loop() {
    let certificate = certificate();
    let staging = staging_dir("stop");
    let server = HttpsSyncServer::bind(
        server_config(&certificate, staging.clone()),
        SyncStore::open_in_memory("primary-1").unwrap(),
    )
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let (result_tx, result_rx) = mpsc::channel();
    let handle = thread::spawn(move || result_tx.send(server.run_until(&thread_stop)).unwrap());

    thread::sleep(Duration::from_millis(25));
    stop.store(true, Ordering::Release);
    assert!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
    handle.join().unwrap();
    std::fs::remove_dir_all(staging).unwrap();
}

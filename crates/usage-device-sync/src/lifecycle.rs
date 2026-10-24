//! Bounded notification of an already committed local disconnect.

use std::time::Duration;
use crate::{AuthenticatedStatusRequest, DeviceIdentity, HttpsTransportConfig, SecondarySyncConfig, Tls13HttpsTransport, TransportError};

pub fn deliver_disconnect_notice(config: &SecondarySyncConfig, now_ms: i64) -> Result<(), TransportError> {
    // Never generate a replacement key for a retired identity: it could not
    // authenticate the old binding and would hide a missing-key failure.
    if !config.identity_path.is_file() {
        return Err(TransportError::Config("disconnect identity file is unavailable".into()));
    }
    let identity = DeviceIdentity::load_or_create(&config.identity_path, &config.origin, &config.origin).map_err(TransportError::State)?;
    if identity.device_id != config.device_id || identity.origin != config.origin {
        return Err(TransportError::Config("disconnect identity does not match the retired binding".into()));
    }
    let request = AuthenticatedStatusRequest::new_disconnect_at(&identity, config.primary_id.clone(), now_ms).map_err(TransportError::State)?;
    let mut transport_config = HttpsTransportConfig::pinned(&config.endpoint, &config.server_name, config.certificate_der.clone());
    transport_config.pinned_certificate_sha256 = config.certificate_pin;
    transport_config.connect_timeout = Duration::from_secs(1);
    transport_config.request_timeout = Duration::from_secs(2);
    let mut transport = Tls13HttpsTransport::new(transport_config)?;
    match transport.disconnect(&request) {
        // The pinned primary no longer has this device, so the desired state
        // is already reached. Other auth failures must remain retryable.
        Err(TransportError::HttpStatus { status: 401, message }) if message == "device is not authorized" => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hash_bytes, DeviceStatus, HttpsServerConfig, HttpsSyncServer, PairingRequest, SyncStore};
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
    use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
    use std::thread::{self, JoinHandle};
    use std::time::Instant;

    fn test_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir
    }

    struct Server { stop: Arc<AtomicBool>, join: Option<JoinHandle<Result<(), TransportError>>> }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.join.take().unwrap().join().unwrap().unwrap();
        }
    }

    fn fixture(dir: &std::path::Path) -> (DeviceIdentity, SecondarySyncConfig, Vec<u8>) {
        let identity = DeviceIdentity::load_or_create(dir.join("identity.json"), "fixture-host", "Fixture").unwrap();
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = CertificateParams::new(vec!["localhost".into()]).unwrap().self_signed(&key).unwrap();
        let certificate_der = cert.der().to_vec();
        let config = SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: identity.device_id.clone(), origin: identity.origin.clone(),
            endpoint: "https://127.0.0.1:1".into(), server_name: "localhost".into(),
            certificate_pin: hash_bytes(&certificate_der), certificate_der, identity_path: dir.join("identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        };
        (identity, config, key.serialize_der())
    }

    fn serve(primary: SyncStore, config: &mut SecondarySyncConfig, key: Vec<u8>, dir: &std::path::Path) -> Server {
        let server = HttpsSyncServer::bind(HttpsServerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(), certificate_der: config.certificate_der.clone(), private_key_der: key,
            allow_lan: false, allowed_peer_ips: vec![], max_body_bytes: crate::MAX_COMPRESSED_BYTES, max_concurrent: 4,
            staging_dir: dir.join("staging"), idle_timeout: Duration::from_secs(2), total_timeout: Duration::from_secs(4),
            staging_quota_bytes: 512 * 1024 * 1024,
        }, primary).unwrap();
        config.endpoint = format!("https://{}", server.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = thread::spawn(move || server.run_until(&thread_stop));
        Server { stop, join: Some(join) }
    }

    fn request(primary: &mut SyncStore, identity: &DeviceIdentity, now: i64) -> String {
        let invite = primary.create_pairing_invite("https://localhost:47821", "11".repeat(32), now + 60_000).unwrap();
        let request = PairingRequest::new(&invite, identity, "test").unwrap();
        primary.receive_pairing_request(&request, now).unwrap().request_id
    }

    #[test]
    fn offline_disconnect_is_local_durable_and_retains_history_and_identity() {
        let dir = test_dir();
        let (identity, config, _) = fixture(dir.path());
        let path = dir.path().join("index.db");
        let mut secondary = SyncStore::open(&path, "primary-1").unwrap();
        secondary.save_secondary_config(&config, 1).unwrap();
        let started = Instant::now();
        secondary.request_secondary_disconnect(3).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(secondary.secondary_config().unwrap().is_none());
        assert!(config.identity_path.is_file());
        secondary.request_secondary_disconnect(4).unwrap(); // repeat stop is idempotent
        drop(secondary);
        let mut reopened = SyncStore::open(&path, "primary-1").unwrap();
        assert!(reopened.secondary_config().unwrap().is_none());
        assert_eq!(SyncStore::read_disconnect_notices(&path, 3).unwrap().len(), 1);
        assert_eq!(SyncStore::read_retained_identity_path(&path, "primary-1").unwrap(), Some(config.identity_path.clone()));
        reopened.finish_disconnect_notice(&identity.device_id, 4, Some("offline")).unwrap();
        assert!(SyncStore::read_disconnect_notices(&path, 4).unwrap().is_empty());
        let notice = SyncStore::read_disconnect_notices(&path, 15_004).unwrap().pop().unwrap();
        assert_eq!(notice.attempts, 1);
        reopened.rotate_primary_id("another-primary", 5).unwrap();
        assert_eq!(SyncStore::read_disconnect_notices(&path, i64::MAX).unwrap().len(), 1, "switching roles must not erase a pending remote revoke");
        reopened.finish_disconnect_notice(&identity.device_id, 15_004, None).unwrap();
        assert!(SyncStore::read_disconnect_notices(&path, i64::MAX).unwrap().is_empty());
    }

    #[test]
    fn tls_disconnect_ack_is_idempotent_and_repair_keeps_device_identity() {
        let dir = test_dir();
        let (identity, mut config, key) = fixture(dir.path());
        let path = dir.path().join("primary.db");
        let mut primary = SyncStore::open(&path, "primary-1").unwrap();
        let now = usage_core::report::now_ms();
        let id = request(&mut primary, &identity, now);
        primary.approve_pairing(&id, now).unwrap();
        let _server = serve(primary, &mut config, key, dir.path());
        deliver_disconnect_notice(&config, now).unwrap();
        deliver_disconnect_notice(&config, now).unwrap();
        let mut primary = SyncStore::open(&path, "primary-1").unwrap();
        assert_eq!(primary.device(&identity.device_id).unwrap().unwrap().status, DeviceStatus::Revoked);
        let id = request(&mut primary, &identity, now + 1);
        let paired = primary.approve_pairing(&id, now + 1).unwrap();
        assert_eq!(paired.device_id, config.device_id);
        assert_eq!(paired.data_epoch, 2);
        assert_eq!(paired.status, DeviceStatus::Paired);
    }

    #[test]
    fn missing_device_on_pinned_primary_is_already_disconnected_but_wrong_pin_is_not() {
        let dir = test_dir();
        let (_, mut config, key) = fixture(dir.path());
        let primary = SyncStore::open_in_memory("primary-1").unwrap();
        let _server = serve(primary, &mut config, key, dir.path());
        deliver_disconnect_notice(&config, usage_core::report::now_ms()).unwrap();
        config.certificate_pin = [9; 32];
        assert!(matches!(deliver_disconnect_notice(&config, usage_core::report::now_ms()), Err(TransportError::PinMismatch)));
    }

    #[test]
    fn missing_identity_never_creates_a_replacement_private_key() {
        let dir = test_dir();
        let (_, config, _) = fixture(dir.path());
        std::fs::remove_file(&config.identity_path).unwrap();
        assert!(deliver_disconnect_notice(&config, 1).is_err());
        assert!(!config.identity_path.exists());
    }

    #[test]
    fn occupied_listener_port_is_a_loud_configuration_error_not_a_silent_rebind() {
        let dir = test_dir();
        let (_, config, key) = fixture(dir.path());
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = occupied.local_addr().unwrap().port();
        let error = HttpsSyncServer::bind_with_host(HttpsServerConfig {
            bind_addr: occupied.local_addr().unwrap(), certificate_der: config.certificate_der, private_key_der: key,
            allow_lan: false, allowed_peer_ips: vec![], max_body_bytes: crate::MAX_COMPRESSED_BYTES, max_concurrent: 1,
            staging_dir: dir.path().join("staging"), idle_timeout: Duration::from_secs(2), total_timeout: Duration::from_secs(4),
            staging_quota_bytes: 512 * 1024 * 1024,
        }, SyncStore::open_in_memory("primary-1").unwrap(), crate::DiagnosticsHost::default())
            .err().expect("an occupied port must fail instead of silently moving");
        let TransportError::Config(message) = error else { panic!("expected a configuration error, got {error:?}") };
        assert!(message.contains("already in use"), "{message}");
        assert!(message.contains(&port.to_string()), "{message}");
    }

    #[test]
    fn endpoint_recovery_updates_only_the_transport_and_cannot_resurrect_a_stopped_binding() {
        let dir = test_dir();
        let (_, config, _) = fixture(dir.path());
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        store.save_secondary_config(&config, 1).unwrap();
        assert!(store.update_secondary_endpoint(&config.device_id, &config.endpoint, "https://127.0.0.1:47825", 3).unwrap());
        let next = store.secondary_config().unwrap().unwrap();
        assert_eq!(next.endpoint, "https://127.0.0.1:47825");
        assert_eq!(next.certificate_pin, config.certificate_pin);
        assert!(!store.update_secondary_endpoint(&config.device_id, &config.endpoint, "https://127.0.0.1:47826", 4).unwrap());
        store.request_secondary_disconnect(5).unwrap();
        assert!(!store.update_secondary_endpoint(&config.device_id, &next.endpoint, "https://127.0.0.1:47826", 6).unwrap());
        assert!(store.secondary_config().unwrap().is_none());
    }

    #[test]
    fn switching_a_stopped_primary_is_atomic_even_when_the_new_binding_write_fails() {
        let dir = test_dir();
        let (identity, mut config, _) = fixture(dir.path());
        let mut store = SyncStore::open_in_memory("primary-1").unwrap();
        store.save_primary_config(&crate::PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(), advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: false, share_data_with_secondaries: false,
        }, 1).unwrap();
        let request_id = request(&mut store, &identity, 2);
        store.approve_pairing(&request_id, 3).unwrap();
        config.primary_id = "primary-2".into();
        store.state.execute_batch("CREATE TRIGGER fail_secondary BEFORE INSERT ON device_sync_secondary_config BEGIN SELECT RAISE(ABORT, 'fixture write failure'); END;").unwrap();
        assert!(store.transition_to_secondary(&config, 4).is_err());
        assert_eq!(store.primary_id(), "primary-1");
        assert!(store.primary_config().unwrap().is_some());
        assert_eq!(store.device(&identity.device_id).unwrap().unwrap().status, DeviceStatus::Paired);
        store.state.execute_batch("DROP TRIGGER fail_secondary").unwrap();
        store.transition_to_secondary(&config, 5).unwrap();
        assert_eq!(store.primary_id(), "primary-2");
        assert!(store.primary_config().unwrap().is_none());
        assert_eq!(store.secondary_config().unwrap().unwrap().primary_id, "primary-2");
    }
}

//! Active primary discovery: the subnet sweep that reaches across routers.
//!
//! mDNS is link-local multicast — it cannot cross a subnet boundary, and a
//! multi-homed primary often advertises an address its peers cannot reach.
//! The sweep is the other half of discovery: enumerate every IPv4 subnet this
//! machine sits on, probe the sync port on each host, and identify a primary
//! through its unauthenticated `/v1/whoami` answer. The fields it returns
//! (primary id, readable hostname, certificate pin) are exactly what the
//! mDNS TXT record already broadcasts in cleartext, so the exposure class is
//! unchanged — only the transport differs.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::transport::WHOAMI_PATH;

/// One discovered primary: the whoami answer plus the address that answered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimaryWhoAmI {
    pub primary_id: String,
    pub display_name: String,
    pub certificate_pin: String,
    pub address: IpAddr,
    pub port: u16,
}

/// Host addresses to probe, derived from the machine's IPv4 interfaces.
/// Each non-loopback IPv4 with a prefix of `max_prefix` or wider (numerically
/// ≤) contributes its subnet's hosts minus the machine's own address, capped
/// per subnet; duplicates across interfaces collapse. IPv6 link-local needs
/// no sweep — mDNS already covers that link.
pub fn sweep_candidates(if_addrs: &[if_addrs::Interface], max_prefix: u8, max_per_subnet: usize) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for iface in if_addrs {
        let IpAddr::V4(v4) = iface.addr.ip() else { continue };
        if v4.is_loopback() || v4.is_unspecified() { continue }
        // if-addrs carries the netmask; derive the prefix length from it.
        let prefix = match &iface.addr {
            if_addrs::IfAddr::V4(v4) => u32::from(v4.netmask).count_ones() as u8,
            _ => continue,
        };
        // Narrower than /24 means an enterprise-sized subnet: the sweep would
        // hammer thousands of foreign hosts for one primary. /24 and narrower
        // (home LAN shape) sweep; wider ones stay mDNS/invite territory.
        if prefix < 24 || prefix > max_prefix { continue }
        let Some(net) = subnet_hosts(v4, prefix, max_per_subnet) else { continue };
        for host in net {
            if host == v4 { continue }
            if seen.insert(host) {
                out.push(IpAddr::V4(host));
            }
        }
    }
    out
}

/// The host addresses of one IPv4 subnet, network+broadcast excluded, capped
/// at `max` entries. `None` when the prefix is too wide to be worth sweeping.
fn subnet_hosts(ip: Ipv4Addr, prefix: u8, max: usize) -> Option<Vec<Ipv4Addr>> {
    if prefix > 32 { return None }
    let bits = u32::from(ip);
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    let network = bits & mask;
    let size = 1u64 << (32 - prefix as u64);
    let mut out = Vec::new();
    // Host range excludes network and broadcast; the cap truncates wide
    // prefixes to their first `max` hosts instead of skipping them.
    for host in 1..size.saturating_sub(1) {
        out.push(Ipv4Addr::from(network + host as u32));
        if out.len() >= max { break }
    }
    Some(out)
}

/// Identifies one primary: TLS with the server's self-signed certificate
/// accepted (its SHA-256 becomes the pin, the same value the mDNS TXT
/// "pin" property carries), then a `/v1/whoami` read. The first write drives
/// the TLS handshake, so no explicit handshake step exists.
pub fn probe_primary_whoami(address: IpAddr, port: u16, timeout: Duration) -> Result<PrimaryWhoAmI, String> {
    use std::io::{Read, Write};
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

    #[derive(Debug)]
    struct CaptureServerCert {
        captured: Mutex<Option<CertificateDer<'static>>>,
    }
    impl ServerCertVerifier for CaptureServerCert {
        fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, TlsError> {
            *self.captured.lock().unwrap_or_else(|p| p.into_inner()) = Some(end_entity.clone().into_owned());
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> { Ok(HandshakeSignatureValid::assertion()) }
        fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> { Ok(HandshakeSignatureValid::assertion()) }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![SignatureScheme::ED25519, SignatureScheme::RSA_PKCS1_SHA256, SignatureScheme::ECDSA_NISTP256_SHA256, SignatureScheme::RSA_PSS_SHA256]
        }
    }

    let addr = std::net::SocketAddr::new(address, port);
    let tcp = TcpStream::connect_timeout(&addr, timeout).map_err(|error| format!("connect: {error}"))?;
    tcp.set_read_timeout(Some(timeout)).map_err(|error| format!("read timeout: {error}"))?;
    tcp.set_write_timeout(Some(timeout)).map_err(|error| format!("write timeout: {error}"))?;

    let verifier = std::sync::Arc::new(CaptureServerCert { captured: Mutex::new(None) });
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    // The SNI name is decoration on a direct-IP probe; the verifier accepts
    // whatever certificate the socket presents.
    let server_name = ServerName::try_from("tokenme.local".to_string()).map_err(|error| format!("sni: {error}"))?;
    let connection = rustls::ClientConnection::new(std::sync::Arc::new(config), server_name)
        .map_err(|error| format!("tls: {error}"))?;
    let mut tls = rustls::StreamOwned::new(connection, tcp);
    // The wire-version header is the sync server's doorbell: a known path
    // without it is rejected before dispatch.
    let request = format!(
        "POST {WHOAMI_PATH} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: 0\r\nx-tokenme-wire-version: 2.0\r\nConnection: close\r\n\r\n"
    );
    tls.write_all(request.as_bytes()).map_err(|error| format!("write: {error}"))?;
    let mut body = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tls.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                body.extend_from_slice(&chunk[..n]);
                if body.len() > 64 * 1024 { break }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    if body.is_empty() { return Err("whoami: empty response".into()); }
    let text = String::from_utf8_lossy(&body);
    let payload = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let answer: WhoAmIWire = serde_json::from_str(payload.trim()).map_err(|error| format!("whoami: {error}"))?;
    let certificate_pin = match verifier.captured.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        Some(cert) => cert_sha256_hex(cert),
        None => answer.pin.clone(),
    };
    Ok(PrimaryWhoAmI {
        primary_id: answer.primary_id,
        display_name: answer.name,
        certificate_pin,
        address,
        port,
    })
}

#[derive(Deserialize)]
struct WhoAmIWire {
    primary_id: String,
    name: String,
    #[allow(dead_code)]
    pin: String,
}

fn cert_sha256_hex(cert: &rustls::pki_types::CertificateDer<'_>) -> String {
    // hash_bytes is the protocol's SHA-256; reuse it so the pin format is the
    // exact string the pairing flow pins against.
    crate::protocol::hash_bytes(cert.as_ref()).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Sweeps every IPv4 subnet the machine sits on for primaries listening on
/// `port`. One worker pool over the merged candidate list; per host the cost
/// is one bounded connect + (on success) one TLS handshake + one whoami.
/// A stop flag lets the caller end the sweep early.
pub fn sweep_local_primaries(
    port: u16,
    per_host_timeout: Duration,
    concurrency: usize,
    budget: Duration,
    stop: &std::sync::atomic::AtomicBool,
) -> Vec<PrimaryWhoAmI> {
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(list) => list,
        Err(_) => return Vec::new(),
    };
    let candidates = sweep_candidates(&ifaces, 24, 1024);
    if candidates.is_empty() { return Vec::new() }
    let next = AtomicUsize::new(0);
    let found: Mutex<BTreeMap<String, PrimaryWhoAmI>> = Mutex::new(BTreeMap::new());
    let deadline = std::time::Instant::now() + budget;
    let workers = concurrency.max(1).min(256);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if stop.load(std::sync::atomic::Ordering::Acquire) { return }
                if std::time::Instant::now() >= deadline { return }
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(address) = candidates.get(index) else { return };
                if let Ok(answer) = probe_primary_whoami(*address, port, per_host_timeout) {
                    if let Ok(mut map) = found.lock() {
                        map.entry(answer.primary_id.clone()).or_insert(answer);
                    }
                }
            });
        }
    });
    found.into_inner().unwrap_or_default().into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn iface(ip: [u8; 4], prefix: u8) -> if_addrs::Interface {
        if_addrs::Interface {
            name: "en0".into(),
            addr: if_addrs::IfAddr::V4(if_addrs::Ifv4Addr {
                ip: Ipv4Addr::from(ip),
                netmask: Ipv4Addr::from(if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) }),
                broadcast: None,
                prefixlen: prefix,
            }),
            index: None,
            // Windows-only field on this locked version; the sweep itself
            // never reads it.
            #[cfg(windows)]
            adapter_name: "wlo1".into(),
        }
    }

    #[test]
    fn sweep_candidates_enumerates_each_subnet_without_own_address() {
        let ifaces = vec![
            iface([10, 0, 0, 112], 24),
            iface([192, 168, 101, 66], 24),
        ];
        let out = sweep_candidates(&ifaces, 24, 1024);
        assert_eq!(out.len(), 506, "two /24 subnets minus network/broadcast/own: {}", out.len());
        assert!(!out.contains(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 112))));
        assert!(!out.contains(&IpAddr::V4(Ipv4Addr::new(192, 168, 101, 66))));
        assert!(out.contains(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(out.contains(&IpAddr::V4(Ipv4Addr::new(192, 168, 101, 1))));
    }

    #[test]
    fn sweep_candidates_dedupes_and_caps_and_skips_wide_prefixes() {
        let ifaces = vec![
            iface([10, 0, 0, 112], 24),
            // The same subnet twice (two names for one NIC): deduped.
            iface([10, 0, 0, 113], 24),
            // A /16 is not worth sweeping.
            iface([172, 26, 0, 1], 16),
        ];
        let out = sweep_candidates(&ifaces, 24, 1024);
        assert_eq!(out.len(), 254, "one subnet's worth after dedupe, /16 skipped");
        let capped = sweep_candidates(&[iface([10, 0, 0, 112], 24)], 24, 10);
        assert_eq!(capped.len(), 10, "the per-subnet cap bounds the sweep");
    }

    #[test]
    fn a_bound_server_answers_the_active_probe_with_its_pin() {
        use crate::protocol::DeviceIdentity;
        use crate::store::SyncStore;
        let device = DeviceIdentity::create("sweep-probe", "Probe").unwrap();
        let store = SyncStore::open_in_memory("primary-1").unwrap();
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let key_der = key.serialize_der();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap().self_signed(&key).unwrap();
        let certificate_der = cert.der().to_vec();
        let certificate_pin = crate::protocol::hash_bytes(&certificate_der);
        let config = crate::transport::HttpsServerConfig {
            bind_addr: std::net::SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            certificate_der: certificate_der.clone(),
            private_key_der: key_der,
            allow_lan: false, allowed_peer_ips: vec![],
            max_body_bytes: 64 * 1024 * 1024,
            max_concurrent: 2, staging_dir: std::env::temp_dir().join("sweep-probe-staging"),
            idle_timeout: Duration::from_secs(10), total_timeout: Duration::from_secs(60),
            staging_quota_bytes: 16 * 1024 * 1024,
        };
        let server = crate::transport::HttpsSyncServer::bind(config, store).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = std::thread::spawn(move || server.run_connections(1));
        let answer = probe_primary_whoami(addr.ip(), addr.port(), Duration::from_secs(3)).unwrap();
        assert_eq!(answer.primary_id, "primary-1");
        assert_eq!(answer.certificate_pin.len(), 64, "the leaf cert's sha256 is the pin");
        assert_eq!(answer.certificate_pin, certificate_pin.iter().map(|b| format!("{b:02x}")).collect::<String>());
        assert!(handle.join().unwrap().is_ok());
    }
}

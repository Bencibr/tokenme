//! One-shot JSON HTTP for the probes: short timeouts, no retries, `None` on any
//! non-2xx. A quota probe is never important enough to block a refresh cycle.

use std::time::Duration;

use serde_json::Value;

const CONNECT: Duration = Duration::from_secs(3);
const READ: Duration = Duration::from_secs(6);

/// Windows crash dumps showed a quota worker failing while evicting a TLS 1.3
/// ticket and dropping its certificate chain from ureq's process-wide cache.
/// One-shot probes do not need resumption; keep normal certificate validation
/// while avoiding that shared cache. Other platforms retain ureq's defaults.
pub(crate) fn agent_builder() -> ureq::AgentBuilder {
    let builder = ureq::AgentBuilder::new().middleware(crate::diagnostics::http_middleware);
    #[cfg(windows)]
    {
        use std::sync::{Arc, OnceLock};
        static TLS: OnceLock<Arc<ureq::rustls::ClientConfig>> = OnceLock::new();
        let config = TLS.get_or_init(|| {
            let roots = ureq::rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            Arc::new(quota_tls_config(roots))
        });
        builder.tls_config(config.clone())
    }
    #[cfg(not(windows))]
    { builder }
}

#[cfg(windows)]
fn quota_tls_config(roots: ureq::rustls::RootCertStore) -> ureq::rustls::ClientConfig {
    let mut config = ureq::rustls::ClientConfig::builder_with_provider(
        ureq::rustls::crypto::ring::default_provider().into(),
    ).with_safe_default_protocol_versions().expect("ring supports the default TLS versions")
        .with_root_certificates(roots).with_no_client_auth();
    config.resumption = ureq::rustls::client::Resumption::disabled();
    config
}

pub fn get_json(url: &str, headers: &[(&str, &str)]) -> Option<Value> {
    request(url, headers, None).and_then(|r| r.into_json().ok())
}

/// Like [`get_json`], but hands back non-2xx answers instead of dropping them.
/// Cline's plan endpoint answers a parseable `404 {"error":"no plan history…"}`
/// for an account without a subscription — a tier fact the probe needs, not a
/// failure. A body that will not parse is still `None`.
pub fn get_json_any_status(url: &str, headers: &[(&str, &str)]) -> Option<(u16, Value)> {
    let (status, resp) = request_any_status(url, headers)?;
    Some((status, resp.into_json().ok()?))
}

/// Used by the probes that must POST (Cline's token refresh, Antigravity's
/// local Connect RPC).
pub fn post_json(url: &str, headers: &[(&str, &str)], body: Value) -> Option<Value> {
    request(url, headers, Some(body)).and_then(|r| r.into_json().ok())
}

/// Form POST for OAuth exchanges (MiniMax Code's token endpoint): a non-2xx here
/// still carries a parseable `{"error":"invalid_grant",…}` the caller must read —
/// it is the difference between "rotate and write back" and "the login is dead,
/// touch nothing". Only a transport failure or an unparsable body is `None`.
pub fn post_form_any_status(
    url: &str,
    headers: &[(&str, &str)],
    form: &[(&str, &str)],
) -> Option<(u16, Value)> {
    post_form_any_status_read(url, headers, form, READ)
}

/// Same form POST with a caller-chosen read timeout: a token-refresh endpoint
/// can sit well past the 6 s probe budget, and a half-timeout refresh is
/// worse than none (the caller would give up mid-exchange).
pub fn post_form_any_status_read(
    url: &str,
    headers: &[(&str, &str)],
    form: &[(&str, &str)],
    read: std::time::Duration,
) -> Option<(u16, Value)> {
    let agent = agent_builder().timeout_connect(CONNECT).timeout_read(read).build();
    let mut req = agent.post(url);
    for (name, value) in headers {
        req = req.set(name, value);
    }
    let resp = match req.send_form(form) {
        Ok(resp) => resp,
        Err(ureq::Error::Status(_, resp)) => resp,
        Err(_) => return None,
    };
    let status = resp.status();
    Some((status, resp.into_json().ok()?))
}

fn request(url: &str, headers: &[(&str, &str)], body: Option<Value>) -> Option<ureq::Response> {
    let resp = send(url, headers, body)?;
    if !(200u16..300).contains(&resp.status()) {
        return None;
    }
    Some(resp)
}

fn request_any_status(url: &str, headers: &[(&str, &str)]) -> Option<(u16, ureq::Response)> {
    let resp = send(url, headers, None)?;
    let status = resp.status();
    Some((status, resp))
}

fn send(url: &str, headers: &[(&str, &str)], body: Option<Value>) -> Option<ureq::Response> {
    let agent = agent_builder().timeout_connect(CONNECT).timeout_read(READ).build();
    let mut req = match body {
        Some(_) => agent.post(url),
        None => agent.get(url),
    };
    for (name, value) in headers {
        req = req.set(name, value);
    }
    match body {
        Some(body) => req.send_json(body).ok(),
        // ureq surfaces non-2xx as `Err`, but a status error still carries the
        // response — and a 404 body is exactly what `get_json_any_status` is
        // here to read.
        None => match req.call() {
            Ok(resp) => Some(resp),
            Err(ureq::Error::Status(_, resp)) => Some(resp),
            Err(_) => None,
        },
    }
}

#[cfg(all(test, windows))]
mod tls_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;
    use ureq::rustls::{self, HandshakeKind};

    fn server(count: usize) -> (u16, rustls::RootCertStore, thread::JoinHandle<Vec<Option<HandshakeKind>>>) {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = key.cert.der().clone();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS13]).unwrap().with_no_client_auth()
            .with_single_cert(vec![cert], rustls::pki_types::PrivatePkcs8KeyDer::from(key.signing_key.serialize_der()).into()).unwrap();
        // More than the client's usual per-server ticket capacity: the old
        // configuration repeatedly evicts cached certificate chains here.
        config.send_tls13_tickets = 16;
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let task = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut workers = Vec::new();
            while workers.len() < count && Instant::now() < deadline {
                let (socket, _) = match listener.accept() {
                    Ok(socket) => socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => { thread::sleep(Duration::from_millis(2)); continue },
                    Err(e) => panic!("TLS fixture accept: {e}"),
                };
                // Winsock inherits the listening socket's nonblocking mode.
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
                let config = config.clone();
                workers.push(thread::spawn(move || {
                let connection = rustls::ServerConnection::new(config.clone()).unwrap();
                let mut stream = rustls::StreamOwned::new(connection, socket);
                let received = (|| -> std::io::Result<()> {
                    let mut reader = BufReader::new(&mut stream);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line)? == 0 || line == "\r\n" { break }
                    }
                    Ok(())
                })();
                if received.is_err() { return None }
                let kind = stream.conn.handshake_kind();
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
                stream.flush().unwrap();
                // ureq closes after reading Content-Length. A fast release
                // client may already be gone when the close alert is flushed.
                stream.conn.send_close_notify();
                let _ = stream.flush();
                let _ = stream.sock.shutdown(std::net::Shutdown::Write);
                kind
                }));
            }
            workers.into_iter().map(|worker| worker.join().unwrap()).collect()
        });
        (port, roots, task)
    }

    #[test]
    fn repeated_ticket_heavy_tls_probes_use_full_handshakes() {
        let (port, roots, server) = server(64);
        let config = Arc::new(quota_tls_config(roots));
        let mut clients = Vec::new();
        for _ in 0..4 {
            let config = config.clone();
            clients.push(thread::spawn(move || {
                for _ in 0..16 {
                    let agent = agent_builder().tls_config(config.clone())
                        .resolver(move |_: &str| Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))]))
                        .timeout_connect(CONNECT).timeout_read(READ).build();
                    let response: Value = agent.get(&format!("https://localhost:{port}/quota")).call().unwrap().into_json().unwrap();
                    assert_eq!(response, serde_json::json!({"ok": true}));
                }
            }));
        }
        for client in clients { client.join().unwrap(); }
        let handshakes = server.join().unwrap();
        assert_eq!(handshakes.len(), 64);
        assert!(handshakes.iter().all(|kind| *kind == Some(HandshakeKind::Full)), "no probe resumes a cached TLS session");
    }

    #[test]
    fn quota_tls_still_rejects_untrusted_certificates_and_wrong_hosts() {
        for untrusted in [true, false] {
            let (port, roots, server) = server(1);
            let roots = if untrusted { rustls::RootCertStore::empty() } else { roots };
            let host = if untrusted { "localhost" } else { "127.0.0.1" };
            let agent = agent_builder().tls_config(Arc::new(quota_tls_config(roots)))
                .resolver(move |_: &str| Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))]))
                .timeout_connect(CONNECT).timeout_read(READ).build();
            let error = agent.get(&format!("https://{host}:{port}/quota")).call().unwrap_err();
            let message = error.to_string();
            assert!(message.contains("certificate"), "TLS must reject the certificate: {message}");
            assert_eq!(server.join().unwrap(), vec![None]);
        }
    }
}

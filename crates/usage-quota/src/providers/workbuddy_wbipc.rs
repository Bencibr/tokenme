//! WorkBuddy billing through the running desktop app's wbipc broker — the
//! zero-interaction credential.
//!
//! The app ≥ 2.48 encrypts its stored access token so it cannot be extracted
//! (at-rest key injected at runtime, never on disk), and asking the user to
//! log in again is a non-starter. But the app also exposes a broker over a
//! 0600 unix socket — `~/.workbuddy-ai/wbipc/endpoint.json`, documented in the
//! shipped `main/server.js` as the contract entry for CLI clients — whose one
//! pipe (`wb.request/http.fetch`) is a generic HTTP proxy that injects the
//! host's own `Authorization`/`X-User-Id` headers. The caller picks only a
//! relative path on the host's backend origin, so this stays read-only against
//! the user's own account.
//!
//! Wire protocol (reconstructed from the shipped `wbipc/protocol.ts` +
//! `broker-server.ts` and verified live 2026-09-29): newline-delimited JSON
//! frames ≤ 1 MiB; `session_hello` → `session_challenge` → `session_prove` →
//! `session_hello_ack` with HMAC-SHA256 proofs over a length-prefixed
//! transcript; then JSON-RPC 2.0 frames carrying `mode: "call"`. The broker may
//! demand a one-time consent (`E_CONSENT_REQUIRED`); none was observed for a
//! `kind: "cli"` client.
//!
//! Everything here returns `None` on any failure — a probe that finds the app
//! closed, logged out, or speaking a different protocol is silence, and the
//! provider falls back to its explicit-token chain. Both platforms speak the
//! same wire: the endpoint string names a unix socket path on macOS/Linux and
//! a `\\.\pipe\` path on Windows, which opens as a read-write byte stream.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const PROTOCOL: u32 = 1;
const PIPE: &str = "wb.request";
const FETCH_METHOD: &str = "http.fetch";
const BROKER_GETPIPE: &str = "broker/GetPipe";
const MAX_FRAME: usize = 1_048_576;

/// `{endpoint, ticket}` discovery file — the broker's one public contract.
/// Same ladder as the adapter's `paths::config_root`.
fn discovery_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("WORKBUDDY_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("wbipc").join("endpoint.json"));
    }
    dirs::home_dir().map(|h| h.join(".workbuddy-ai").join("wbipc").join("endpoint.json"))
}

use crate::providers::hmac_sha256;

/// Length-prefixed concat — without it `a|bc` and `ab|c` share one transcript.
fn transcript(role: &str, endpoint: &str, client_nonce: &str, server_nonce: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for part in [role, &PROTOCOL.to_string(), endpoint, client_nonce, server_nonce] {
        let buf = part.as_bytes();
        out.extend_from_slice(&(buf.len() as u32).to_be_bytes());
        out.extend_from_slice(buf);
    }
    out
}

fn proof(ticket: &str, role: &str, endpoint: &str, client_nonce: &str, server_nonce: &str) -> String {
    let mac = hmac_sha256(ticket.as_bytes(), &transcript(role, endpoint, client_nonce, server_nonce));
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac)
}

fn ticket_id(ticket: &str) -> String {
    let hex: String = Sha256::digest(ticket.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    hex[..16].to_string()
}

/// One meter call through the app. `body` is the vendor envelope the direct
/// token chain would have POSTed; the answer is the same envelope, parsed.
pub(crate) fn meter_envelope(body: &Value) -> Option<Value> {
    let disc = std::fs::read_to_string(discovery_path()?).ok()?;
    let disc: Value = serde_json::from_str(&disc).ok()?;
    let endpoint = disc.get("endpoint")?.as_str()?.to_string();
    let ticket = disc.get("ticket")?.as_str()?.to_string();

    let mut conn = Connection {
        stream: BufReader::new(connect(&endpoint)?),
        next_id: 1,
    };
    let (channel, methods) = conn.handshake(&endpoint, &ticket)?;
    if !methods.iter().any(|m| m == FETCH_METHOD) {
        return None;
    }
    let result = conn.call(
        &format!("{channel}/{FETCH_METHOD}"),
        json!({
            "method": "POST",
            "path": "/billing/meter/get-user-resource",
            "headers": {"content-type": "application/json"},
            "body_b64": base64::engine::general_purpose::STANDARD.encode(body.to_string()),
        }),
    )?;
    let status = result.get("status")?.as_u64()?;
    if !(200..300).contains(&status) {
        return None;
    }
    let body_b64 = result.get("body_b64")?.as_str()?;
    let raw = base64::engine::general_purpose::STANDARD.decode(body_b64).ok()?;
    serde_json::from_slice(&raw).ok()
}

struct Connection {
    stream: BufReader<Stream>,
    next_id: u64,
}

/// The broker endpoint, per platform: an AF_UNIX socket path on macOS/Linux,
/// a `\\.\pipe\` named pipe on Windows — which opens as a read-write byte
/// stream, so both sides speak the same NDJSON frames over `Read + Write`.
enum Stream {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    #[cfg(windows)]
    Pipe(std::fs::File),
}

fn connect(endpoint: &str) -> Option<Stream> {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(endpoint).ok().map(Stream::Unix)
    }
    #[cfg(windows)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(endpoint)
            .ok()
            .map(Stream::Pipe)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.read(buf),
            #[cfg(windows)]
            Stream::Pipe(f) => f.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.write(buf),
            #[cfg(windows)]
            Stream::Pipe(f) => f.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.flush(),
            #[cfg(windows)]
            Stream::Pipe(f) => f.flush(),
        }
    }
}

impl Connection {
    fn send(&mut self, frame: &Value) -> Option<()> {
        let mut line = frame.to_string().into_bytes();
        line.push(b'\n');
        self.stream.get_mut().write_all(&line).ok()
    }

    /// One NDJSON frame, capped like the broker's own reader.
    fn read_frame(&mut self) -> Option<Value> {
        let mut line = Vec::new();
        let n = self.stream.read_until(b'\n', &mut line).ok()?;
        if n == 0 || line.len() > MAX_FRAME {
            return None;
        }
        serde_json::from_slice(&line).ok()
    }

    /// hello → challenge (the server must prove it knows the ticket) →
    /// prove → ack. The ack's pipe list doubles as a login check: an
    /// un-logged-in host keeps `wb.request` invisible on purpose.
    fn handshake(&mut self, endpoint: &str, ticket: &str) -> Option<(String, Vec<String>)> {
        let client_nonce = format!(
            "{:016x}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as u64
        );
        self.send(&json!({
            "type": "session_hello",
            "protocol_min": PROTOCOL,
            "protocol_max": PROTOCOL,
            "client_nonce": client_nonce,
            "ticket_id": ticket_id(ticket),
            "client": {"kind": "cli", "id": "tokenme", "version": env!("CARGO_PKG_VERSION")},
        }))?;
        let challenge = self.read_frame()?;
        if challenge.get("type")?.as_str()? != "session_challenge" {
            return None;
        }
        let server_nonce = challenge.get("server_nonce")?.as_str()?.to_string();
        let server_proof = challenge.get("server_proof")?.as_str()?;
        if server_proof != proof(ticket, "wbipc-s", endpoint, &client_nonce, &server_nonce) {
            return None;
        }
        self.send(&json!({
            "type": "session_prove",
            "client_proof": proof(ticket, "wbipc-c", endpoint, &client_nonce, &server_nonce),
        }))?;
        let ack = self.read_frame()?;
        if ack.get("type")?.as_str()? != "session_hello_ack" {
            return None;
        }
        let pipes: Vec<String> = ack
            .get("pipes")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if !pipes.iter().any(|p| p == PIPE) {
            return None;
        }
        let result = self.call(BROKER_GETPIPE, json!({"pipe": PIPE}))?;
        let channel = result.get("channel")?.as_str()?.to_string();
        let methods = result
            .get("methods")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        Some((channel, methods))
    }

    fn call(&mut self, method: &str, params: Value) -> Option<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "mode": "call", "params": params}))?;
        loop {
            let frame = self.read_frame()?;
            if frame.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return match frame.get("error") {
                Some(e) if !e.is_null() => None,
                _ => frame.get("result").cloned(),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vectors computed independently (python hashlib/hmac) for
    /// ticket "test-ticket", endpoint "/tmp/t.sock", nonces "client-nonce"
    /// and "server-nonce". Pins the transcript framing, the HMAC and the
    /// unpadded-base64url encoding against protocol drift.
    const TICKET: &str = "test-ticket";
    const ENDPOINT: &str = "/tmp/t.sock";
    const CLIENT_NONCE: &str = "client-nonce";
    const SERVER_NONCE: &str = "server-nonce";
    const TICKET_ID: &str = "46d48dfb2504812d";
    const SERVER_PROOF: &str = "8rwp6vvPJ6LUMllMh3bbr-ekUl5h81e3_ShEFm7s5ko";
    const CLIENT_PROOF: &str = "zlFvYGxHkoUK5S3_tkmEi2N74xrfcDXp2z_XAdw1SkY";

    #[test]
    fn handshake_proofs_match_the_reference_implementation() {
        assert_eq!(ticket_id(TICKET), TICKET_ID);
        assert_eq!(proof(TICKET, "wbipc-s", ENDPOINT, CLIENT_NONCE, SERVER_NONCE), SERVER_PROOF);
        assert_eq!(proof(TICKET, "wbipc-c", ENDPOINT, CLIENT_NONCE, SERVER_NONCE), CLIENT_PROOF);
    }

    #[test]
    fn the_transcript_is_length_prefixed_not_concatenated() {
        // "ab"+"c" must differ from "a"+"bc" — the framing exists exactly to
        // keep those apart.
        assert_ne!(transcript("wbipc-c", "/x", "ab", "c"), transcript("wbipc-c", "/x", "a", "bc"));
    }
}

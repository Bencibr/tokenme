//! Minimal, loopback-only OTLP/HTTP JSON receiver for Claude Code TTFT.
//!
//! The app owns lifecycle/configuration and exposes only a loopback port. This
//! module must never bind to an unspecified address. Request bodies are bounded
//! and parsed through an allowlist; request and response content are not stored.

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const OTLP_TRACES_PATH: &str = "/v1/traces";
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_SAMPLES_PER_EXPORT: usize = 100;
pub const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_METRIC_MS: f64 = 10.0 * 60.0 * 1000.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricField {
    TtftMs,
    FirstContentMs,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TtftSample {
    pub field: MetricField,
    pub millis: f64,
    /// Local receipt time, Unix epoch milliseconds.
    pub sampled_at_ms: u64,
    /// Model label only; no request identifiers or prompt/response fields.
    pub model: Option<String>,
}

/// Binds strictly to IPv4 loopback. The caller owns and must lifecycle-manage
/// the listener; this function never binds to a LAN or wildcard interface.
pub fn bind_loopback(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
}

/// Runs the receiver on a managed background thread. `shutdown` closes it and
/// waits for the worker to exit. It accepts one bounded POST at a time; callers
/// should keep this opt-in and configure the Claude exporter to this port.
pub fn start_server<F>(port: u16, callback: F) -> io::Result<ServerHandle>
where
    F: FnMut(Vec<TtftSample>) + Send + 'static,
{
    let listener = bind_loopback(port)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_stopped = Arc::clone(&stopped);
    let worker = thread::Builder::new()
        .name("claude-otel-ttft".into())
        .spawn(move || serve(listener, worker_stopped, callback))?;
    Ok(ServerHandle { local_addr, stopped, worker: Some(worker) })
}

pub struct ServerHandle {
    local_addr: SocketAddr,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr { self.local_addr }

    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) { self.stop_and_join(); }
}

fn serve<F>(listener: TcpListener, stopped: Arc<AtomicBool>, mut callback: F)
where
    F: FnMut(Vec<TtftSample>),
{
    while !stopped.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer)) if peer.ip().is_loopback() => {
                let _ = handle_connection(stream, &mut callback);
            }
            Ok((mut stream, _)) => { let _ = write_response(&mut stream, 403, "Forbidden", "{}"); }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(20)),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Handles one accepted connection. OTLP receiver has no authentication, so a
/// local process can spoof samples; product integration must retain opt-in and
/// should choose an unpredictable per-process port where feasible.
pub fn handle_connection<F>(mut stream: TcpStream, mut on_sample: F) -> io::Result<()>
where
    F: FnMut(Vec<TtftSample>),
{
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(request_line) = read_bounded_line(&mut reader, 1024)? else {
        return write_response(&mut stream, 400, "Bad Request", "{}");
    };
    let request_line = trim_crlf(&request_line);
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        return write_response(&mut stream, 400, "Bad Request", "{}");
    }
    if method != "POST" || path != OTLP_TRACES_PATH {
        return write_response(&mut stream, 404, "Not Found", "{}");
    }

    let mut header_bytes = request_line.len() + 2;
    let mut content_length = None;
    let mut content_type_ok = false;
    let mut transfer_encoding = false;
    loop {
        let Some(line) = read_bounded_line(&mut reader, MAX_HEADER_BYTES - header_bytes)? else {
            return write_response(&mut stream, 400, "Bad Request", "{}");
        };
        let read = line.len();
        header_bytes = header_bytes.saturating_add(read);
        if header_bytes > MAX_HEADER_BYTES {
            return write_response(&mut stream, 431, "Request Header Fields Too Large", "{}");
        }
        let line = trim_crlf(&line);
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return write_response(&mut stream, 400, "Bad Request", "{}");
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return write_response(&mut stream, 400, "Bad Request", "{}");
            }
            content_length = value.parse::<usize>().ok();
            if content_length.is_none() {
                return write_response(&mut stream, 400, "Bad Request", "{}");
            }
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type_ok = value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"));
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            transfer_encoding = true;
        }
    }
    if transfer_encoding {
        return write_response(&mut stream, 400, "Bad Request", "{}");
    }
    let Some(length) = content_length else {
        return write_response(&mut stream, 411, "Length Required", "{}");
    };
    if length > MAX_BODY_BYTES {
        return write_response(&mut stream, 413, "Payload Too Large", "{}");
    }
    if !content_type_ok {
        return write_response(&mut stream, 415, "Unsupported Media Type", "{}");
    }
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return write_response(&mut stream, 400, "Bad Request", "{}");
    }

    // Parse only the OTLP trace envelope and the two approved numeric
    // attributes. Unknown fields are skipped by Serde rather than retained.
    let parsed = serde_json::from_slice::<OtlpExport>(&body);
    drop(body);
    if let Ok(export) = parsed {
        let mut samples = Vec::new();
        for resource in export.resource_spans {
            for scope in resource.scope_spans {
                for span in scope.spans {
                    if span.name != "claude_code.llm_request" {
                        continue;
                    }
                    let mut ttft = None;
                    let mut first_content = None;
                    let mut model = None;
                    let mut duplicate = false;
                    let (mut saw_ttft, mut saw_first_content, mut saw_model) = (false, false, false);
                    for attribute in span.attributes {
                        let (seen, slot) = match attribute.key.as_str() {
                            "ttft_ms" => (&mut saw_ttft, &mut ttft),
                            "first_content_ms" => (&mut saw_first_content, &mut first_content),
                            "gen_ai.request.model" => {
                                if saw_model { duplicate = true; break; }
                                saw_model = true;
                                model = attribute.text;
                                continue;
                            }
                            _ => continue,
                        };
                        if *seen {
                            duplicate = true;
                            break;
                        }
                        *seen = true;
                        *slot = attribute.number;
                    }
                    if duplicate {
                        continue;
                    }
                    let sampled_at_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        .min(u64::MAX as u128) as u64;
                    // Prefer the explicit TTFT attribute. `first_content_ms`
                    // is a fallback with a related but distinct definition; do
                    // not emit both for one request or mix them in medians.
                    if let Some(millis) = ttft {
                        samples.push(TtftSample { field: MetricField::TtftMs, millis, sampled_at_ms, model });
                    } else if let Some(millis) = first_content {
                        samples.push(TtftSample { field: MetricField::FirstContentMs, millis, sampled_at_ms, model });
                    }
                    if samples.len() >= MAX_SAMPLES_PER_EXPORT { break; }
                }
                if samples.len() >= MAX_SAMPLES_PER_EXPORT { break; }
            }
            if samples.len() >= MAX_SAMPLES_PER_EXPORT { break; }
        }
        if !samples.is_empty() { on_sample(samples); }
        write_response(&mut stream, 200, "OK", "{}")
    } else {
        write_response(&mut stream, 400, "Bad Request", "{}")
    }
}

fn trim_crlf(bytes: &[u8]) -> &str {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    std::str::from_utf8(bytes).unwrap_or("")
}

fn read_bounded_line<R: BufRead>(reader: &mut R, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() { Ok(None) } else { Ok(Some(line)) };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        if count > limit.saturating_sub(line.len()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "HTTP line too long"));
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

fn write_response(stream: &mut TcpStream, code: u16, reason: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    stream.flush()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OtlpExport {
    #[serde(default)]
    resource_spans: Vec<ResourceSpans>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceSpans {
    #[serde(default)]
    scope_spans: Vec<ScopeSpans>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScopeSpans {
    #[serde(default)]
    spans: Vec<Span>,
}

#[derive(Deserialize)]
struct Span {
    #[serde(default)]
    name: String,
    #[serde(default)]
    attributes: Vec<Attribute>,
}

struct Attribute {
    key: String,
    number: Option<f64>,
    text: Option<String>,
}

impl<'de> Deserialize<'de> for Attribute {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        struct AttributeVisitor;
        impl<'de> Visitor<'de> for AttributeVisitor {
            type Value = Attribute;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("an OTLP key/value attribute")
            }
            fn visit_map<M>(self, mut map: M) -> Result<Attribute, M::Error>
            where M: MapAccess<'de> {
                let mut key: Option<String> = None;
                let mut raw_value = None;
                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "key" => {
                            if key.is_some() { return Err(serde::de::Error::duplicate_field("key")); }
                            key = Some(map.next_value()?);
                        }
                        "value" => {
                            if raw_value.is_some() { return Err(serde::de::Error::duplicate_field("value")); }
                            // JSON object member order is not significant. Keep
                            // this bounded value only until its key is known,
                            // then extract an allowlisted scalar and drop it.
                            raw_value = Some(map.next_value::<serde_json::Value>()?);
                        }
                        _ => { let _: IgnoredAny = map.next_value()?; }
                    }
                }
                let key = key.unwrap_or_default();
                let value = raw_value.ok_or_else(|| serde::de::Error::missing_field("value"))?;
                let mut number = None;
                let mut text = None;
                match key.as_str() {
                    "ttft_ms" | "first_content_ms" => {
                        number = serde_json::from_value::<AttributeValue>(value).ok().and_then(|value| value.number());
                    }
                    "gen_ai.request.model" => {
                        text = serde_json::from_value::<AttributeValue>(value).ok().and_then(|value| value.string_value)
                            .filter(|s| s.len() <= 128 && !s.chars().any(char::is_control));
                    }
                    _ => {}
                }
                Ok(Attribute { key, number, text })
            }
        }
        deserializer.deserialize_map(AttributeVisitor)
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttributeValue {
    int_value: Option<String>,
    double_value: Option<f64>,
    string_value: Option<String>,
}

impl AttributeValue {
    fn number(&self) -> Option<f64> {
        let number = self
            .double_value
            .or_else(|| self.int_value.as_deref()?.parse::<f64>().ok())
            .or_else(|| self.string_value.as_deref()?.parse::<f64>().ok())?;
        (number.is_finite() && (0.0..=MAX_METRIC_MS).contains(&number)).then_some(number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_allowed_metrics_from_llm_request_spans() {
        let body = br#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"claude-code"}}]},"scopeSpans":[{"spans":[{"name":"claude_code.llm_request","attributes":[{"key":"ttft_ms","value":{"intValue":"137"}},{"key":"first_content_ms","value":{"doubleValue":139.5}},{"key":"prompt","value":{"stringValue":"must not be retained"}}]},{"name":"other.span","attributes":[{"key":"ttft_ms","value":{"intValue":"1"}}]}]}]}]}"#;
        let parsed: OtlpExport = serde_json::from_slice(body).unwrap();
        let span = &parsed.resource_spans[0].scope_spans[0].spans[0];
        assert_eq!(span.name, "claude_code.llm_request");
        assert_eq!(span.attributes[0].key, "ttft_ms");
        assert_eq!(span.attributes[0].number, Some(137.0));
        assert_eq!(span.attributes[1].number, Some(139.5));
        assert_eq!(span.attributes[2].key, "prompt");
        assert_eq!(span.attributes[2].text, None);
    }

    #[test]
    fn attribute_key_value_order_does_not_change_extraction() {
        let body = br#"{"resourceSpans":[{"scopeSpans":[{"spans":[{"name":"claude_code.llm_request","attributes":[{"value":{"intValue":"42"},"key":"ttft_ms"},{"value":{"stringValue":"claude-opus"},"key":"gen_ai.request.model"}]}]}]}]}"#;
        let parsed: OtlpExport = serde_json::from_slice(body).unwrap();
        let attributes = &parsed.resource_spans[0].scope_spans[0].spans[0].attributes;
        assert_eq!(attributes[0].number, Some(42.0));
        assert_eq!(attributes[1].text.as_deref(), Some("claude-opus"));
    }

    #[test]
    fn rejects_negative_non_finite_and_implausible_values() {
        for raw in ["-1", "600001", "\"NaN\"", "\"nope\""] {
            let value: AttributeValue = serde_json::from_str(&format!("{{\"doubleValue\":{raw}}}")).unwrap_or_default();
            assert_eq!(value.number(), None, "{raw}");
        }
    }

    #[test]
    fn listener_is_loopback_only() {
        let listener = bind_loopback(0).unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }

    #[test]
    fn server_accepts_bounded_otlp_post_and_emits_only_metric_metadata() {
        let (tx, rx) = std::sync::mpsc::channel();
        let server = start_server(0, move |samples| { let _ = tx.send(samples); }).unwrap();
        let mut client = TcpStream::connect(server.local_addr()).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let body = br#"{"resourceSpans":[{"scopeSpans":[{"spans":[{"name":"claude_code.llm_request","attributes":[{"key":"ttft_ms","value":{"intValue":"137"}},{"key":"first_content_ms","value":{"intValue":"139"}},{"key":"gen_ai.request.model","value":{"stringValue":"claude-sonnet"}},{"key":"prompt","value":{"stringValue":"private user content"}}]}]}]}]}"#;
        write!(client, "POST /v1/traces HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        client.write_all(body).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let samples = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(samples.len(), 1);
        let sample = &samples[0];
        assert_eq!(sample.field, MetricField::TtftMs);
        assert_eq!(sample.millis, 137.0);
        assert!(rx.try_recv().is_err(), "one export produces one callback batch");
        assert_eq!(sample.model.as_deref(), Some("claude-sonnet"));
        assert!(sample.sampled_at_ms > 0);
        server.shutdown();
    }
}

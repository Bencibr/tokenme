//! Local metadata-only quota tracing. A native abort leaves a begin event
//! without a matching response/probe end; WER supplies the native stack.
//! Never pass headers, bodies, URL paths/query strings, or error Display text.

use std::cell::Cell;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, serde::Serialize)]
pub struct Event {
    pub schema: u8,
    pub run_id: String,
    pub timestamp_ms: u64,
    pub pid: u32,
    pub thread: String,
    pub thread_name: Option<String>,
    pub os_thread_id: Option<u32>,
    pub provider: &'static str,
    pub probe_id: Option<u64>,
    pub request_id: Option<u64>,
    pub phase: &'static str,
    pub outcome: Option<&'static str>,
    pub method: Option<&'static str>,
    pub host: Option<String>,
    pub status: Option<u16>,
    pub transport_kind: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub samples: Option<usize>,
    pub probes_in_flight: usize,
    pub requests_in_flight: usize,
}

pub type Sink = Arc<dyn Fn(&Event) + Send + Sync>;
static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PROBES: AtomicUsize = AtomicUsize::new(0);
static REQUESTS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct Context { provider: &'static str, probe_id: Option<u64> }
thread_local! { static CONTEXT: Cell<Context> = const { Cell::new(Context { provider: "unknown", probe_id: None }) }; }

/// Installing a sink does not perform any network operation. It must not
/// contain credentials; callers receive only the typed metadata above.
pub fn set_sink(sink: Option<Sink>) { *SINK.lock().unwrap_or_else(|e| e.into_inner()) = sink; }

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64 }

fn event(phase: &'static str) -> Event {
    static RUN: OnceLock<String> = OnceLock::new();
    let context = CONTEXT.with(Cell::get);
    #[cfg(windows)]
    let os_thread_id = Some(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() });
    #[cfg(not(windows))]
    let os_thread_id = None;
    Event {
        schema: 1, run_id: RUN.get_or_init(|| format!("{}-{}", std::process::id(), now_ms())).clone(),
        timestamp_ms: now_ms(), pid: std::process::id(), thread: format!("{:?}", std::thread::current().id()),
        thread_name: std::thread::current().name().map(str::to_string), os_thread_id,
        provider: context.provider, probe_id: context.probe_id, request_id: None, phase,
        outcome: None, method: None, host: None, status: None, transport_kind: None, elapsed_ms: None, samples: None,
        probes_in_flight: PROBES.load(Ordering::Relaxed), requests_in_flight: REQUESTS.load(Ordering::Relaxed),
    }
}

fn emit(event: &Event) {
    let sink = SINK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(sink) = sink {
        // Diagnostic failures cannot turn a successful probe into a failure.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(event)));
    }
}

pub(crate) fn note(provider: &'static str, phase: &'static str, outcome: &'static str) {
    let mut entry = event(phase); entry.provider = provider; entry.outcome = Some(outcome); emit(&entry);
}

pub(crate) struct ProbeTrace {
    previous: Context,
    started: Instant,
    finished: bool,
}

impl ProbeTrace {
    pub(crate) fn start(provider: &'static str) -> Self {
        let id = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let previous = CONTEXT.with(|slot| slot.replace(Context { provider, probe_id: Some(id) }));
        PROBES.fetch_add(1, Ordering::Relaxed);
        emit(&event("probe_begin"));
        Self { previous, started: Instant::now(), finished: false }
    }

    pub(crate) fn finish(&mut self, outcome: &'static str, samples: Option<usize>) {
        if self.finished { return }
        self.finished = true;
        PROBES.fetch_sub(1, Ordering::Relaxed);
        let mut entry = event("probe_end");
        entry.outcome = Some(outcome); entry.samples = samples;
        entry.elapsed_ms = Some(self.started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        emit(&entry);
    }
}

impl Drop for ProbeTrace {
    fn drop(&mut self) {
        if !self.finished { self.finish(if std::thread::panicking() { "panic" } else { "abandoned" }, None); }
        CONTEXT.with(|slot| slot.set(self.previous));
    }
}

/// Only an ASCII authority is retained. Userinfo, path, query and fragment
/// never enter an Event, even if a caller put a credential into the URL.
fn safe_host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?.rsplit('@').next()?;
    if authority.is_empty() || authority.len() > 255 || !authority.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b)) { return None }
    Some(authority.to_ascii_lowercase())
}

fn safe_method(method: &str) -> &'static str {
    match method { "GET" => "GET", "POST" => "POST", "PUT" => "PUT", "PATCH" => "PATCH", "DELETE" => "DELETE", "HEAD" => "HEAD", "OPTIONS" => "OPTIONS", _ => "OTHER" }
}

struct RequestTrace { entry: Event, started: Instant, finished: bool }

impl RequestTrace {
    fn start(method: &str, url: &str) -> Self {
        REQUESTS.fetch_add(1, Ordering::Relaxed);
        let mut entry = event("http_begin");
        entry.request_id = Some(SEQUENCE.fetch_add(1, Ordering::Relaxed)); entry.method = Some(safe_method(method)); entry.host = safe_host(url);
        emit(&entry);
        Self { entry, started: Instant::now(), finished: false }
    }

    fn finish(&mut self, phase: &'static str, outcome: &'static str, status: Option<u16>, kind: Option<String>) {
        if self.finished { return }
        self.finished = true;
        REQUESTS.fetch_sub(1, Ordering::Relaxed);
        self.entry.phase = phase; self.entry.outcome = Some(outcome); self.entry.status = status; self.entry.transport_kind = kind;
        self.entry.timestamp_ms = now_ms(); self.entry.elapsed_ms = Some(self.started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        self.entry.probes_in_flight = PROBES.load(Ordering::Relaxed); self.entry.requests_in_flight = REQUESTS.load(Ordering::Relaxed);
        emit(&self.entry);
    }
}

impl Drop for RequestTrace {
    fn drop(&mut self) { if !self.finished { self.finish("http_failed", if std::thread::panicking() { "panic" } else { "abandoned" }, None, None); } }
}

pub(crate) fn http_middleware(request: ureq::Request, next: ureq::MiddlewareNext<'_>) -> Result<ureq::Response, ureq::Error> {
    let mut trace = RequestTrace::start(request.method(), request.url());
    let result = next.handle(request);
    match &result {
        Ok(response) => trace.finish("http_response", if response.status() < 400 { "ok" } else { "http_error" }, Some(response.status()), None),
        Err(ureq::Error::Status(status, _)) => trace.finish("http_response", "http_error", Some(*status), None),
        Err(ureq::Error::Transport(error)) => trace.finish("http_failed", "transport_error", None, Some(format!("{:?}", error.kind()))),
    }
    // A response event means headers arrived, NOT that body parsing completed.
    // The encompassing probe_end is emitted only after fetch/body processing.
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct Recording(Arc<Mutex<Vec<Event>>>);
    impl Recording {
        fn new() -> Self {
            let records = Arc::new(Mutex::new(Vec::new())); let copy = records.clone();
            set_sink(Some(Arc::new(move |entry| copy.lock().unwrap().push(entry.clone())))); Self(records)
        }
        fn provider(&self, name: &str) -> Vec<Event> { self.0.lock().unwrap().iter().filter(|e| e.provider == name).cloned().collect() }
    }
    impl Drop for Recording { fn drop(&mut self) { set_sink(None); } }

    #[test]
    fn traces_keep_correlation_without_credentials_or_response_data() {
        let _guard = TEST_LOCK.lock().unwrap(); let records = Recording::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap(); let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap(); let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new(); reader.read_line(&mut line).unwrap(); if line == "\r\n" { break }
                if let Some((name, value)) = line.split_once(':') { if name.eq_ignore_ascii_case("content-length") { length = value.trim().parse().unwrap(); } }
            }
            let mut received = vec![0; length]; reader.read_exact(&mut received).unwrap();
            assert!(String::from_utf8(received).unwrap().contains("request-body-secret"));
            let body = b"{\"data\":\"response-secret\"}";
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(body).unwrap();
        });
        let mut probe = ProbeTrace::start("diagnostic_privacy_test");
        let url = format!("http://username-secret:password-secret@127.0.0.1:{port}/path-secret?key=query-secret#fragment-secret");
        let response = crate::http::agent_builder().build().post(&url).set("Authorization", "header-secret")
            .send_json(serde_json::json!({"secret": "request-body-secret"})).unwrap();
        let body: serde_json::Value = response.into_json().unwrap(); assert_eq!(body["data"], "response-secret");
        probe.finish("ok", Some(1)); drop(probe); server.join().unwrap();
        let entries = records.provider("diagnostic_privacy_test");
        assert_eq!(entries.iter().map(|e| e.phase).collect::<Vec<_>>(), vec!["probe_begin", "http_begin", "http_response", "probe_end"]);
        assert!(entries.iter().all(|e| e.probe_id == entries[0].probe_id));
        assert_eq!(entries[1].request_id, entries[2].request_id); assert_eq!(entries[2].status, Some(200));
        assert_eq!(entries[1].method, Some("POST"));
        #[cfg(windows)]
        assert!(entries.iter().all(|entry| entry.os_thread_id.is_some_and(|id| id > 0)));
        let text = serde_json::to_string(&entries).unwrap();
        for secret in ["username-secret", "password-secret", "path-secret", "query-secret", "fragment-secret", "header-secret", "request-body-secret", "response-secret"] { assert!(!text.contains(secret), "diagnostics leaked {secret}"); }
    }

    #[test]
    fn panic_and_nested_probes_finish_and_restore_the_parent_context() {
        let _guard = TEST_LOCK.lock().unwrap(); let records = Recording::new();
        let mut outer = ProbeTrace::start("diagnostic_outer_test");
        let _ = std::panic::catch_unwind(|| { let _inner = ProbeTrace::start("diagnostic_inner_test"); panic!("fixture panic"); });
        outer.finish("ok", Some(0)); drop(outer);
        assert_eq!(records.provider("diagnostic_inner_test")[1].outcome, Some("panic"));
        let entries = records.provider("diagnostic_outer_test"); assert_eq!(entries.len(), 2); assert_eq!(entries[0].probe_id, entries[1].probe_id);
        assert!(CONTEXT.with(|slot| slot.get().probe_id.is_none()));
    }
}

//! One-shot JSON HTTP for the probes: short timeouts, no retries, `None` on any
//! non-2xx. A quota probe is never important enough to block a refresh cycle.

use std::time::Duration;

use serde_json::Value;

const CONNECT: Duration = Duration::from_secs(3);
const READ: Duration = Duration::from_secs(6);

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
    let agent = ureq::AgentBuilder::new().timeout_connect(CONNECT).timeout_read(READ).build();
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

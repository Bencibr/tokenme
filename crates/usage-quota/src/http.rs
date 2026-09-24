//! One-shot JSON HTTP for the probes: short timeouts, no retries, `None` on any
//! non-2xx. A quota probe is never important enough to block a refresh cycle.

use std::time::Duration;

use serde_json::Value;

const CONNECT: Duration = Duration::from_secs(3);
const READ: Duration = Duration::from_secs(6);

pub fn get_json(url: &str, headers: &[(&str, &str)]) -> Option<Value> {
    request(url, headers, None)
}

/// Used by the probes that must POST (Antigravity's local Connect RPC); the
/// read-only ones stick to `get_json`.
#[allow(dead_code)]
pub fn post_json(url: &str, headers: &[(&str, &str)], body: Value) -> Option<Value> {
    request(url, headers, Some(body))
}

fn request(url: &str, headers: &[(&str, &str)], body: Option<Value>) -> Option<Value> {
    let agent = ureq::AgentBuilder::new().timeout_connect(CONNECT).timeout_read(READ).build();
    let mut req = match body {
        Some(_) => agent.post(url),
        None => agent.get(url),
    };
    for (name, value) in headers {
        req = req.set(name, value);
    }
    let resp = match body {
        Some(body) => req.send_json(body).ok()?,
        None => req.call().ok()?,
    };
    if !(200u16..300).contains(&resp.status()) {
        return None;
    }
    resp.into_json().ok()
}

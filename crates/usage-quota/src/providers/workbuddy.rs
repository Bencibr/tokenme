//! WorkBuddy AI: the account's remaining credits, through the same billing
//! interface the desktop app and the open-source `workbuddy2api` gateway use.
//!
//! ## Credential
//!
//! The desktop app keeps its OAuth record at
//! `~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop-ai.info`:
//! `auth.accessToken`, `auth.expiresAt` (epoch ms), `auth.domain`
//! (`www.workbuddy.ai` global / `www.codebuddy.cn` CN), `account.uid`. The
//! record is read, never written: **nothing here ever refreshes** — consuming
//! a refresh grant without writing the rotation back would log the user out
//! of their own app, the same rule the Qoder probe follows.
//!
//! ## The call
//!
//! Transport, in order: **the running desktop app's wbipc broker** — the app
//! itself proxies the meter call with its own login state, so a stock install
//! needs no credential work at all (see `workbuddy_wbipc`) — then explicit
//! tokens: the desktop app's auth file (`WORKBUDDY_AUTH_FILE` overrides), then
//! a bare pasted token (`WORKBUDDY_TOKEN` or `tokenme/workbuddy.token`) —
//! current app versions encrypt that file's accessToken in place, so the paste
//! is what keeps installs without a running app working.
//! `POST https://<domain>/billing/meter/get-user-resource` (global; the CN
//! realm keeps the older `/v2/billing/meter/get-user-resource`, which is also
//! the global 404 fallback), body
//! `{PageNumber, PageSize, ProductCode: "p_tcaca", Status: [0,3],
//! PackageEndTimeRangeBegin, PackageEndTimeRangeEnd}` — measured live against
//! this machine's account, matching `workbuddy2api`'s `ResourceSummary`.
//!
//! The answer lists one row per credit package (this account: seven
//! "Bonus Pack" rows expiring on successive days), so packages are grouped by
//! name into one bar each — a pack-per-bar rendering would flood the strip.
//! A group's reset is the nearest expiry among its packs that still hold
//! credits; `CycleEndTime` is a UTC+8 wall clock ("2006-01-02 15:04:05").

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{json, Value};
use usage_core::QuotaSample;

use crate::http::post_json;
use crate::QuotaProbe;

pub struct WorkBuddyQuota;

const METER_PATH: &str = "/billing/meter/get-user-resource";
const METER_PATH_V2: &str = "/v2/billing/meter/get-user-resource";

impl QuotaProbe for WorkBuddyQuota {
    fn tool(&self) -> &'static str {
        "workbuddy"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let body = request_body(chrono::Local::now());
        // The app's own broker first: no token on disk needed. Silence there
        // (app closed, logged out, protocol moved on) falls through to the
        // explicit-token chain. Both platforms speak the same frames — the
        // endpoint string names a socket or a pipe per platform.
        if let Some(data) = super::workbuddy_wbipc::meter_envelope(&body) {
            let samples = samples_from_resource(&data);
            if !samples.is_empty() {
                return samples;
            }
        }
        let Some(info) = read_auth()
            .or_else(saved_login_auth)
            .or_else(|| bare_token().map(synthesize_auth))
        else {
            return Vec::new();
        };
        if !token_is_fresh(&info, chrono::Utc::now().timestamp_millis()) {
            return Vec::new();
        }
        let Some(data) = call(&info, &body) else { return Vec::new() };
        samples_from_resource(&data)
    }
}

// ---------------------------------------------------------------- credential

/// The auth record's location; `WORKBUDDY_AUTH_FILE` overrides it for tests
/// and side-by-side installs.
fn auth_file() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("WORKBUDDY_AUTH_FILE") {
        return Some(PathBuf::from(path));
    }
    dirs::data_dir()
        .map(|d| d.join("CodeBuddyExtension/Data/Public/auth/workbuddy-desktop-ai.info"))
        .filter(|p| p.is_file())
}

fn read_auth() -> Option<Value> {
    let raw = std::fs::read_to_string(auth_file()?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// A bare access token, for installs where the desktop app encrypts its auth
/// file. WorkBuddy ≥ 2.48 stores `accessToken` behind its own `$wbEncrypted`
/// at-rest scheme whose key never touches disk in extractable form, so the
/// app's own file yields no token on current versions. The account token is
/// long-lived (about a year), so pasting it once into `WORKBUDDY_TOKEN` or a
/// one-line `tokenme/workbuddy.token` file keeps the probe fed until expiry.
fn bare_token() -> Option<String> {
    if let Some(raw) = std::env::var_os("WORKBUDDY_TOKEN") {
        let t = raw.to_string_lossy().trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let path = dirs::config_dir()?.join("tokenme").join("workbuddy.token");
    let t = std::fs::read_to_string(path).ok()?;
    let t = t.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// The minimal auth record `call` needs when only a bare token is known: the
/// global domain default and no `expiresAt` (absent means fresh — the token's
/// real expiry lives with whoever issued it).
fn synthesize_auth(token: String) -> Value {
    serde_json::json!({ "auth": { "accessToken": token, "domain": "www.workbuddy.ai" } })
}

// ------------------------------------------------------------------- login

/// The plugin device-authorization endpoint WorkBuddy's own CLI family uses
/// (measured against `auto-checkin`'s login flow): ask for a state, finish
/// Tencent SSO in the browser, then poll until the token lands.
const LOGIN_BASE: &str = "https://copilot.tencent.com";
const LOGIN_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";

fn saved_login_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tokenme").join("workbuddy-auth.json"))
}

/// The credential saved by [`login`]: a JSON mirror of the auth file's `auth`
/// section plus `uid`, so `token_is_fresh` and `call` read it unchanged.
fn saved_login_auth() -> Option<Value> {
    let raw = std::fs::read_to_string(saved_login_path()?).ok()?;
    let cred: Value = serde_json::from_str(&raw).ok()?;
    let auth = cred.get("auth")?.clone();
    Some(serde_json::json!({ "auth": auth, "account": { "uid": cred.get("uid") } }))
}

fn open_browser(url: &str) {
    let command = match std::env::consts::OS {
        "macos" => vec!["open", url],
        "windows" => vec!["cmd", "/c", "start", "", url],
        _ => vec!["xdg-open", url],
    };
    let _ = std::process::Command::new(command[0]).args(&command[1..]).spawn();
}

/// Run the device login dance and persist the credential. Blocking (the CLI
/// wraps it); `poll` and `deadline` are injectable so tests can fake both.
pub fn login_with(
    mut poll: impl FnMut(&str) -> Option<Value>,
    mut sleep: impl FnMut(std::time::Duration),
    attempts: usize,
) -> Result<String, String> {
    let state_body: Value = serde_json::json!({});
    let state_resp = crate::http::post_json(
        &format!("{LOGIN_BASE}/v2/plugin/auth/state?platform=CLI"),
        &[("user-agent", LOGIN_UA), ("content-type", "application/json")],
        state_body,
    )
    .ok_or("auth/state unreachable — check the network")?;
    let state = state_resp
        .pointer("/data/state")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("auth/state returned no state: {state_resp}"))?
        .to_string();
    let auth_url = state_resp
        .pointer("/data/authUrl")
        .and_then(Value::as_str)
        .ok_or("auth/state returned no authUrl")?
        .to_string();
    open_browser(&auth_url);

    for _ in 0..attempts {
        sleep(std::time::Duration::from_secs(5));
        if let Some(token_data) = poll(&state) {
            let access = token_data
                .get("accessToken")
                .or_else(|| token_data.get("access_token"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if access.is_empty() {
                continue;
            }
            let expires_in = token_data
                .get("expiresIn")
                .or_else(|| token_data.get("expires_in"))
                .and_then(Value::as_i64)
                .unwrap_or(7 * 24 * 3600);
            let cred = serde_json::json!({
                "auth": {
                    "accessToken": access,
                    "refreshToken": token_data.get("refreshToken").or_else(|| token_data.get("refresh_token")),
                    "expiresAt": chrono::Utc::now().timestamp_millis() + expires_in * 1000,
                    "domain": token_data.get("domain").and_then(Value::as_str).unwrap_or("codebuddy.cn"),
                },
                "uid": token_data.get("uid").and_then(Value::as_str).unwrap_or("workbuddy"),
            });
            let path = saved_login_path().ok_or("no platform config dir")?;
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&cred).map_err(|e| e.to_string())?,
            )
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            return Ok(path.to_string_lossy().to_string());
        }
    }
    Err("login window elapsed (5 min) without a token — run workbuddy-login again".into())
}

/// The CLI entry: opens the browser, waits, saves. Returns a human summary.
pub fn login() -> Result<String, String> {
    let poll = |state: &str| {
        let url = format!("{LOGIN_BASE}/v2/plugin/auth/token?state={state}");
        crate::http::get_json(&url, &[("user-agent", LOGIN_UA)])
    };
    let path = login_with(poll, std::thread::sleep, 60)?;
    let cred = serde_json::from_str::<Value>(
        &std::fs::read_to_string(&path).map_err(|e| format!("cannot read {path}: {e}"))?,
    )
    .map_err(|e| e.to_string())?;
    let expires_at = cred.pointer("/auth/expiresAt").and_then(Value::as_i64).unwrap_or(0);
    let when = chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(expires_at.max(0) as u64),
    );
    Ok(format!(
        "WorkBuddy credentials saved to {} (token expires {})",
        path,
        when.format("%Y-%m-%d %H:%M")
    ))
}

/// An expired token is "no sample", never a refresh grant.
fn token_is_fresh(info: &Value, now_ms: i64) -> bool {
    info.pointer("/auth/expiresAt").and_then(Value::as_i64).is_none_or(|ms| ms > now_ms)
}

// ------------------------------------------------------------------ the call

/// `Status: [0,3]` = active and exhausted-but-unexpired packages, the window
/// the app's own usage page asks for.
fn request_body(now: chrono::DateTime<chrono::Local>) -> Value {
    let fmt = "%Y-%m-%d %H:%M:%S";
    json!({
        "PageNumber": 1,
        "PageSize": 100,
        "ProductCode": "p_tcaca",
        "Status": [0, 3],
        "PackageEndTimeRangeBegin": now.format(fmt).to_string(),
        "PackageEndTimeRangeEnd": (now + chrono::Duration::days(365)).format(fmt).to_string(),
    })
}

fn call(info: &Value, body: &Value) -> Option<Value> {
    let auth = info.get("auth")?;
    let token = auth.get("accessToken").and_then(Value::as_str).filter(|t| !t.is_empty())?;
    let uid = info.pointer("/account/uid").and_then(Value::as_str).unwrap_or_default();
    let domain = auth.get("domain").and_then(Value::as_str).unwrap_or("www.workbuddy.ai");
    let cn = domain.contains("codebuddy.cn");
    let base = format!("https://{domain}");
    let bearer = format!("Bearer {token}");
    let headers: Vec<(&str, &str)> = vec![
        ("authorization", &bearer),
        ("content-type", "application/json"),
        ("accept", "application/json"),
        ("x-user-id", uid),
        ("user-agent", "WorkBuddy/2.48.0"),
    ];
    // CN answers only under /v2; global prefers the bare path and falls back
    // on it. post_json drops non-2xx, so a 404 on the first candidate simply
    // moves to the next — a network blip costs one extra POST, nothing else.
    let paths: &[&str] = if cn { &[METER_PATH_V2] } else { &[METER_PATH, METER_PATH_V2] };
    for path in paths {
        if let Some(body) = post_json(&format!("{base}{path}"), &headers, body.clone()) {
            return Some(body);
        }
    }
    None
}

// ------------------------------------------------------------------- parsing

/// The wire answer, grouped into one bar per package name. `data.Response.Data`
/// nests twice (Tencent API envelope inside the app's own envelope).
pub(crate) fn samples_from_resource(body: &Value) -> Vec<QuotaSample> {
    let accounts = body
        .pointer("/data/Response/Data/Accounts")
        .or_else(|| body.pointer("/Response/Data/Accounts"))
        .and_then(Value::as_array);
    let Some(accounts) = accounts else { return Vec::new() };

    struct Group {
        size: f64,
        remain: f64,
        resets_at_ms: i64,
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for acct in accounts {
        let name = acct
            .get("PackageName")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or("积分包")
            .to_string();
        // Cycle fields describe the current billing cycle and win when set;
        // the Capacity triple is the package's lifetime fallback.
        let (size, remain) = match (
            acct.get("CycleCapacitySize").and_then(Value::as_f64).unwrap_or(0.0),
            acct.get("CycleCapacityRemain").and_then(Value::as_f64).unwrap_or(0.0),
        ) {
            (size, remain) if size > 0.0 => (size, remain.clamp(0.0, size)),
            _ => (
                acct.get("CapacitySize").and_then(Value::as_f64).unwrap_or(0.0),
                acct.get("CapacityRemain").and_then(Value::as_f64).unwrap_or(0.0),
            ),
        };
        if size <= 0.0 {
            continue;
        }
        let group = groups.entry(name).or_insert(Group { size: 0.0, remain: 0.0, resets_at_ms: 0 });
        group.size += size;
        group.remain += remain;
        // The group's reset is the nearest expiry among packs that still hold
        // credits — a pack at 0 has nothing left to lose.
        if remain > 0.0 {
            if let Some(ms) = acct.get("CycleEndTime").and_then(Value::as_str).and_then(parse_cycle_end) {
                if group.resets_at_ms == 0 || ms < group.resets_at_ms {
                    group.resets_at_ms = ms;
                }
            }
        }
    }
    groups
        .into_iter()
        .map(|(name, g)| QuotaSample {
            used_percent: ((1.0 - g.remain / g.size) * 100.0).clamp(0.0, 100.0),
            window_minutes: 0,
            resets_at_ms: g.resets_at_ms,
            label: Some(format!("{name} · 已用 {}/{}", trim(g.size - g.remain), trim(g.size))),
            // The pack name is the window's stable identity — grouping is by
            // name, so it is unique per sample. A shared id across packs makes
            // the notify machine see one window flapping between tiers and
            // re-fires the exhaustion banner on every poll (measured on the
            // CodeBuddy sibling, 2026-10-10; this shape is identical).
            id: Some(name),
        })
        .collect()
}

/// `CycleEndTime` is a UTC+8 wall clock with no offset on the wire.
fn parse_cycle_end(raw: &str) -> Option<i64> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S").ok()?;
    let zone = chrono::FixedOffset::east_opt(8 * 3600)?;
    Some(zone.from_local_datetime(&naive).single()?.timestamp_millis())
}

/// Whole when whole, one decimal when fractional (same rule as Qoder's).
fn trim(n: f64) -> String {
    if n.fract() == 0.0 { format!("{n:.0}") } else { format!("{n:.1}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer this machine's account actually gave (2026-09-25), trimmed
    /// to the fields the probe reads: one exhausted pack and two live ones.
    const MEASURED: &str = r#"{"code":0,"msg":"OK","data":{"Response":{"Data":{
        "TotalCount":3,"TotalDosage":60,"Accounts":[
          {"PackageName":"Bonus Pack","Status":3,"CycleEndTime":"2026-09-25 19:35:00",
           "CapacitySize":250,"CapacityRemain":0,"CapacityUsed":250,
           "CycleCapacitySize":250,"CycleCapacityRemain":0,"CycleCapacityUsed":250},
          {"PackageName":"Bonus Pack","Status":0,"CycleEndTime":"2026-10-13 01:18:53",
           "CapacitySize":30,"CapacityRemain":30,"CapacityUsed":0,
           "CycleCapacitySize":30,"CycleCapacityRemain":30,"CycleCapacityUsed":0},
          {"PackageName":"Bonus Pack","Status":0,"CycleEndTime":"2026-10-19 02:27:49",
           "CapacitySize":30,"CapacityRemain":30,"CapacityUsed":0,
           "CycleCapacitySize":30,"CycleCapacityRemain":30,"CycleCapacityUsed":0}
        ]}}}}"#;

    #[test]
    fn same_named_packs_fold_into_one_bar() {
        let body: Value = serde_json::from_str(MEASURED).unwrap();
        let s = samples_from_resource(&body);
        assert_eq!(s.len(), 1, "{s:?}");
        let bar = &s[0];
        assert_eq!(bar.label.as_deref(), Some("Bonus Pack · 已用 250/310"));
        assert!((bar.used_percent - (1.0 - 60.0 / 310.0) * 100.0).abs() < 1e-9);
        // The reset is the nearest expiry among packs still holding credits,
        // not the exhausted pack's earlier date.
        assert_eq!(bar.resets_at_ms, parse_cycle_end("2026-10-13 01:18:53").unwrap());
    }

    #[test]
    fn differently_named_packs_get_their_own_bars() {
        let body = json!({"Response": {"Data": {"Accounts": [
            {"PackageName": "套餐内", "CycleCapacitySize": 2000, "CycleCapacityRemain": 500,
             "CycleEndTime": "2026-10-01 00:00:00"},
            {"PackageName": "Bonus Pack", "CapacitySize": 30, "CapacityRemain": 30,
             "CycleCapacitySize": 0, "CycleEndTime": ""}
        ]}}});
        let s = samples_from_resource(&body);
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[0].label.as_deref(), Some("Bonus Pack · 已用 0/30"));
        assert_eq!(s[0].used_percent, 0.0);
        assert_eq!(s[0].resets_at_ms, 0, "a capacity-only pack advertises no cycle reset");
        assert_eq!(s[1].label.as_deref(), Some("套餐内 · 已用 1500/2000"));
        assert_eq!(s[1].used_percent, 75.0);
    }

    #[test]
    fn empty_or_shapeless_answers_are_silence() {
        assert!(samples_from_resource(&json!({"code": 0})).is_empty());
        assert!(samples_from_resource(&json!({"Response": {"Data": {"Accounts": []}}})).is_empty());
        // A zero-sized pack is not a 100% bar (same rule as Qoder's 0/0).
        assert!(samples_from_resource(&json!({"Response": {"Data": {"Accounts": [
            {"PackageName": "Free", "CapacitySize": 0, "CapacityRemain": 0}
        ]}}})).is_empty());
    }

    #[test]
    fn the_cycle_end_wall_clock_reads_as_beijing_time() {
        // 2026-10-13 01:18:53 +0800 = 2026-10-12 17:18:53 UTC.
        let ms = parse_cycle_end("2026-10-13 01:18:53").unwrap();
        assert_eq!(ms, 1_791_825_533_000);
        assert!(parse_cycle_end("not a date").is_none());
    }

    #[test]
    fn a_bare_token_synthesizes_a_working_auth_record() {
        let info = synthesize_auth("tok-123".into());
        assert_eq!(
            info.pointer("/auth/accessToken").and_then(Value::as_str),
            Some("tok-123")
        );
        // No expiresAt at all counts as fresh: the real expiry is the issuer's
        // business, and an unparsable date must not silently kill the probe.
        assert!(token_is_fresh(&info, i64::MAX - 1));
        let auth = info.get("auth").unwrap();
        assert_eq!(auth.get("domain").and_then(Value::as_str), Some("www.workbuddy.ai"));
    }

    #[test]
    fn an_expired_token_is_not_used() {
        let info = json!({"auth": {"accessToken": "t", "expiresAt": 1_791_071_360_000i64}});
        assert!(!token_is_fresh(&info, 1_791_071_360_000));
        assert!(token_is_fresh(&info, 1_791_071_359_999));
        assert!(token_is_fresh(&json!({"auth": {"accessToken": "t"}}), i64::MAX));
    }
}

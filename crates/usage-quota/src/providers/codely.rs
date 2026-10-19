//! Codely (Tuanjie / Unity China): the account's own usage windows.
//!
//! ## The interface, from the product's own client
//!
//! The shipped core (`configHandler.controlPlaneClient.getUserUsageSummary`)
//! calls:
//!
//! ```text
//! GET https://codely.tuanjie.cn/api/user/usage/summary[?orgId=<uuid>]
//! Authorization: Bearer <access token>
//! ```
//!
//! and reads the `coding_plan` entry's `windows[]`:
//! `{window_type, quota_points, used_points, remaining_points, exhausted,
//! period?{start_at,end_at}}`. The desktop window renders exactly these as
//! `5 小时 / 每周 / 每月` (`Ol = ["usage_5h","subscription_week",
//! "subscription_month"]` in the shipped UI bundle). A `usage_5h` window
//! normally has no `period` — the vendor only prints a reset instant for the
//! subscription windows — so it is reported with `resets_at_ms = 0` and the
//! panel shows the bar without a countdown instead of inventing one.
//!
//! `details` also carries a `recharged` wallet (`remaining_points` only). The
//! panel's row shape is a percent bar, and no quota can be derived from a bare
//! balance, so it is deliberately not reported.
//!
//! ## Credentials, read-only
//!
//! The CLI (and the Cowork core it drives) writes
//! `~/.codely-cli/oauth_creds.json` → `access_token`; the desktop app keeps the
//! same token in `~/.codely/cowork-settings.json` → `ContinueAccessToken`. The
//! CLI record is tried first and the Cowork record second, deduped, so an
//! account that signed in through either product answers. Nothing here ever
//! refreshes or writes a token: the vendor's refresh grant may rotate the
//! refresh token, and consuming a rotation without writing it back logs the user
//! out of their own client. An expired login simply answers no sample.
//!
//! `orgId` comes from `~/.codely-cli/org.json` (`accounts[*].currentOrgId`),
//! which is how the product scopes a user who belongs to several organisations.
//! Without one the call is still valid and answers the account's default.

use std::path::PathBuf;

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::http::get_json;
use crate::QuotaProbe;

const API_URL: &str = "https://codely.tuanjie.cn/api/user/usage/summary";
const USER_AGENT: &str = concat!("tokenme/", env!("CARGO_PKG_VERSION"));

/// `(wire window_type, the label the product prints, window minutes)`.
const WINDOWS: &[(&str, &str, i64)] = &[
    ("usage_5h", "5 小时", 300),
    ("subscription_week", "周", 10_080),
    ("subscription_month", "月", 43_200),
];

pub struct CodelyQuota;

impl QuotaProbe for CodelyQuota {
    fn tool(&self) -> &'static str {
        "codely"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        live().unwrap_or_default()
    }
}

fn live() -> Option<Vec<QuotaSample>> {
    let org = org_id();
    for token in tokens() {
        let authorization = format!("Bearer {token}");
        let headers = [
            ("authorization", authorization.as_str()),
            ("accept", "application/json"),
            ("user-agent", USER_AGENT),
        ];
        if let Some(body) = get_json(&url(org.as_deref()), &headers) {
            let samples = samples_from(&body);
            if !samples.is_empty() {
                return Some(samples);
            }
        }
    }
    None
}

fn url(org: Option<&str>) -> String {
    match org {
        Some(org) if !org.is_empty() => format!("{API_URL}?orgId={}", encode(org)),
        _ => API_URL.to_string(),
    }
}

/// Percent-encode the org id: the value is a UUID today, but it reaches a URL
/// and no percent-encoding dependency is worth adding for one parameter.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The CLI home, with tokenme's test/relocation override.
fn cli_home() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("CODELY_CLI_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|home| home.join(".codely-cli"))
}

/// The desktop product's data home, where Cowork keeps its own login record.
fn data_home() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("CODELY_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|home| home.join(".codely"))
}

/// Every token the machine holds, CLI first, duplicates removed.
fn tokens() -> Vec<String> {
    let mut out = Vec::new();
    let candidates = [
        cli_home().map(|home| home.join("oauth_creds.json")),
        data_home().map(|home| home.join("cowork-settings.json")),
    ];
    for path in candidates.into_iter().flatten() {
        let Some(value) = read_json(&path) else { continue };
        let token = value.get("access_token").and_then(Value::as_str).or_else(|| {
            // The desktop settings use the Continue-derived key spelling.
            value.get("ContinueAccessToken").and_then(Value::as_str)
        });
        let Some(token) = token.map(str::trim).filter(|token| !token.is_empty()) else { continue };
        if !out.iter().any(|seen| seen == token) {
            out.push(token.to_string());
        }
    }
    out
}

fn org_id() -> Option<String> {
    let home = cli_home()?;
    let value = read_json(&home.join("org.json"))?;
    org_from(&value, signed_in_user(&home).as_deref())
}

/// `user_id` in the CLI's login record, as the key `org.json` uses. Older
/// builds wrote a number, newer ones a string; both name the same account.
fn signed_in_user(home: &std::path::Path) -> Option<String> {
    let value = read_json(&home.join("oauth_creds.json"))?;
    match value.get("user_id")? {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
        _ => None,
    }
}

/// `{"accounts":{"<user id>":{"currentOrgId":"…","currentOrgName":"…"}}}`.
/// The signed-in account's entry wins; a machine with several organisations
/// must not report the first map key's org for the account that is actually
/// paying for this session.
fn org_from(value: &Value, user: Option<&str>) -> Option<String> {
    let accounts = value.get("accounts")?.as_object()?;
    let account = user
        .and_then(|user| accounts.get(user))
        .or_else(|| accounts.values().next())?;
    account
        .get("currentOrgId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|org| !org.is_empty())
        .map(str::to_string)
}

fn read_json(path: &std::path::Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        // The endpoint writes points as decimal strings (`"8000"`, `"126477"`).
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// The `coding_plan` windows as the panel's percent rows.
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    let Some(details) = body.get("details").and_then(Value::as_array) else { return Vec::new() };
    let Some(plan) = details
        .iter()
        .find(|detail| detail.get("type").and_then(Value::as_str) == Some("coding_plan"))
    else {
        return Vec::new();
    };
    let Some(windows) = plan.get("windows").and_then(Value::as_array) else { return Vec::new() };

    let mut out = Vec::new();
    for window in windows {
        let Some(kind) = window.get("window_type").and_then(Value::as_str) else { continue };
        let Some((label, minutes)) = WINDOWS
            .iter()
            .find(|(wire, _, _)| *wire == kind)
            .map(|(_, label, minutes)| (*label, *minutes))
        else {
            // A window the product does not draw is not a row tokenme invents.
            continue;
        };
        let quota = number(window.get("quota_points")).unwrap_or(0.0);
        if quota <= 0.0 {
            continue;
        }
        let used = number(window.get("used_points")).unwrap_or(0.0);
        let exhausted = window.get("exhausted").and_then(Value::as_bool).unwrap_or(false);
        let used_percent = if exhausted {
            100.0
        } else {
            (used / quota * 100.0).clamp(0.0, 100.0)
        };
        let resets_at_ms = window
            .pointer("/period/end_at")
            .and_then(Value::as_str)
            .and_then(parse_ts_ms)
            .unwrap_or(0);
        out.push(QuotaSample {
            used_percent,
            window_minutes: minutes,
            resets_at_ms,
            label: Some(label.to_string()),
            id: Some(kind.to_string()),
        });
    }
    out.sort_by_key(|sample| sample.window_minutes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_two_plan_windows_become_two_bars() {
        let body = json!({
            "remaining_points": "129817",
            "is_exhausted": false,
            "details": [{
                "type": "coding_plan",
                "quota_points": "8000",
                "used_points": "2000",
                "remaining_points": "6000",
                "exhausted": false,
                "windows": [
                    {"window_type": "usage_5h", "quota_points": "8000", "used_points": "2000",
                     "remaining_points": "6000", "exhausted": false},
                    {"window_type": "subscription_week", "quota_points": "40000", "used_points": "10000",
                     "remaining_points": "30000", "exhausted": false,
                     "period": {"start_at": "2026-10-20T00:00:00+08:00", "end_at": "2026-10-27T00:00:00+08:00"}}
                ]
            }, {"type": "recharged", "remaining_points": "3340"}]
        });
        let samples = samples_from(&body);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].label.as_deref(), Some("5 小时"));
        assert_eq!(samples[0].window_minutes, 300);
        assert_eq!(samples[0].used_percent, 25.0);
        assert_eq!(samples[0].resets_at_ms, 0, "the 5-hour window has no printed reset");
        assert_eq!(samples[0].id.as_deref(), Some("usage_5h"));
        assert_eq!(samples[1].label.as_deref(), Some("周"));
        assert_eq!(samples[1].window_minutes, 10_080);
        assert_eq!(samples[1].used_percent, 25.0);
        assert_eq!(
            samples[1].resets_at_ms,
            parse_ts_ms("2026-10-27T00:00:00+08:00").unwrap(),
            "the weekly reset is the period's end"
        );
    }

    #[test]
    fn an_exhausted_window_is_full_even_before_used_reaches_quota() {
        let body = json!({"details": [{"type": "coding_plan", "windows": [
            {"window_type": "usage_5h", "quota_points": "8000", "used_points": "1", "exhausted": true}
        ]}]});
        assert_eq!(samples_from(&body)[0].used_percent, 100.0);
    }

    #[test]
    fn a_plan_without_windows_or_with_zero_quota_is_not_a_bar() {
        assert!(samples_from(&json!({"details": [{"type": "coding_plan", "windows": []}]})).is_empty());
        assert!(samples_from(&json!({"details": [{"type": "coding_plan", "windows": [
            {"window_type": "usage_5h", "quota_points": "0", "used_points": "0"}
        ]}]}))
        .is_empty());
        assert!(samples_from(&json!({"details": [{"type": "recharged", "remaining_points": "3340"}]})).is_empty());
    }

    #[test]
    fn a_window_the_product_does_not_draw_is_not_invented() {
        let body = json!({"details": [{"type": "coding_plan", "windows": [
            {"window_type": "subscription_month", "quota_points": "1000", "used_points": "10"},
            {"window_type": "mystery_window", "quota_points": "1000", "used_points": "10"}
        ]}]});
        let samples = samples_from(&body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].label.as_deref(), Some("月"));
        assert_eq!(samples[0].window_minutes, 43_200);
    }

    #[test]
    fn the_org_is_the_accounts_current_one() {
        let value = json!({"accounts": {
            "111": {"currentOrgId": "org-other", "currentOrgName": "Other"},
            "58316": {"currentOrgId": "org-1", "currentOrgName": "Demo"}
        }});
        assert_eq!(org_from(&value, Some("58316")).as_deref(), Some("org-1"));
        assert_eq!(
            org_from(&value, Some("missing")).as_deref(),
            Some("org-other"),
            "an unknown account falls back to the map's first entry"
        );
        assert_eq!(org_from(&json!({"accounts": {}}), None), None);
        assert_eq!(url(Some("org 1")), "https://codely.tuanjie.cn/api/user/usage/summary?orgId=org%201");
        assert_eq!(url(None), "https://codely.tuanjie.cn/api/user/usage/summary");
    }

    #[test]
    #[ignore = "reads ~/.codely-cli and calls the vendor's live usage API"]
    fn the_live_account_reports_its_windows() {
        let samples = live().expect("the signed-in Codely account answers its usage summary");
        assert!(!samples.is_empty(), "a coding plan answers at least one window");
        for sample in &samples {
            assert!((0.0..=100.0).contains(&sample.used_percent), "{sample:?}");
            assert!(sample.window_minutes > 0, "{sample:?}");
            assert!(sample.id.is_some(), "{sample:?}");
        }
        println!("{samples:#?}");
    }
}

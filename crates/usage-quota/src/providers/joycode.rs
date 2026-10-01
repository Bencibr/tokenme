//! JoyCode (JD's AI IDE): the account's remaining point balance through the
//! IDE's own color-gateway endpoint.
//!
//! ## The call
//!
//! JoyCode's extension declares every backend route in a typed endpoint table
//! (`COLOR_ENDPOINTS`, `extensions/joycoder-editor/dist/extension.js`), and the
//! quota display is this one:
//!
//! ```text
//! IDE_POINT: functionId "get_newpoint_ide",
//!            rawUrl   "/api/saas/point/v1/getNewIdePoint"
//! ```
//!
//! `colorGatewayEnabled` routes it through JD's "color" gateway rather than
//! the agent's direct base URL, and the gateway request is **signed**:
//!
//! ```text
//! POST https://api-ai.jd.com/api
//!      ?appid=joycode_ide&functionId=get_newpoint_ide&t=<epoch-ms>&sign=<hex>
//! headers: Content-Type: application/json
//!          ptKey: <session key>      loginType: PIN_JD_CLOUD
//! sign = HMAC-SHA256(key = "0691a3f0b37b4a85aeb63ad0fc7db3ed",
//!                    msg  = sorted-key param values joined with "&").hex
//! ```
//!
//! — the appid, the key and the sorting/join rule are all hardcoded in the
//! shipped bundle (measured live 2026-09-29: HTTP 200 with this machine's real
//! balance). The `ptKey` session credential is not a secret blob: JoyCode
//! stores its login record in **plain text** inside the IDE's own
//! `state.vscdb` (`ItemTable` key `JoyCoder.IDE` → `joyCoderUser.ptKey`,
//! `loginType`), which is what the probe reads. No keychain, no prompt.
//!
//! ## How a point package becomes a bar
//!
//! The answer's `data.usageItems[]` carries one entry per point package with
//! `used`/`total`/`usedPercent` (which may exceed 100 — this machine's team
//! package is at 101%) and an `expireText` like `有效期至 2026年10月21日`,
//! parsed as that day's end in Beijing time. A package with no total is
//! skipped, the same 0/0 rule the other probes follow.

use std::path::PathBuf;

use serde_json::{json, Value};
use usage_core::QuotaSample;

use crate::providers::hmac_sha256;
use crate::QuotaProbe;

pub struct JoycodeQuota;

const GATEWAY_URL: &str = "https://api-ai.jd.com/api";
const APPID: &str = "joycode_ide";
const FUNCTION_ID: &str = "get_newpoint_ide";
const SIGN_KEY: &str = "0691a3f0b37b4a85aeb63ad0fc7db3ed";
const DEFAULT_LOGIN_TYPE: &str = "PIN_JD_CLOUD";

/// The IDE's auth/config database. `JOYCODE_STATE_DB` overrides it for tests
/// and side-by-side installs.
fn state_db() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("JOYCODE_STATE_DB") {
        return (!dir.is_empty()).then(|| PathBuf::from(dir));
    }
    dirs::data_dir().map(|d| d.join("JoyCode/User/globalStorage/state.vscdb"))
}

struct Login {
    pt_key: String,
    login_type: String,
    color_base: String,
}

/// The login record, read straight out of the IDE's own state database.
fn read_login() -> Option<Login> {
    let conn = rusqlite::Connection::open_with_flags(
        state_db()?,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let read = |key: &str| -> Option<String> {
        conn.query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get::<_, String>(0),
        )
        .ok()
    };
    let user: Value = serde_json::from_str(&read("JoyCoder.IDE")?).ok()?;
    let user = user.get("joyCoderUser")?;
    let pt_key = user.get("ptKey").and_then(Value::as_str)?.to_string();
    let login_type = user
        .get("loginType")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .unwrap_or(DEFAULT_LOGIN_TYPE)
        .to_string();
    let storage: Value = serde_json::from_str(&read("joycode.storageUser")?).ok()?;
    let color_base = storage
        .get("colorBaseUrl")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(GATEWAY_URL)
        .trim_end_matches('/')
        .to_string();
    Some(Login { pt_key, login_type, color_base })
}

/// Sorted-key param values joined with `&`, empties filtered — exactly the
/// bundle's `h()`. The `sign` parameter itself is appended after signing.
fn sign_params(params: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let joined: Vec<&str> = sorted
        .iter()
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
        .collect();
    let mac = hmac_sha256(SIGN_KEY.as_bytes(), joined.join("&").as_bytes());
    mac.iter().map(|b| format!("{b:02x}")).collect()
}

fn gateway_request(login: &Login, now_ms: i64) -> (String, String) {
    let params: Vec<(String, String)> = vec![
        ("appid".into(), APPID.into()),
        ("functionId".into(), FUNCTION_ID.into()),
        ("t".into(), now_ms.to_string()),
    ];
    let sign = sign_params(&params);
    let mut qs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect();
    qs.push(format!("sign={sign}"));
    (format!("{}/api?{}", login.color_base, qs.join("&")), sign)
}

fn urlencode(v: &str) -> String {
    let mut out = String::new();
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

impl QuotaProbe for JoycodeQuota {
    fn tool(&self) -> &'static str {
        "joycode"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(login) = read_login() else { return Vec::new() };
        let (url, _sign) = gateway_request(&login, chrono::Utc::now().timestamp_millis());
        let Some(body) = crate::http::post_json(
            &url,
            &[
                ("content-type", "application/json"),
                ("ptKey", login.pt_key.as_str()),
                ("loginType", login.login_type.as_str()),
                ("accept", "application/json"),
            ],
            json!({}),
        ) else {
            return Vec::new();
        };
        samples_from_point(&body)
    }
}

/// The wire answer, one bar per point package.
pub(crate) fn samples_from_point(body: &Value) -> Vec<QuotaSample> {
    let items = body.pointer("/data/usageItems").and_then(Value::as_array);
    let Some(items) = items else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let total = item.get("total").and_then(Value::as_f64)?;
            if total <= 0.0 {
                return None;
            }
            let used = item.get("used").and_then(Value::as_f64).unwrap_or(0.0);
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
                .unwrap_or("积分");
            let badge = item.get("badge").and_then(Value::as_str).unwrap_or("");
            let expire = item.get("expireText").and_then(Value::as_str).unwrap_or("");
            let label = format!("{title} · 已用 {}/{}{}", trim(used), trim(total), {
                let mut extra = String::new();
                if !badge.is_empty() {
                    extra.push_str(&format!(" · {badge}"));
                }
                if !expire.is_empty() {
                    extra.push_str(&format!(" · {expire}"));
                }
                extra
            });
            Some(QuotaSample {
                used_percent: item
                    .get("usedPercent")
                    .and_then(Value::as_f64)
                    .unwrap_or(used / total * 100.0)
                    .clamp(0.0, 100.0),
                window_minutes: 0,
                resets_at_ms: parse_expire(expire).unwrap_or(0),
                label: Some(label),
                id: item.get("id").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

/// `有效期至 2026年10月21日` → that day's 23:59:59 Beijing time. Hand-scanned:
/// the probe does not grow a regex dependency for one date shape.
fn parse_expire(text: &str) -> Option<i64> {
    use chrono::TimeZone;
    let y = text.find('年')?;
    let y: i64 = text[..y].chars().rev().take_while(|c| c.is_ascii_digit()).collect::<String>().chars().rev().collect::<String>().parse().ok()?;
    let after_y = &text[text.find('年')? + 3..];
    let m = after_y.find('月')?;
    let m: i64 = after_y[..m].parse().ok()?;
    let after_m = &after_y[m.to_string().len() + 3..];
    let d = after_m.find('日')?;
    let d: i64 = after_m[..d].parse().ok()?;
    let zone = chrono::FixedOffset::east_opt(8 * 3600)?;
    Some(
        zone.with_ymd_and_hms(y as i32, m as u32, d as u32, 23, 59, 59)
            .single()?
            .timestamp_millis(),
    )
}

/// Whole when whole, one decimal when fractional (same rule as the others).
fn trim(n: f64) -> String {
    if n.fract() == 0.0 { format!("{n:.0}") } else { format!("{n:.1}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden sign computed independently (python hmac/hashlib) for the
    /// fixed params appid=joycode_ide, functionId=get_newpoint_ide,
    /// t=1700000000000. Pins the sorted-values join and the hex encoding.
    #[test]
    fn the_gateway_sign_matches_the_reference_implementation() {
        let params: Vec<(String, String)> = vec![
            ("appid".into(), APPID.into()),
            ("functionId".into(), FUNCTION_ID.into()),
            ("t".into(), "1700000000000".into()),
        ];
        assert_eq!(
            sign_params(&params),
            "923845cd0b0b526e1c1e01d783a29f59b3fb80f2a6c3de633bbffbadd0c34815"
        );
    }

    /// The answer this machine's account actually gave (2026-09-29), trimmed
    /// to the fields the probe reads: one over-drawn team package.
    const MEASURED: &str = r#"{"code":0,"data":{
        "emptyText":"暂无可用积分信息",
        "usageItems":[{"badge":"团队版（限免）","usedPercent":101,"total":10000,
          "expireText":"有效期至 2026年10月21日","remain":-142,"id":"seat-package",
          "used":10142,"type":"seatPackage","title":"套餐积分"}],
        "title":"我的用量","accountRole":"main"},"msg":"成功"}"#;

    #[test]
    fn the_measured_answer_becomes_one_overdrawn_bar() {
        let body: Value = serde_json::from_str(MEASURED).unwrap();
        let s = samples_from_point(&body);
        assert_eq!(s.len(), 1, "{s:?}");
        let bar = &s[0];
        // 101% is real (the package is over-drawn); the bar clamps, the label keeps truth.
        assert_eq!(bar.used_percent, 100.0);
        assert_eq!(
            bar.label.as_deref(),
            Some("套餐积分 · 已用 10142/10000 · 团队版（限免） · 有效期至 2026年10月21日")
        );
        assert_eq!(bar.id.as_deref(), Some("seat-package"));
        // 有效期至 2026-10-21 23:59:59 +08:00
        assert_eq!(bar.resets_at_ms, 1_792_598_399_000);
    }

    #[test]
    fn shapeless_or_packageless_answers_are_silence() {
        assert!(samples_from_point(&json!({"code": 0})).is_empty());
        assert!(samples_from_point(&json!({"data": {"usageItems": []}})).is_empty());
        // A package without a total is the 0/0 bar the other probes skip.
        assert!(samples_from_point(&json!({"data": {"usageItems": [
            {"id": "x", "title": "积分", "used": 5}
        ]}})).is_empty());
    }

    #[test]
    fn the_expire_date_reads_as_beijing_end_of_day() {
        assert_eq!(parse_expire("有效期至 2026年10月21日"), Some(1_792_598_399_000));
        assert_eq!(parse_expire("no date here"), None);
    }
}

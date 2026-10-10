//! CodeBuddy CN's Craft-credit balance, from the same billing meter the
//! plans-usage console page drives.
//!
//! The chain, measured on this machine (2026-10-10): the desktop IDE caches its
//! login in Chromium `safeStorage` form — `%APPDATA%\CodeBuddy CN\Local State`
//! holds the DPAPI-wrapped AES key, `User/globalStorage/state.vscdb` holds the
//! `secret://…planning-genie.new.accessTokencn` blob next to it — and the
//! decrypted JSON carries `auth.accessToken`. That token POSTs to
//! `workbuddy.cn`'s billing meter (the two products share one account
//! backend; the open-source `wwenc6621/CodeBuddy-Usage` reader crosses this
//! path too), and the answer's `Accounts[]` are the credit packs: names,
//! capacities and the cycle expiry, all carried as *string* numbers
//! (`CapacityRemainPrecise` — parsed, never trusted as a type).
//!
//! Read-only end to end, and a miss is silence: a locked db (the IDE writes
//! while it runs), an expired token, a moved endpoint — each is an empty vec,
//! and the panel keeps the last answer.

use base64::Engine as _;
use serde_json::{json, Value};

use super::crypto;

/// The meter behind `https://www.codebuddy.cn/profile/plans-usage`. The two
/// products share the account backend; this host answers the extension's
/// bearer token where codebuddy.cn's own web session would be needed.
const METER_URL: &str = "https://www.workbuddy.cn/billing/meter/get-user-resource";
/// The desktop IDE's roaming profile (the `Local State` + `state.vscdb` pair).
const SUPPORT_DIR: &str = "CodeBuddy CN";
/// The cached CN login: account envelope plus the bearer token.
const TOKEN_SECRET_KEY: &str = r#"secret://{"extensionId":"tencent-cloud.coding-copilot","key":"planning-genie.new.accessTokencn"}"#;

pub struct CodeBuddyQuota;

impl super::super::QuotaProbe for CodeBuddyQuota {
    fn tool(&self) -> &'static str {
        "codebuddy"
    }

    fn fetch(&self) -> Vec<crate::QuotaSample> {
        let Some(token) = access_token() else { return Vec::new() };
        let Some(body) = post_meter(&token) else { return Vec::new() };
        samples_from_resource(&body)
    }
}

/// The bearer token from the IDE's encrypted cache, or nothing. Windows only —
/// the macOS half of the same chain wraps its key in the Keychain and has not
/// been measured here.
#[cfg(windows)]
fn access_token() -> Option<String> {
    let base = dirs::data_dir()?.join(SUPPORT_DIR);
    let info: Value = serde_json::from_str(
        &std::fs::read_to_string(base.join("Local State")).ok()?,
    )
    .ok()?;
    let wrapped = base64::engine::general_purpose::STANDARD
        .decode(info.get("os_crypt")?.get("encrypted_key")?.as_str()?)
        .ok()?
        .strip_prefix(b"DPAPI")?
        .to_vec();
    let key = crypto::dpapi_unprotect(&wrapped)?;
    if key.len() != 32 {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        base.join("User").join("globalStorage").join("state.vscdb"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let raw: String = conn
        .query_row("SELECT value FROM ItemTable WHERE key = ?1", [TOKEN_SECRET_KEY], |r| r.get(0))
        .ok()?;
    let blob: Vec<u8> = serde_json::from_str::<Value>(&raw)
        .ok()?
        .get("data")?
        .as_array()?
        .iter()
        .map(|n| n.as_u64().unwrap_or(0) as u8)
        .collect();
    let plain = crypto::decrypt_windows_v10(&key, &blob)?;
    let info: Value = serde_json::from_slice(&plain).ok()?;
    info.pointer("/auth/accessToken")?.as_str().filter(|t| !t.is_empty()).map(str::to_string)
}

#[cfg(not(windows))]
fn access_token() -> Option<String> {
    None
}

/// One POST, shaped the way the console page and the open-source reader both
/// send it: only currently-valid packs, both live (0) and exhausted (3) ones.
fn post_meter(token: &str) -> Option<Value> {
    let resp: Value = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout_read(std::time::Duration::from_secs(10))
        .build()
        .post(METER_URL)
        .set("authorization", &format!("Bearer {token}"))
        .set("content-type", "application/json")
        .set("origin", "https://www.workbuddy.cn")
        .set("referer", "https://www.workbuddy.cn/profile/plans-usage")
        .set("x-client-platform", "web")
        .set("user-agent", concat!("tokenme/", env!("CARGO_PKG_VERSION")))
        .send_string(
            &json!({
                "PageNumber": 1,
                "PageSize": 200,
                "ProductCode": "p_tcaca",
                "Status": [0, 3],
                "OnlyValidPeriod": true,
                "NeedInUsage": true,
            })
            .to_string(),
        )
        .ok()?
        .into_json()
        .ok()?;
    Some(resp)
}

/// The credit packs, grouped by name the way WorkBuddy's probe groups the same
/// vendor shape: cycle capacities win when present, the capacity triple is the
/// package's lifetime fallback, and a group's reset is the nearest expiry among
/// packs that still hold credit. Both field sets arrive as string numbers.
pub(crate) fn samples_from_resource(body: &Value) -> Vec<crate::QuotaSample> {
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
    let mut groups: std::collections::BTreeMap<String, Group> = std::collections::BTreeMap::new();
    for acct in accounts {
        let name = acct
            .get("PackageName")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or("积分包")
            .to_string();
        let (size, remain) = match (
            precise(acct.get("CycleCapacitySizePrecise")),
            precise(acct.get("CycleCapacityRemainPrecise")),
        ) {
            (size, remain) if size > 0.0 => (size, remain.clamp(0.0, size)),
            _ => (
                precise(acct.get("CapacitySizePrecise")),
                precise(acct.get("CapacityRemainPrecise")),
            ),
        };
        if size <= 0.0 {
            continue;
        }
        let group = groups.entry(name).or_insert(Group { size: 0.0, remain: 0.0, resets_at_ms: 0 });
        group.size += size;
        group.remain += remain;
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
        .map(|(name, g)| crate::QuotaSample {
            used_percent: ((1.0 - g.remain / g.size) * 100.0).clamp(0.0, 100.0),
            window_minutes: 0,
            resets_at_ms: g.resets_at_ms,
            label: Some(format!("{name} · 已用 {}/{}", trim(g.size - g.remain), trim(g.size))),
            // The pack name is the window's stable identity — grouping is by
            // name, so it is unique per sample. A shared id makes the notify
            // machine see one window flapping between tiers: the live pack's
            // quiet reading re-arms the exhausted pack's spent state through
            // the same key, and the 已用完 banner re-fires on every poll
            // (measured 2026-10-10).
            id: Some(name),
        })
        .collect()
}

/// A number the wire carries as a string (`"500"`, `"492.29000001"`) — and
/// occasionally as a real JSON number, which parses just the same.
fn precise(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `CycleEndTime` is a UTC+8 wall clock with no offset on the wire — the same
/// convention WorkBuddy's packs carry.
fn parse_cycle_end(raw: &str) -> Option<i64> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S").ok()?;
    let zone = chrono::FixedOffset::east_opt(8 * 3600)?;
    Some(zone.from_local_datetime(&naive).single()?.timestamp_millis())
}

/// Whole when whole, one decimal when fractional (same rule as WorkBuddy's).
fn trim(n: f64) -> String {
    if n.fract() == 0.0 { format!("{n:.0}") } else { format!("{n:.1}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This machine's account answered exactly this on 2026-10-10 (fields the
    /// probe does not read trimmed): one live pack mid-cycle and two exhausted
    /// ones — the exhausted pair must group away from the live pack's percent,
    /// not dilute it.
    const MEASURED: &str = r#"{"code":0,"msg":"OK","requestId":"r","data":{"Response":{"Data":{"Accounts":[
        {"PackageName":"CodeBuddy个人体验版","CapacityRemainPrecise":"500","CapacityUsedPrecise":"0","CapacitySizePrecise":"500","CycleCapacityRemainPrecise":"492.29000001","CycleCapacitySizePrecise":"500","CycleEndTime":"2026-10-31 23:59:59","Status":0},
        {"PackageName":"CodeBuddy个人版国内运营裂变包","CapacityRemainPrecise":"0","CapacityUsedPrecise":"1000","CapacitySizePrecise":"1000","CycleCapacityRemainPrecise":"0","CycleCapacitySizePrecise":"1000","CycleEndTime":"2027-04-29 14:52:10","Status":3},
        {"PackageName":"CodeBuddy个人版国内运营裂变包","CapacityRemainPrecise":"0","CapacityUsedPrecise":"3000","CapacitySizePrecise":"3000","CycleCapacityRemainPrecise":"0","CycleCapacitySizePrecise":"3000","CycleEndTime":"2027-04-29 14:52:14","Status":3}
      ]}}}}"#;

    #[test]
    fn the_measured_answer_groups_live_and_exhausted_packs_apart() {
        let samples = samples_from_resource(&serde_json::from_str(MEASURED).unwrap());
        assert_eq!(samples.len(), 2, "one group per package name");
        let trial = samples.iter().find(|s| s.label.as_deref().is_some_and(|l| l.contains("体验版"))).unwrap();
        assert_eq!(trial.id.as_deref(), Some("CodeBuddy个人体验版"), "the pack name is the stable window id");
        assert!((trial.used_percent - (1.0 - 492.29 / 500.0) * 100.0).abs() < 0.01);
        assert_eq!(trial.label.as_deref(), Some("CodeBuddy个人体验版 · 已用 7.7/500"));
        assert!(trial.resets_at_ms > 0, "a live pack carries its cycle expiry");
        let spent = samples.iter().find(|s| s.label.as_deref().is_some_and(|l| l.contains("裂变包"))).unwrap();
        assert_eq!(spent.used_percent, 100.0, "both exhausted packs sum into one group");
        assert_eq!(spent.id.as_deref(), Some("CodeBuddy个人版国内运营裂变包"));
        assert_ne!(spent.id, trial.id, "two packs must never share one window identity");
        assert_eq!(spent.label.as_deref(), Some("CodeBuddy个人版国内运营裂变包 · 已用 4000/4000"));
        assert_eq!(spent.resets_at_ms, 0, "a pack with nothing left has no reset worth naming");
    }

    #[test]
    fn missing_accounts_and_bad_numbers_are_silence() {
        assert!(samples_from_resource(&serde_json::json!({"data": {"Response": {"Data": {}}}})).is_empty());
        assert!(samples_from_resource(&serde_json::json!({})).is_empty());
        // A numeric capacity parses the same as a string one.
        let numeric = serde_json::json!({"data": {"Response": {"Data": {"Accounts": [
            {"PackageName":"P","CapacitySizePrecise":100,"CapacityRemainPrecise":50,"CycleEndTime":"2026-11-01 00:00:00"}
        ]}}}});
        let samples = samples_from_resource(&numeric);
        assert_eq!(samples.len(), 1);
        assert!((samples[0].used_percent - 50.0).abs() < 0.001);
    }
}

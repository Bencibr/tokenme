//! Domestic WorkBuddy credits and the daily check-in grant. The international
//! WorkBuddy AI account is a different probe and never enters this claim path.
//! Wire reference: https://github.com/88lin/workbuddy-auto-signin (MIT),
//! verified against the desktop broker: status first, then claim if eligible.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::TimeZone;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use usage_core::QuotaSample;

use crate::QuotaProbe;
use super::{workbuddy, workbuddy_auth, workbuddy_wbipc};

pub struct WorkBuddyCnQuota;
const METER: &str = "/billing/meter/get-user-resource";
const STATUS: &str = "/v2/billing/meter/checkin-activity-status";
const CLAIM: &str = "/v2/billing/meter/daily-checkin";
static CHECKIN_LOCK: Mutex<()> = Mutex::new(());

impl QuotaProbe for WorkBuddyCnQuota {
    fn tool(&self) -> &'static str { "workbuddy_cn" }

    fn fetch(&self) -> Vec<QuotaSample> {
        let mut transport = Transport::default();
        let account = account_key();
        let (day, reset) = beijing_day(chrono::Utc::now());
        let checkin = {
            let _guard = CHECKIN_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            daily_checkin(state_path().as_deref(), &account, &day, false, |path| transport.request(path, &json!({})))
        };
        meter(&mut transport, checkin.is_ok_and(|(done, _)| done), reset)
    }
}

#[derive(Default)]
struct Transport { direct: Option<Option<Value>> }

impl Transport {
    fn request(&mut self, path: &str, body: &Value) -> Option<(u16, Value)> {
        if let Some(answer) = workbuddy_wbipc::request(true, path, body) { return Some(answer); }
        self.direct_request(path, body)
    }

    fn direct_request(&mut self, path: &str, body: &Value) -> Option<(u16, Value)> {
        let info = self.direct.get_or_insert_with(|| {
            workbuddy_auth::credentials(true).or_else(workbuddy::saved_login_auth)
                .or_else(bare_auth)
                .filter(|v| workbuddy_auth::matches_edition(v, true))
        }).as_ref()?;
        if info.pointer("/auth/expiresAt").and_then(Value::as_i64)
            .is_some_and(|ms| ms <= chrono::Utc::now().timestamp_millis()) { return None; }
        let token = info.pointer("/auth/accessToken")?.as_str().filter(|t| !t.is_empty())?;
        let uid = info.pointer("/account/uid").and_then(Value::as_str).unwrap_or_default();
        let domain = info.pointer("/auth/domain").and_then(Value::as_str).unwrap_or("www.workbuddy.cn");
        let enterprise = info.pointer("/account/enterpriseId").and_then(Value::as_str).unwrap_or_default();
        // Fixed vendor origins: metadata in an auth record cannot redirect a token.
        let base = if path == METER { "https://www.workbuddy.cn" } else { "https://copilot.tencent.com" };
        let req = crate::http::agent_builder().timeout_connect(Duration::from_secs(3)).timeout_read(Duration::from_secs(6))
            .redirects(0).build().post(&format!("{base}{path}"))
            .set("authorization", &format!("Bearer {token}"))
            .set("content-type", "application/json").set("accept", "application/json")
            .set("x-user-id", uid).set("x-domain", domain).set("user-agent", "WorkBuddy")
            .set("x-enterprise-id", enterprise).set("x-tenant-id", enterprise);
        let resp = match req.send_json(body) {
            Ok(resp) | Err(ureq::Error::Status(_, resp)) => resp,
            Err(err) => {
                #[cfg(test)] eprintln!("WorkBuddy direct API transport failed: {err}");
                let _ = err;
                return None;
            }
        };
        Some((resp.status(), resp.into_json().ok()?))
    }
}

fn bare_auth() -> Option<Value> {
    let token = std::env::var("WORKBUDDY_CN_TOKEN").ok().or_else(|| {
        std::fs::read_to_string(dirs::config_dir()?.join("tokenme/workbuddy-cn.token")).ok()
    })?;
    let token = token.trim();
    (!token.is_empty()).then(|| json!({"auth":{"accessToken":token,"domain":"www.workbuddy.cn"}}))
}

fn account_key() -> String {
    let info = workbuddy_auth::record(true).or_else(workbuddy::saved_login_auth)
        .filter(|v| workbuddy_auth::matches_edition(v, true)).or_else(bare_auth);
    let identity = info.as_ref().and_then(|v| v.pointer("/account/uid").or_else(|| v.pointer("/uid")))
        .and_then(Value::as_str).filter(|s| !s.is_empty())
        .or_else(|| info.as_ref()?.pointer("/auth/accessToken")?.as_str());
    identity.map(|id| format!("{:x}", Sha256::digest(id.as_bytes()))).unwrap_or_default()
}

fn meter(transport: &mut Transport, done: bool, reset: i64) -> Vec<QuotaSample> {
    let body = json!({"PageNumber":1,"PageSize":200,"ProductCode":"p_tcaca","Status":[0,3],"OnlyValidPeriod":true,"NeedInUsage":true});
    let Some((status, answer)) = transport.request(METER, &body) else { return Vec::new() };
    if !successful(status, &answer) { return Vec::new(); }
    let mut samples = workbuddy::samples_from_resource(&answer);
    // A failed meter leaves the last bars cached; the claim state is persisted
    // separately and becomes visible with the next successful meter read.
    if done && !samples.is_empty() {
        samples.push(QuotaSample { used_percent:0.0, window_minutes:0, resets_at_ms:reset,
            label:Some("今日已签到".into()), id:Some("checkin".into()) });
    }
    samples
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct CheckinState { account: String, day: String }

fn state_path() -> Option<PathBuf> {
    std::env::var_os("WORKBUDDY_CN_CHECKIN_STATE").filter(|v| !v.is_empty()).map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|d| d.join("tokenme/quota/workbuddy_cn_checkin.json")))
}

fn beijing_day(now: chrono::DateTime<chrono::Utc>) -> (String, i64) {
    let zone = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    let local = now.with_timezone(&zone);
    let tomorrow = local.date_naive().succ_opt().unwrap().and_hms_opt(0,0,0).unwrap();
    (local.format("%Y-%m-%d").to_string(), zone.from_local_datetime(&tomorrow).single().unwrap().timestamp_millis())
}

fn successful(status: u16, body: &Value) -> bool {
    (200..300).contains(&status) && body.get("code").is_none_or(|c| c.as_i64() == Some(0))
}

fn payload(body: &Value) -> &Value { body.get("data").unwrap_or(body) }

fn daily_checkin(
    path: Option<&Path>, account: &str, today: &str, force: bool,
    mut request: impl FnMut(&str) -> Option<(u16, Value)>,
) -> Result<(bool, String), String> {
    let state: CheckinState = path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
    if !force && !account.is_empty() && state.account == account && state.day == today {
        return Ok((true, "今日已签到".into()));
    }

    // Master switch off and not an explicit click: nothing leaves, and the
    // pack renders unclaimed rather than speaking for a claim never made.
    if !force && !crate::auto_checkin_enabled() {
        return Ok((false, "自动签到已关闭".into()));
    }
    let (status, body) = request(STATUS).ok_or("无法读取 WorkBuddy 签到状态，请确认已登录国内版")?;
    if !successful(status, &body) { return Err("WorkBuddy 签到状态读取失败，请确认登录有效".into()); }
    let data = payload(&body);
    if data.get("active") == Some(&Value::Bool(false)) { return Ok((false, "签到活动未开启".into())); }
    let checked = data.get("today_checked_in");
    let done = checked == Some(&Value::Bool(true)) || checked.and_then(Value::as_i64) == Some(1);
    let message = if done { "今日已签到".into() } else {
        // Unknown or incomplete status never authorizes a claim.
        if data.get("active") != Some(&Value::Bool(true)) ||
            !(checked == Some(&Value::Bool(false)) || checked.and_then(Value::as_i64) == Some(0)) {
            return Err("WorkBuddy 签到状态格式已变化，暂未领取".into());
        }
        let (status, body) = request(CLAIM).ok_or("WorkBuddy 签到请求未确认，下次自动重试")?;
        if status == 401 || status == 403 { return Err("WorkBuddy 登录或签到权限已失效".into()); }
        if (status == 400 || (200..300).contains(&status)) && body.get("code").and_then(Value::as_i64) == Some(10001) {
            "今日已签到".into()
        } else if successful(status, &body) {
            let data = payload(&body);
            if data.is_null() { "今日已签到".into() } else {
                let credit = data.get("credit").and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
                    .filter(|n| n.is_finite() && *n >= 0.0).ok_or("WorkBuddy 未确认签到成功")?;
                format!("签到成功，领取 {credit} 积分")
            }
        } else { return Err("WorkBuddy 签到失败，稍后自动重试".into()); }
    };
    if let Some(path) = path.filter(|_| !account.is_empty()) {
        if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
        if let Ok(text) = serde_json::to_string(&CheckinState { account:account.into(), day:today.into() }) {
            let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&tmp, text).is_ok() { let _ = usage_core::replace_file(&tmp, path); }
        }
    }
    Ok((true, message))
}

pub fn manual_checkin() -> Result<(bool, String), String> {
    let _guard = CHECKIN_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut transport = Transport::default();
    let (day, reset) = beijing_day(chrono::Utc::now());
    let result = daily_checkin(state_path().as_deref(), &account_key(), &day, true, |path| transport.request(path, &json!({})))?;
    if result.0 {
        let samples = meter(&mut transport, true, reset);
        if !samples.is_empty() {
            if let Some(cache) = crate::cache::Cache::open("tokenme/quota", crate::TTL) { cache.store("workbuddy_cn", &samples); }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check-in master switch is the auto path's gate: off, the automatic
    /// claim answers before any request leaves; an explicit click (force)
    /// still reaches the transport. Serialized on the fetch lock because the
    /// flag is process-global.
    #[test]
    fn the_master_switch_stops_the_auto_claim_before_any_request() {
        let _serial = CHECKIN_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(crate::auto_checkin_enabled(), "the switch defaults on");
        crate::set_auto_checkin_enabled(false);
        let mut calls = 0;
        let auto = daily_checkin(None, "acc", "2026-10-12", false, |_| { calls += 1; None });
        let forced = daily_checkin(None, "acc", "2026-10-12", true, |_| { calls += 1; None });
        crate::set_auto_checkin_enabled(true);
        assert_eq!(auto, Ok((false, "自动签到已关闭".into())));
        assert_eq!(calls, 1, "off stops the auto path; the forced click still went out");
        assert!(forced.is_err(), "the dead transport proves the forced request left");
    }

    #[test]
    #[ignore = "claims today's credits on the signed-in domestic WorkBuddy account"]
    fn live_cn_checkin_and_separate_quota_rows() {
        let mut transport = Transport::default();
        let (_, before) = transport.request(STATUS, &json!({})).expect("CN broker status unavailable");
        eprintln!("WorkBuddy CN before: active={}, checked_in={}", payload(&before)["active"], payload(&before)["today_checked_in"]);
        let result = manual_checkin().expect("CN claim failed");
        assert!(result.0, "{}", result.1);
        eprintln!("WorkBuddy CN claim: {}", result.1);
        let cn = WorkBuddyCnQuota.fetch();
        assert!(cn.iter().any(|s| s.id.as_deref() == Some("checkin")), "CN claim marker missing");
        assert!(cn.iter().any(|s| s.id.as_deref() != Some("checkin")), "CN credit packages missing");
        for sample in &cn { eprintln!("WorkBuddy CN: {}", sample.label.as_deref().unwrap_or_default()); }
        let global = workbuddy::WorkBuddyQuota.fetch();
        assert!(!global.is_empty(), "international credits unavailable");
        assert!(global.iter().all(|s| s.id.as_deref() != Some("checkin")));
        for sample in &global { eprintln!("WorkBuddy AI: {}", sample.label.as_deref().unwrap_or_default()); }
        let (_, after) = transport.request(STATUS, &json!({})).expect("CN status roundtrip failed");
        assert_eq!(payload(&after)["today_checked_in"], true);
    }

    #[test]
    #[ignore = "reads the installed runtime and makes account API calls without the broker"]
    fn live_native_credentials_read_quota_without_the_broker() {
        let info = workbuddy_auth::credentials(true).expect("native CN credential unavailable");
        let mut transport = Transport { direct:Some(Some(info)) };
        let (status, answer) = transport.direct_request(STATUS, &json!({})).expect("native CN credential or direct status failed");
        assert!(successful(status, &answer));
        let body = json!({"PageNumber":1,"PageSize":200,"ProductCode":"p_tcaca","Status":[0,3],"OnlyValidPeriod":true,"NeedInUsage":true});
        let (status, answer) = transport.direct_request(METER, &body).expect("direct meter failed");
        assert!(successful(status, &answer));
        assert!(!workbuddy::samples_from_resource(&answer).is_empty());
        assert!(workbuddy_auth::credentials(false).is_some(), "international native credential failed");
        eprintln!("Both native credentials opened; CN status and credit packages read without wbipc.");
    }

    #[test]
    fn a_confirmed_day_is_cached_per_account_and_day_only() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("state.json");
        let mut calls = Vec::new();
        let answer = daily_checkin(Some(&path), "account-a", "2026-10-10", false, |p| {
            calls.push(p.to_string());
            Some((200, if p == STATUS { json!({"code":0,"data":{"active":true,"today_checked_in":false}}) }
                else { json!({"code":0,"data":{"credit":100}}) }))
        }).unwrap();
        assert!(answer.0); assert!(answer.1.contains("100")); assert_eq!(calls, [STATUS, CLAIM]);
        assert!(daily_checkin(Some(&path), "account-a", "2026-10-10", false, |_| panic!("same day re-claimed")).unwrap().0);
        for (account, day, force) in [("account-b","2026-10-10",false),("account-a","2026-10-11",false),("account-a","2026-10-10",true)] {
            assert!(daily_checkin(Some(&path), account, day, force, |_| None).is_err());
        }
    }

    #[test]
    fn inactive_unknown_and_failed_status_never_claim() {
        for (status, body) in [(200,json!({"data":{"active":false}})), (200,json!({"data":{}})),
            (200,json!({"code":7,"data":{"active":true,"today_checked_in":false}})), (401,json!({}))] {
            let mut count = 0;
            let _ = daily_checkin(None, "a", "day", false, |p| { assert_eq!(p, STATUS); count += 1; Some((status,body.clone())) });
            assert_eq!(count, 1);
        }
    }

    #[test]
    fn only_confirmed_claims_mark_the_day_and_auth_rejection_wins() {
        for (status, claim, ok) in [(400,json!({"code":10001}),true),(200,Value::Null,true),
            (200,json!({"code":0,"data":{"credit":100}}),true),(401,json!({"code":10001}),false),
            (403,json!({"credit":100}),false),(500,Value::Null,false),(200,json!({"code":9,"data":{"credit":100}}),false),
            (200,json!({}),false)] {
            let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("state.json");
            let result = daily_checkin(Some(&path), "a", "day", false, |p| Some(if p == STATUS {
                (200,json!({"data":{"active":true,"today_checked_in":false}}))
            } else { (status,claim.clone()) }));
            assert_eq!(result.is_ok_and(|(done,_)| done), ok, "{status}: {claim}");
            assert_eq!(path.exists(), ok);
        }
    }

    #[test]
    fn the_claim_day_resets_at_beijing_midnight() {
        let before = chrono::DateTime::parse_from_rfc3339("2026-10-10T15:59:59Z").unwrap().with_timezone(&chrono::Utc);
        let (day, reset) = beijing_day(before);
        assert_eq!(day, "2026-10-10");
        assert_eq!(reset, (before + chrono::Duration::seconds(1)).timestamp_millis());
        assert_eq!(beijing_day(before + chrono::Duration::seconds(1)).0, "2026-10-11");
    }
}

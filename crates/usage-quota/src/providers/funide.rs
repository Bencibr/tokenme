//! FunIDE: the cloud points balance behind the IDE's GLM-plan models.
//!
//! ## Where the numbers live
//!
//! The FunIDE cloud (`https://fun.quqimeng.com/ide/api/v1`) bills its plan
//! models in points, and the account state is reachable with the IDE's own
//! access token — stored *in plain text* (a JWT) in the VS Code secret table:
//! `<data>/FunIDE/User/globalStorage/state.vscdb`, ItemTable key
//! `funide.cloud.accessToken`. Measured live 2026-09-29:
//!
//! ```text
//! GET /ide/api/v1/billing/balance   (Bearer <jwt>)
//! → {"ok":true,"data":{"points":10.04,"total_consumed":0.056,
//!    "checkin":{"open":true,"today_checked":false,…}}}
//! POST /ide/api/v1/billing/checkin  (Bearer <jwt>, {})
//! → {"ok":true,"data":{"points":0.1,"balance":10.14,…}}
//! ```
//!
//! The token is short-lived (2 h, refreshed by the IDE); an expired or absent
//! token answers `Vec::new()` — opening FunIDE refreshes it and the probe
//! resumes on a later pass.
//!
//! ## How a points balance becomes a bar
//!
//! Points are pay-as-you-go with check-in grants on top of recharges, so the
//! bar reads against the account's own turnover: used = consumed /
//! (remaining + consumed). The label carries the remaining figure.

use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use usage_core::QuotaSample;

use crate::{auto_checkin_enabled, http::{get_json, post_json}, QuotaProbe};

const BALANCE_URL: &str = "https://fun.quqimeng.com/ide/api/v1/billing/balance";
const CHECKIN_URL: &str = "https://fun.quqimeng.com/ide/api/v1/billing/checkin";
const TOKEN_KEY: &str = "funide.cloud.accessToken";

pub struct FunIdeQuota;

impl QuotaProbe for FunIdeQuota {
    fn tool(&self) -> &'static str {
        "funide"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(token) = access_token() else { return Vec::new() };
        let Some(mut body) = balance(&token) else { return Vec::new() };
        if auto_checkin_enabled() && checkin_state(&body).is_some_and(|state| state.open && !state.checked) {
            if let Ok((true, message, refreshed)) = claim(&token, &body) {
                crate::log_checkin(&format!("funide: {message} (auto)"));
                body = refreshed;
            }
        }
        samples_from(&body)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CheckinState {
    open: bool,
    checked: bool,
}

fn balance(token: &str) -> Option<Value> {
    let authorization = format!("Bearer {token}");
    get_json(BALANCE_URL, &[("authorization", authorization.as_str()), ("accept", "application/json")])
}

fn checkin_state(body: &Value) -> Option<CheckinState> {
    if body.get("ok").and_then(Value::as_bool) != Some(true) { return None; }
    let checkin = body.pointer("/data/checkin")?;
    Some(CheckinState {
        open: checkin.get("open").and_then(Value::as_bool)?,
        checked: checkin.get("today_checked").and_then(Value::as_bool)?,
    })
}

/// Claim only after the vendor explicitly says today's activity is open and
/// unclaimed, then confirm the POST with a fresh balance response. A successful
/// HTTP status without `today_checked:true` is not enough to paint 已签到.
fn claim(token: &str, body: &Value) -> Result<(bool, String, Value), String> {
    let state = checkin_state(body).ok_or_else(|| "FunIDE 签到状态格式已变化，暂未领取".to_string())?;
    if state.checked { return Ok((true, "今日已签到".into(), body.clone())); }
    if !state.open { return Ok((false, "签到活动未开启".into(), body.clone())); }
    let authorization = format!("Bearer {token}");
    let response = post_json(CHECKIN_URL, &[("authorization", authorization.as_str()), ("accept", "application/json")], serde_json::json!({}))
        .ok_or_else(|| "FunIDE 签到请求未确认，下次自动重试".to_string())?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("FunIDE 签到失败，稍后自动重试".into());
    }
    let refreshed = balance(token).ok_or_else(|| "FunIDE 签到请求已发送，但未能确认结果".to_string())?;
    if checkin_state(&refreshed).is_some_and(|state| state.checked) {
        let points = response.pointer("/data/points").and_then(number).unwrap_or(0.0);
        return Ok((true, format!("签到成功，+{points:.2} 积分"), refreshed));
    }
    Err("FunIDE 签到请求已发送，但未能确认结果".into())
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.trim().parse::<f64>().ok()).filter(|n| n.is_finite())
}

fn samples_from(body: &Value) -> Vec<QuotaSample> {
    let mut out = sample_from(body).into_iter().collect::<Vec<_>>();
    if checkin_state(body).is_some_and(|state| state.checked) {
        out.push(QuotaSample { used_percent: 0.0, window_minutes: 0, resets_at_ms: 0, label: Some("今日已签到".into()), id: Some("checkin".into()) });
    }
    out
}

pub fn manual_checkin() -> Result<(bool, String), String> {
    let token = access_token().ok_or_else(|| "未找到 FunIDE 登录".to_string())?;
    let body = balance(&token).ok_or_else(|| "无法读取 FunIDE 签到状态，请确认已登录".to_string())?;
    let (ok, message, refreshed) = claim(&token, &body)?;
    if ok {
        if let Some(cache) = crate::cache::Cache::open("tokenme/quota", crate::TTL) {
            cache.store("funide", &samples_from(&refreshed));
        }
    }
    Ok((ok, message))
}

/// `<data>/FunIDE/User/globalStorage/state.vscdb`, then the plaintext token.
/// `$FUNIDE_DATA_DIR` is tokenme's override; the IDE documents none.
fn access_token() -> Option<String> {
    let path = state_db()?;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let value: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            rusqlite::params![TOKEN_KEY],
            |r| r.get(0),
        )
        .ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn state_db() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FUNIDE_DATA_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        let p = dir.join("User").join("globalStorage").join("state.vscdb");
        return p.is_file().then_some(p);
    }
    let base = dirs::data_dir()?;
    let p = base
        .join("FunIDE")
        .join("User")
        .join("globalStorage")
        .join("state.vscdb");
    p.is_file().then_some(p)
}

/// `ok:true` gates the answer; the bar reads against the account's turnover
/// (points + consumed), since check-in grants sit on top of recharges.
fn sample_from(body: &Value) -> Option<QuotaSample> {
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let data = body.get("data")?;
    let num = |k: &str| {
        data.get(k)
            .and_then(Value::as_f64)
            .or_else(|| data.get(k)?.as_str()?.trim().parse::<f64>().ok())
            .unwrap_or(0.0)
            .max(0.0)
    };
    let points = num("points");
    let consumed = num("total_consumed");
    if points + consumed <= 0.0 {
        return None;
    }
    let used_percent = (consumed / (points + consumed) * 100.0).clamp(0.0, 100.0);
    Some(QuotaSample {
        used_percent,
        window_minutes: 0,
        resets_at_ms: 0,
        label: Some(format!("积分 {points:.2}")),
        id: Some("funide-points".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r#"{"ok":true,"data":{"points":10.043741,"frozen_points":0,
        "total_recharged":0.1,"total_consumed":0.056259,"points_per_yuan":1,
        "billing_enabled":true}}"#;

    #[test]
    fn the_points_balance_becomes_a_bar_against_turnover() {
        let body: Value = serde_json::from_str(LIVE).unwrap();
        let s = sample_from(&body).expect("a funded account has a bar");
        assert_eq!(s.label.as_deref(), Some("积分 10.04"));
        assert!((s.used_percent - 0.56).abs() < 0.01, "0.056 of 10.10 turnover: {s:?}");
        assert_eq!(s.window_minutes, 0, "points are pay-as-you-go, no window");
        assert_eq!(s.id.as_deref(), Some("funide-points"));
    }

    #[test]
    fn failures_answer_nothing() {
        for raw in [
            r#"{"ok":false,"error":{"code":"AUTH"}}"#,
            r#"{"ok":true,"data":{"points":0,"total_consumed":0}}"#,
            "not json",
        ] {
            let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            assert!(sample_from(&v).is_none(), "{raw}");
        }
    }

    #[test]
    fn checkin_state_requires_explicit_open_and_today_status() {
        let body: Value = serde_json::from_str(r#"{"ok":true,"data":{"checkin":{"open":true,"today_checked":false}}}"#).unwrap();
        assert_eq!(checkin_state(&body), Some(CheckinState { open: true, checked: false }));
        let missing: Value = serde_json::from_str(r#"{"ok":true,"data":{"checkin":{"open":true}}}"#).unwrap();
        assert_eq!(checkin_state(&missing), None);
    }

    #[test]
    fn checked_balance_adds_the_ui_marker_without_faking_a_points_row() {
        let body: Value = serde_json::from_str(r#"{"ok":true,"data":{"points":10,"total_consumed":1,"checkin":{"open":true,"today_checked":true}}}"#).unwrap();
        let rows = samples_from(&body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].id.as_deref(), Some("checkin"));
        assert_eq!(rows[1].label.as_deref(), Some("今日已签到"));
    }
}

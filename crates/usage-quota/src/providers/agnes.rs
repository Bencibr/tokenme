//! AgnesCode (Agnes AI's agent IDE): the account's remaining membership points.
//!
//! ## The call
//!
//! The desktop app's own membership panel queries the vendor SaaS directly:
//!
//! ```text
//! GET https://api.agnes-ai.com/api/v2/subscription/credits-balance
//! headers: Authorization: Bearer <token>    X-User-Language: zh-Hans
//! → {code: 0|“000000”, data: {total_balance, time_sensitive_balance,
//!      permanent_balance, level, level_name}}
//! ```
//!
//! (endpoint + auth header + response shape read from the shipped renderer
//! bundle, `Partitions/agnes` webview code, 2026-09-29.)
//!
//! ## Where the token comes from — and what does not work
//!
//! The membership Bearer token is NOT on disk in readable form (checked
//! 2026-09-29): the agent home carries no token file, the keychain entry
//! `agnes` holds only the MODEL gateway key (`{"AGNES_AI_API_KEY": …}`, which
//! the membership endpoint rejects with 401 Login-expired — and that key IS
//! the membership credential: a JWT whose exp ran out on 2026-09-01. The
//! account's AgnesCode login session has simply expired; re-login inside
//! AgnesCode refreshes this very keychain entry and the probe picks the new
//! token up on its next pass with zero further work.
//!
//! What the probe ships today: a pasted token (`AGNES_TOKEN` env or
//! `tokenme/agnes.token`) is the working source; the keychain `agnes` entry is
//! also tried and parsed for an `AGNES_AI_API_KEY` for when the vendor accepts
//! the model key on this endpoint. No token at all answers an empty vec; the
//! user's own bars are never blanked by a probe.
//!
//! ## How a points pool becomes a bar
//!
//! The balance response carries only remaining balances — no granted total, so
//! an honest used-percent does not exist. The bar stays empty and the label
//! carries the real numbers: 总剩余、限时点数（到期作废的那部分）、会员档。

use serde_json::Value;
use usage_core::QuotaSample;

use crate::QuotaProbe;

pub struct AgnesQuota;

const BALANCE_PATH: &str = "/api/v2/subscription/credits-balance";
const DEFAULT_BASE: &str = "https://api.agnes-ai.com";

impl QuotaProbe for AgnesQuota {
    fn tool(&self) -> &'static str {
        "agnes"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(token) = token() else { return Vec::new() };
        let base = base_url();
        let Some(body) = crate::http::get_json(
            &format!("{base}{BALANCE_PATH}"),
            &[
                ("authorization", format!("Bearer {token}").as_str()),
                ("x-user-language", "zh-Hans"),
                ("accept", "application/json"),
            ],
        ) else {
            return Vec::new();
        };
        samples_from_balance(&body)
    }
}

/// The membership Bearer token: pasted token first (env, then file), then the
/// IDE's own keychain entry. `Err` from `security` (item missing, or the user
/// declining the one-time approval) is silence, never an error surfaced.
fn token() -> Option<String> {
    if let Some(raw) = std::env::var_os("AGNES_TOKEN") {
        let t = raw.to_string_lossy().trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    if let Some(dir) = dirs::config_dir() {
        let path = dir.join("tokenme").join("agnes.token");
        if let Ok(t) = std::fs::read_to_string(path) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", "agnes", "-w"])
        .output()
        .ok()?;
    let ok = out.status.success();
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    ok.then_some(text).filter(|t| !t.is_empty())
}

fn base_url() -> String {
    std::env::var("AGNES_API_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_BASE.to_string())
}

/// `code` 0 / "000000" both mean success (the wire is inconsistent).
fn ok_code(body: &Value) -> bool {
    matches!(
        body.get("code").map(|c| c == 0 || c == "000000"),
        Some(true)
    )
}

/// The balance answer → one bar. The vendor exposes remaining balances only —
/// no granted total — so the bar stays empty and the label is the payload.
fn samples_from_balance(body: &Value) -> Vec<QuotaSample> {
    if !ok_code(body) {
        return Vec::new();
    }
    let Some(data) = body.get("data") else { return Vec::new() };
    let Some(total) = data.get("total_balance").and_then(Value::as_f64) else {
        return Vec::new();
    };
    let time_sensitive = data
        .get("time_sensitive_balance")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let permanent = data
        .get("permanent_balance")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let tier = data
        .get("level_name")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            data.get("level").and_then(Value::as_i64).map(|level| {
                (match level {
                    0 => "免费版",
                    _ => "会员",
                })
                .to_string()
            })
        })
        .unwrap_or_else(|| "会员".to_string());
    let mut label = format!("剩余 {} 点 · 限时 {} · 每日刷新 {}", trim(total), trim(time_sensitive), trim(permanent));
    if !tier.is_empty() {
        label.push_str(&format!(" · {tier}"));
    }
    vec![QuotaSample {
        used_percent: 0.0,
        window_minutes: 0,
        resets_at_ms: 0,
        label: Some(label),
        id: Some("points".into()),
    }]
}

fn trim(n: f64) -> String {
    if n.fract() == 0.0 { format!("{n:.0}") } else { format!("{n:.1}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_balance_answer_becomes_one_points_bar() {
        let body = json!({
            "code": 0,
            "data": {
                "total_balance": 8578.0,
                "time_sensitive_balance": 1240.0,
                "permanent_balance": 7338.0,
                "level": 2,
                "level_name": "Pro"
            }
        });
        let s = samples_from_balance(&body);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].used_percent, 0.0, "no granted total exists — no honest percent");
        let label = s[0].label.as_deref().unwrap();
        assert!(label.contains("剩余 8578 点"), "{label}");
        assert!(label.contains("限时 1240"), "{label}");
        assert!(label.contains("每日刷新 7338"), "{label}");
        assert!(label.ends_with("Pro"), "{label}");
        assert_eq!(s[0].id.as_deref(), Some("points"));
    }

    #[test]
    fn string_zero_code_and_missing_fields_are_silence() {
        // The wire sometimes sends the code as a string.
        let body = json!({"code": "000000", "data": {"total_balance": 10.0}});
        assert_eq!(samples_from_balance(&body).len(), 1);
        assert!(samples_from_balance(&json!({"code": 401, "message": "unauthorized"})).is_empty());
        assert!(samples_from_balance(&json!({"code": 0})).is_empty(), "no data, no bar");
    }

    #[test]
    fn the_env_token_is_trimmed_and_wins_over_the_paste_file() {
        std::env::set_var("AGNES_TOKEN", "  env-token  ");
        assert_eq!(token().as_deref(), Some("env-token"));
        std::env::remove_var("AGNES_TOKEN");
        // The paste file is the next leg; the keychain leg after it asks for a
        // one-time macOS approval, so tests never walk that far.
        let paste = dirs::config_dir().map(|d| d.join("tokenme").join("agnes.token"));
        if let Some(paste) = paste {
            if let Some(parent) = paste.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&paste, "paste-token").unwrap();
            assert_eq!(token().as_deref(), Some("paste-token"));
            let _ = std::fs::remove_file(&paste);
        }
    }
}

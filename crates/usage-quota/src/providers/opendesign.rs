//! Open Design (open-design.ai) quota: the Design Plan's 5-hour and 7-day
//! allowance windows.
//!
//! The desktop app signs in to the Vela control plane and keeps the account's
//! machine-issued pair in `~/.amr/config.json` (`profiles.prod.controlKey`,
//! `profiles.prod.apiUrl`). The control key — `vela_ctrl_…` — is what reads the
//! plan; the runtime key beside it (`sk-amr-…`) is only for model requests and
//! the control plane refuses it, so it is never used here.
//!
//! The read is the one its own cloud dashboard and the vendor's reference
//! provider plugin (`opencode-opendesign-auth`) make:
//!
//! 1. `GET {apiUrl}/api/v1/workspaces/current` → `workspaceId`.
//! 2. `GET {apiUrl}/api/v1/workspaces/<id>/billing/preflight` →
//!    `codingPlan.windows[]`, each `{durationSeconds, usedCredits,
//!    limitCredits, resetsAt, …}`. The amounts arrive as decimal **strings**
//!    (`"374"` / `"50000"`), so they are parsed, not read as numbers.
//!
//! `eligible: false` means the account has no Design Plan allowance and its
//! requests draw on the wallet balance instead; that is reported as no windows,
//! because a wallet balance is not a window. The panel has no row shape for a
//! currency balance, so it is deliberately not reported here either.
//!
//! Window names follow the product's own card (`5 hours`, `7 days`), and the
//! panel translates the familiar `5 小时` / `7 天` labels.

use std::path::PathBuf;

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::http::get_json;
use crate::QuotaProbe;

/// Used when a profile names no origin of its own.
const DEFAULT_API: &str = "https://amr-api.open-design.ai";
const USER_AGENT: &str = concat!("tokenme/", env!("CARGO_PKG_VERSION"));

pub struct OpenDesignQuota;

impl QuotaProbe for OpenDesignQuota {
    fn tool(&self) -> &'static str {
        "opendesign"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        live().unwrap_or_default()
    }
}

fn live() -> Option<Vec<QuotaSample>> {
    let account = account()?;
    let workspaces = get(&format!("{}/api/v1/workspaces/current", account.api), &account.control_key)?;
    let id = workspaces
        .get("activeWorkspaceId")
        .or_else(|| workspaces.get("workspaceId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    let preflight = get(
        &format!("{}/api/v1/workspaces/{id}/billing/preflight", account.api),
        &account.control_key,
    )?;
    Some(samples_from(&preflight))
}

fn get(url: &str, key: &str) -> Option<Value> {
    let bearer = format!("Bearer {key}");
    get_json(
        url,
        &[
            ("authorization", bearer.as_str()),
            ("accept", "application/json"),
            ("user-agent", USER_AGENT),
        ],
    )
}

/// One signed-in desktop profile.
struct Account {
    control_key: String,
    api: String,
}

/// The desktop app's own config. `OPEN_DESIGN_HOME` points a test (or a
/// relocated install) at the directory holding `config.json`, defaulting to
/// `~/.amr`.
fn config_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("OPEN_DESIGN_HOME") {
        return Some(PathBuf::from(home).join("config.json"));
    }
    dirs::home_dir().map(|home| home.join(".amr").join("config.json"))
}

fn account() -> Option<Account> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    account_from(&value)
}

/// `profiles.prod` is the profile the app itself uses; any other profile is
/// only a fallback, and one without a control key cannot answer this probe.
fn account_from(value: &Value) -> Option<Account> {
    let profiles = value.get("profiles")?.as_object()?;
    let mut ordered: Vec<&Value> = Vec::new();
    if let Some(prod) = profiles.get("prod") {
        ordered.push(prod);
    }
    ordered.extend(profiles.iter().filter(|(name, _)| name.as_str() != "prod").map(|(_, profile)| profile));
    for profile in ordered {
        let Some(control_key) = profile.get("controlKey").and_then(Value::as_str).map(str::trim).filter(|key| !key.is_empty())
        else {
            continue;
        };
        let api = profile
            .get("apiUrl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| url.starts_with("https://"))
            .unwrap_or(DEFAULT_API)
            .trim_end_matches('/')
            .to_string();
        return Some(Account { control_key: control_key.to_string(), api });
    }
    None
}

/// The preflight's `codingPlan.windows[]`, shortest first. A plan that is not
/// eligible owns no allowance to draw, so it yields no rows.
pub(crate) fn samples_from(preflight: &Value) -> Vec<QuotaSample> {
    let Some(plan) = preflight.get("codingPlan") else {
        return Vec::new();
    };
    if plan.get("eligible").and_then(Value::as_bool) == Some(false) {
        return Vec::new();
    }
    let mut out: Vec<QuotaSample> = plan
        .get("windows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(window_sample)
        .collect();
    out.sort_by_key(|sample| sample.window_minutes);
    out
}

fn window_sample(window: &Value) -> Option<QuotaSample> {
    let seconds = amount(window.get("durationSeconds")?)?;
    let used = amount(window.get("usedCredits")?)?;
    let limit = amount(window.get("limitCredits")?)?;
    if seconds <= 0.0 || limit <= 0.0 {
        return None;
    }
    Some(QuotaSample {
        used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
        window_minutes: (seconds / 60.0).round() as i64,
        resets_at_ms: window
            .get("resetsAt")
            .and_then(Value::as_str)
            .and_then(parse_ts_ms)
            .unwrap_or(0),
        label: Some(window_label(seconds)),
        id: Some(format!("coding-plan-{}s", seconds.round() as i64)),
    })
}

/// Numbers arrive as decimal strings on this endpoint; a numeric form is
/// accepted too so a wire change does not blank the row.
fn amount(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// The vendor's card says "5 hours" / "7 days"; the panel translates these two
/// familiar labels, and an unusual plan gets its own true duration.
fn window_label(seconds: f64) -> String {
    let seconds = seconds.round() as i64;
    if seconds % 86_400 == 0 {
        format!("{} 天", seconds / 86_400)
    } else if seconds % 3_600 == 0 {
        format!("{} 小时", seconds / 3_600)
    } else {
        format!("{} 分钟", (seconds / 60).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Captured shape of a 200 from the preflight (fake numbers, real field
    /// names and string amounts).
    fn preflight() -> Value {
        json!({
            "schemaVersion": 1,
            "workspaceId": "mplv9rt6da1bhzvicju47w1c",
            "balanceUsd": "1.0000",
            "funding": "wallet",
            "codingPlan": {
                "eligible": true,
                "tier": "go",
                "windows": [
                    {
                        "policyId": "model_limit_policy_week",
                        "durationSeconds": 604800,
                        "usedCredits": "374",
                        "limitCredits": "150000",
                        "remainingCredits": "149626",
                        "resetsAt": "2026-10-27T12:02:22.359Z"
                    },
                    {
                        "policyId": "model_limit_policy_5h",
                        "durationSeconds": 18000,
                        "usedCredits": "374",
                        "limitCredits": "50000",
                        "remainingCredits": "49626",
                        "resetsAt": "2026-10-20T17:02:22.359Z"
                    }
                ]
            }
        })
    }

    #[test]
    fn both_windows_are_read_from_string_amounts_shortest_first() {
        let samples = samples_from(&preflight());
        assert_eq!(samples.len(), 2);
        assert_eq!(
            samples.iter().map(|s| (s.window_minutes, s.label.as_deref())).collect::<Vec<_>>(),
            vec![(300, Some("5 小时")), (10_080, Some("7 天"))],
            "sorted by window, not by the order the payload serialised"
        );
        assert!((samples[0].used_percent - 0.748).abs() < 1e-9);
        assert_eq!(samples[0].id.as_deref(), Some("coding-plan-18000s"));
        assert_eq!(
            samples[0].resets_at_ms,
            chrono::DateTime::parse_from_rfc3339("2026-10-20T17:02:22.359Z").unwrap().timestamp_millis()
        );
    }

    #[test]
    fn a_plan_without_an_allowance_reports_no_windows() {
        let mut body = preflight();
        body["codingPlan"]["eligible"] = json!(false);
        assert!(samples_from(&body).is_empty(), "wallet fallback is not a window");
        assert!(samples_from(&json!({"codingPlan": null})).is_empty());
        assert!(samples_from(&json!({})).is_empty());
    }

    #[test]
    fn malformed_windows_are_skipped_not_guessed() {
        let body = json!({"codingPlan": {"eligible": true, "windows": [
            {"durationSeconds": 0, "usedCredits": "1", "limitCredits": "2"},
            {"durationSeconds": 18000, "usedCredits": "1", "limitCredits": "0"},
            {"durationSeconds": "18000", "usedCredits": "1", "limitCredits": "4"},
            {"durationSeconds": 18000, "usedCredits": "nope", "limitCredits": "4"},
            {"nonsense": true}
        ]}});
        let samples = samples_from(&body);
        assert_eq!(samples.len(), 1, "only the complete window survives: {samples:?}");
        assert_eq!(samples[0].used_percent, 25.0);
    }

    #[test]
    fn the_prod_profile_wins_and_another_profile_is_the_fallback() {
        let value = json!({"profiles": {
            "dev": {"controlKey": "vela_ctrl_dev", "apiUrl": "https://dev.example"},
            "prod": {"controlKey": "vela_ctrl_prod", "apiUrl": "https://amr-api.open-design.ai/"}
        }});
        let account = account_from(&value).expect("a signed-in profile");
        assert_eq!(account.control_key, "vela_ctrl_prod");
        assert_eq!(account.api, "https://amr-api.open-design.ai", "the trailing slash is trimmed");

        let fallback = json!({"profiles": {"dev": {"controlKey": "vela_ctrl_dev"}}});
        let account = account_from(&fallback).expect("the only signed-in profile");
        assert_eq!(account.api, DEFAULT_API);

        let runtime_only = json!({"profiles": {"prod": {"runtimeKey": "sk-amr-x"}}});
        assert!(account_from(&runtime_only).is_none(), "a runtime key cannot read the plan");
        assert!(account_from(&json!({"profiles": {}})).is_none());
    }
}

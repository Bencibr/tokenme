//! GitHub Copilot premium-request quota.
//!
//! Copilot never writes its allowance anywhere on disk, so this is the only
//! source for it. The credential is the one the user already has: `gh auth token`,
//! falling back to `GH_TOKEN` / `GITHUB_TOKEN`. No token, no row.
//!
//! Request shape is `slkiser/opencode-quota:src/lib/copilot.ts:1209-1218` and
//! `nicezic/lean-quota-monitor:src/providers/copilot.ts:72-93` (identical, including
//! the `token` scheme: `copilot_internal/user` is the Copilot-internal API, which
//! authenticates the VS Code OAuth token this way). `Editor-Version` is load
//! bearing — without it the endpoint can answer `account_not_supported`/404. Both
//! references send `vscode/1.96.2` + `copilot-chat/0.26.7`.
//!
//! NOTE: there is no `copilot` usage adapter, so the id is not in
//! `usage_adapter_all::TOOL_IDS`; it rides `PROBE_ONLY_TOOLS` in `lib.rs`'s registry
//! test instead. The probe is registered (`providers::optional`) and runs everywhere
//! `gh` answers — which is why its spawn carries CREATE_NO_WINDOW.

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::http::get_json;
use crate::QuotaProbe;

const ENDPOINT: &str = "https://api.github.com/copilot_internal/user";
const EDITOR_VERSION: &str = "vscode/1.96.2";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
const USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
const API_VERSION: &str = "2025-04-01";

/// The allowance is a calendar-month bucket: `quota_reset_date_utc` is the day the
/// monthly entitlement rolls over
/// (`nicezic/lean-quota-monitor:docs/provider-acquisition-spec.md:751-760`).
const WINDOW_MINUTES: i64 = 43_200;
const LABEL: &str = "月度 Premium 请求";

pub struct CopilotQuota;

impl QuotaProbe for CopilotQuota {
    fn tool(&self) -> &'static str {
        "copilot"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        live().unwrap_or_default()
    }
}

fn live() -> Option<Vec<QuotaSample>> {
    let token = access_token()?;
    let body = get_json(
        ENDPOINT,
        &[
            ("authorization", &format!("token {token}")),
            ("accept", "application/json"),
            ("editor-version", EDITOR_VERSION),
            ("editor-plugin-version", EDITOR_PLUGIN_VERSION),
            ("user-agent", USER_AGENT),
            ("x-github-api-version", API_VERSION),
        ],
    )?;
    Some(samples_from(&body))
}

/// `quota_snapshots.premium_interactions` only: `chat` and `completions` are either
/// unmetered or not a window the user can hit. `percent_remaining` is the vendor's
/// own number, so `used_percent` is its complement; when it is absent the two raw
/// counters give the same ratio.
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    let Some(snapshot) = body
        .get("quota_snapshots")
        .and_then(|s| s.get("premium_interactions"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    // Unlimited means "this plan has no meter here"; a 0 % bar would be a lie.
    if snapshot.get("unlimited").and_then(Value::as_bool) == Some(true) {
        return Vec::new();
    }
    let entitlement = snapshot.get("entitlement").and_then(Value::as_f64);
    if entitlement == Some(0.0) {
        return Vec::new();
    }

    let remaining_percent = snapshot
        .get("percent_remaining")
        .and_then(Value::as_f64)
        .filter(|p| p.is_finite())
        .or_else(|| {
            let remaining = snapshot.get("remaining").and_then(Value::as_f64)?;
            Some((remaining / entitlement?) * 100.0)
        })
        .map(|p| p.clamp(0.0, 100.0));
    let Some(remaining_percent) = remaining_percent else { return Vec::new() };

    let resets_at_ms = body
        .get("quota_reset_date_utc")
        .or_else(|| body.get("quota_reset_date"))
        .and_then(Value::as_str)
        .and_then(parse_ts_ms)
        .unwrap_or(0);

    vec![QuotaSample {
        used_percent: 100.0 - remaining_percent,
        window_minutes: WINDOW_MINUTES,
        resets_at_ms,
        label: Some(LABEL.into()),
        id: None,
    }]
}

/// `gh auth token` first (it is the token the user actually logs in with), then the
/// env vars CI and shell profiles set.
fn access_token() -> Option<String> {
    gh_token()
        .or_else(|| env_token("GH_TOKEN").or_else(|| env_token("GITHUB_TOKEN")))
        .filter(|t| !t.is_empty())
}

fn env_token(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `None` when `gh` is missing or not logged in; its stderr is never surfaced, so
/// nothing that looks like a token can reach a log.
///
/// The console flag is not cosmetic: `gh` is a console binary, so on Windows every
/// TTL pass would otherwise get a visible console window through the default
/// terminal — a black box that steals focus once every few minutes, from a tray
/// app the user never asked to show anything. Same reason `atomcode` and the
/// Antigravity CLI carry it.
fn gh_token() -> Option<String> {
    let mut command = std::process::Command::new("gh");
    command.args(["auth", "token"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let out = command.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured shape of a 200 from `GET /copilot_internal/user` (token-free; the
    /// plan fields kept only as much as the parser reads).
    const OK_SAMPLE: &str = r#"{
      "user_login": "someone",
      "copilot_plan": "copilotpro",
      "quota_reset_date": "2026-10-01",
      "quota_reset_date_utc": "2026-10-01T00:00:00Z",
      "allow_premium_chat": true,
      "token_based_billing": {"select": false, "enabled": false},
      "quota_snapshots": {
        "chat": {"percent_remaining": 99, "unlimited": true},
        "premium_interactions": {
          "total_count": 12, "remaining": 30, "percent_remaining": 60,
          "entitlement": 50, "unlimited": false, "available": true, "overage_permitted": false
        }
      }
    }"#;

    #[test]
    fn percent_remaining_becomes_used_percent() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        let s = samples_from(&body);
        assert_eq!(s.len(), 1, "premium interactions are the one metered window");
        assert_eq!(s[0].used_percent, 40.0, "100 - 60 % remaining");
        assert_eq!(s[0].window_minutes, 43_200);
        assert_eq!(s[0].label.as_deref(), Some("月度 Premium 请求"));
    }

    #[test]
    fn the_calendar_reset_arrives_as_milliseconds() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        let s = samples_from(&body);
        assert_eq!(s[0].resets_at_ms, 1_790_812_800_000, "2026-10-01T00:00:00Z in ms");
    }

    #[test]
    fn remaining_over_entitlement_is_used_when_no_percent_is_reported() {
        let body: Value = serde_json::from_str(
            r#"{"quota_snapshots":{"premium_interactions":{"remaining":10,"entitlement":40}}}"#,
        )
        .unwrap();
        let s = samples_from(&body);
        assert_eq!(s[0].used_percent, 75.0, "(40-10)/40");
        assert_eq!(s[0].resets_at_ms, 0, "no reset date is unknown, not now");
    }

    #[test]
    fn unlimited_and_zero_entitlement_report_nothing_instead_of_a_fake_0_percent() {
        for json in [
            r#"{"quota_snapshots":{"premium_interactions":{"unlimited":true,"percent_remaining":0}}}"#,
            r#"{"quota_snapshots":{"premium_interactions":{"entitlement":0,"remaining":0,"percent_remaining":100}}}"#,
            r#"{"copilot_plan":"copilotfree","token_based_billing":{"enabled":true}}"#,
            r#"{"quota_snapshots":{"chat":{"unlimited":true}}}"#,
        ] {
            let body: Value = serde_json::from_str(json).unwrap();
            assert!(samples_from(&body).is_empty(), "{json} must not render a bar");
        }
    }

    #[test]
    fn junk_payloads_yield_nothing() {
        for json in [
            r#"{"message":"Bad credentials","status":"401"}"#,
            r#"{"quota_snapshots":{"premium_interactions":{"percent_remaining":"60"}}}"#,
            r#"{"quota_snapshots":null}"#,
        ] {
            let body: Value = serde_json::from_str(json).unwrap();
            assert!(samples_from(&body).is_empty(), "{json}");
        }
        assert!(samples_from(&Value::Null).is_empty());
    }

    #[test]
    fn env_tokens_are_trimmed_and_blanks_are_absent() {
        // Not a credential: a placeholder, so this test cannot leak anything.
        std::env::set_var("GH_TOKEN", "  ghp_placeholder_not_real  ");
        assert_eq!(env_token("GH_TOKEN").as_deref(), Some("ghp_placeholder_not_real"));
        std::env::set_var("GH_TOKEN", "   ");
        assert_eq!(env_token("GH_TOKEN"), None);
        std::env::remove_var("GH_TOKEN");
        std::env::remove_var("GITHUB_TOKEN");
    }

    #[test]
    #[ignore]
    fn live_usage_endpoint_answers_for_this_account() {
        if gh_token().is_none() && env_token("GH_TOKEN").is_none() && env_token("GITHUB_TOKEN").is_none() {
            println!("copilot: no credential on this machine (`gh auth status` reports no hosts, GH_TOKEN/GITHUB_TOKEN unset) -> no rows");
            return;
        }
        let samples = CopilotQuota.fetch();
        assert!(!samples.is_empty(), "token present but no premium_interactions window: plan without premium requests, or HTTP 401/403");
        for s in &samples {
            println!("copilot quota: {:.2}% window={} label={:?} resets_at_ms={}", s.used_percent, s.window_minutes, s.label, s.resets_at_ms);
        }
    }
}

//! Codex live rate limits from the ChatGPT backend.
//!
//! The Codex adapter already lifts `rate_limits` out of `~/.codex/sessions`, but
//! those lines only appear while a turn is running, so the panel can show a window
//! that has already reset. This probe asks the backend directly; the log stays the
//! authority for spend and this is the freshness backstop.
//!
//! Request and field names are `nicezic/lean-quota-monitor:src/providers/openai.ts:142-150`
//! (`docs/provider-acquisition-spec.md:100-160` documents the same call), cross-checked
//! against the live answer captured in the ignored test below:
//!
//! ```text
//! GET https://chatgpt.com/backend-api/wham/usage
//! Authorization: Bearer <~/.codex/auth.json tokens.access_token>
//! chatgpt-account-id: <~/.codex/auth.json tokens.account_id>   # when present
//! ```
//!
//! Nothing here logs in or refreshes the token: an expired `access_token` answers
//! 401 and yields no row.

use serde_json::Value;
use usage_core::QuotaSample;

use crate::http::get_json;
use crate::QuotaProbe;

const ENDPOINT: &str = "https://chatgpt.com/backend-api/wham/usage";
const USER_AGENT: &str = concat!("tokenme/", env!("CARGO_PKG_VERSION"));

pub struct CodexQuota;

impl QuotaProbe for CodexQuota {
    fn tool(&self) -> &'static str {
        "codex"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        live().unwrap_or_default()
    }
}

fn live() -> Option<Vec<QuotaSample>> {
    let (token, account_id) = credential()?;
    let mut headers = vec![
        ("authorization", format!("Bearer {token}")),
        ("accept", "application/json".into()),
        ("user-agent", USER_AGENT.into()),
    ];
    if let Some(id) = account_id {
        headers.push(("chatgpt-account-id", id));
    }
    let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let body = get_json(ENDPOINT, &borrowed)?;
    Some(samples_from(&body))
}

/// `reset_at` is unix **seconds**, `used_percent` is already 0-100, and the window
/// length arrives as `limit_window_seconds` (18 000 = 5 h, 604 800 = 1 week).
/// Unknown bucket names are kept: the container is scanned rather than hard-coded to
/// `primary_window`/`secondary_window`, because which slot holds which window is not
/// stable (`nicezic/lean-quota-monitor:docs/provider-acquisition-spec.md:186-190`).
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    // `rate_limit` is what the endpoint sends today; `rate_limits` is the name the
    // app-server RPC and older notes use, so both are accepted.
    let group = body
        .get("rate_limit")
        .or_else(|| body.get("rate_limits"))
        .and_then(Value::as_object)
        .or_else(|| body.as_object());
    let Some(group) = group else { return Vec::new() };

    let mut out = Vec::new();
    for (key, window) in group {
        let Some(window) = window.as_object() else { continue };
        let Some(used_percent) = window.get("used_percent").and_then(Value::as_f64) else { continue };
        let Some(seconds) = window.get("limit_window_seconds").and_then(Value::as_i64) else { continue };
        if seconds <= 0 {
            continue;
        }
        // Codex currently exposes the five-hour and weekly token windows. A
        // stale/extra 30-day bucket is not a Codex window and must not be
        // presented as one just because the wire payload contains a duration.
        let minutes = seconds / 60;
        if minutes >= 43_200 {
            continue;
        }
        out.push(QuotaSample {
            used_percent: used_percent.clamp(0.0, 100.0),
            window_minutes: minutes,
            resets_at_ms: unix_ms(window.get("reset_at").unwrap_or(&Value::Null)),
            label: Some(label_for(key, minutes)),
            id: None,
        });
    }
    out.sort_by_key(|s| s.window_minutes);
    out
}

/// Seconds or milliseconds, whichever arrived.
fn unix_ms(value: &Value) -> i64 {
    match value.as_i64().or_else(|| value.as_f64().map(|f| f as i64)) {
        Some(n) if n > 0 => {
            if n < 10_000_000_000 {
                n * 1000
            } else {
                n
            }
        }
        _ => 0,
    }
}

fn label_for(key: &str, minutes: i64) -> String {
    match minutes {
        300 => "5 小时".into(),
        1_440 => "天".into(),
        10_080 => "周".into(),
        43_200 => "月".into(),
        other => format!("{key} · {other} 分钟"),
    }
}

/// `$CODEX_HOME/auth.json` else `~/.codex/auth.json`; `tokens` holds the OAuth
/// envelope the CLI wrote at `codex login`
/// (`nicezic/lean-quota-monitor:src/providers/openai.ts:128-140`).
fn credential() -> Option<(String, Option<String>)> {
    let root = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex")))?;
    token_in(&std::fs::read_to_string(root.join("auth.json")).ok()?)
}

pub(crate) fn token_in(text: &str) -> Option<(String, Option<String>)> {
    let body: Value = serde_json::from_str(text).ok()?;
    let tokens = body.get("tokens")?.as_object()?;
    let token = tokens.get("access_token")?.as_str()?.trim();
    if token.is_empty() {
        return None;
    }
    let account = tokens.get("account_id").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty()).map(str::to_string);
    Some((token.to_string(), account))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured 200 body from `GET /backend-api/wham/usage` (account identifiers
    /// faked, every field name and unit is the real thing).
    const OK_SAMPLE: &str = r#"{
      "user_id": "user-FAKE0000",
      "account_id": "00000000-0000-0000-0000-000000000000",
      "email": "someone@example.com",
      "plan_type": "plus",
      "rate_limit": {
        "allowed": true,
        "limit_reached": false,
        "primary_window": {
          "used_percent": 21, "limit_window_seconds": 18000,
          "reset_after_seconds": 11655, "reset_at": 1790188292
        },
        "secondary_window": {
          "used_percent": 22, "limit_window_seconds": 604800,
          "reset_after_seconds": 562367, "reset_at": 1790739004
        }
      },
      "code_review_rate_limit": null,
      "additional_rate_limits": null,
      "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
      "rate_limit_reset_credits": {"available_count": 1}
    }"#;

    #[test]
    fn both_windows_come_back_shortest_first() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        let s = samples_from(&body);
        assert_eq!(
            s.iter().map(|x| (x.window_minutes, x.used_percent)).collect::<Vec<_>>(),
            vec![(300, 21.0), (10_080, 22.0)],
            "primary/secondary are not assumed to be 5h/week: the duration decides"
        );
        assert_eq!(s[0].label.as_deref(), Some("5 小时"));
        assert_eq!(s[1].label.as_deref(), Some("周"));
    }

    #[test]
    fn reset_at_is_unix_seconds_promoted_to_milliseconds() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        let s = samples_from(&body);
        assert_eq!(s[0].resets_at_ms, 1_790_188_292_000, "×1000, not passed through");
        assert_eq!(s[1].resets_at_ms, 1_790_739_004_000);
    }

    #[test]
    fn null_and_unknown_buckets_are_skipped_not_faked() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        assert_eq!(samples_from(&body).len(), 2, "the sibling null rate limits add nothing");
        let odd: Value = serde_json::from_str(
            r#"{"rate_limit":{"primary_window":{"used_percent":5,"limit_window_seconds":900,"reset_at":1790188292},
                 "allowed":true,"limit_reached":false,"whatever":{"used_percent":9}}}"#,
        )
        .unwrap();
        let s = samples_from(&odd);
        assert_eq!(s.len(), 1, "a window with no duration cannot be labelled, and `allowed` is not one");
        assert_eq!(s[0].window_minutes, 15);
        assert_eq!(s[0].label.as_deref(), Some("primary_window · 15 分钟"));
    }

    #[test]
    fn used_percent_is_clamped_and_junk_yields_nothing() {
        let body: Value = serde_json::from_str(
            r#"{"rate_limits":{"secondary_window":{"used_percent":140.5,"limit_window_seconds":604800}}}"#,
        )
        .unwrap();
        let s = samples_from(&body);
        assert_eq!(s[0].used_percent, 100.0);
        assert_eq!(s[0].resets_at_ms, 0, "no reset known, not a fake one");
        assert!(samples_from(&Value::Null).is_empty());
        assert!(samples_from(&serde_json::from_str(r#"{"rate_limit":null}"#).unwrap()).is_empty());
        assert!(samples_from(&serde_json::from_str(r#"{"rate_limit":{}}"#).unwrap()).is_empty());
    }

    #[test]
    fn a_monthly_bucket_is_not_a_codex_window() {
        let body: Value = serde_json::from_str(
            r#"{"rate_limit":{"primary_window":{"used_percent":12,"limit_window_seconds":18000,"reset_at":1790188292},"secondary_window":{"used_percent":99,"limit_window_seconds":2592000,"reset_at":1790739004}}}"#,
        )
        .unwrap();
        let samples = samples_from(&body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].window_minutes, 300);
        assert!(!samples.iter().any(|s| s.window_minutes >= 43_200));
    }

    #[test]
    fn the_credential_needs_both_a_token_and_keeps_the_account_id() {
        let (token, account) = token_in(r#"{"tokens":{"access_token":"  eyJfake.jwt.sig  ","account_id":"acct-1"}}"#).unwrap();
        assert_eq!(token, "eyJfake.jwt.sig", "trimmed, never logged");
        assert_eq!(account.as_deref(), Some("acct-1"));
        assert!(token_in(r#"{"tokens":{"access_token":""}}"#).is_none(), "an empty token is not a login");
        assert!(token_in(r#"{"OPENAI_API_KEY":"sk-x"}"#).is_none(), "an api-key-only file has no rate window");
        assert!(token_in("not json").is_none());
        let (_, missing) = token_in(r#"{"tokens":{"access_token":"t"}}"#).unwrap();
        assert_eq!(missing, None, "the account header is optional");
    }

    #[test]
    #[ignore]
    fn live_usage_endpoint_answers_for_this_account() {
        let samples = CodexQuota.fetch();
        assert!(!samples.is_empty(), "no ~/.codex/auth.json tokens.access_token, or it expired (401)?");
        for s in &samples {
            println!("codex quota: {:.2}% window={} label={:?} resets_at_ms={}", s.used_percent, s.window_minutes, s.label, s.resets_at_ms);
        }
    }
}

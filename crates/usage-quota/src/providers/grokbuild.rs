//! Grok Build / SuperGrok weekly pool from the CLI billing proxy.
//!
//! Grok Build's TUI `/usage` panel loads the shared SuperGrok weekly allowance
//! through the same upstream the CLI uses after `grok login`. Captured live on
//! this machine (and independently by claudia / GrokUsageBar):
//!
//! ```text
//! GET https://cli-chat-proxy.grok.com/v1/billing?format=credits
//! Authorization: Bearer <~/.grok/auth.json OIDC key>
//! X-XAI-Token-Auth: xai-grok-cli
//! ```
//!
//! `config.creditUsagePercent` is **used** (0–100). `config.currentPeriod.end`
//! is the reset. When `isUnifiedBillingUser` is true, Cursor / Grok Bot /
//! GrokChat draw from this same weekly window — `productUsage` is a share
//! breakdown, not a separate meter, so this probe emits one weekly sample.
//!
//! Nothing here logs in or refreshes the token: an expired `expires_at` or a
//! 401 yields no row. Grok owns refresh (`auth.json` stays read-only).

use serde_json::Value;
use usage_core::QuotaSample;

use crate::http::get_json;
use crate::QuotaProbe;

const ENDPOINT: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const USER_AGENT: &str = concat!("tokenme/", env!("CARGO_PKG_VERSION"));
const TOKEN_AUTH: &str = "xai-grok-cli";
const WEEK_MINUTES: i64 = 10_080;

pub struct GrokBuildQuota;

impl QuotaProbe for GrokBuildQuota {
    fn tool(&self) -> &'static str {
        "grokbuild"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        live().unwrap_or_default()
    }
}

fn live() -> Option<Vec<QuotaSample>> {
    let (token, identity) = credential()?;
    let headers = [
        ("authorization", format!("Bearer {token}")),
        ("x-xai-token-auth", TOKEN_AUTH.to_string()),
        ("accept", "application/json".into()),
        ("user-agent", USER_AGENT.into()),
    ];
    let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let body = get_json(ENDPOINT, &borrowed)?;
    let mut samples = samples_from(&body);
    for sample in &mut samples {
        sample.id = Some(super::account_window_id(&identity, sample.window_minutes));
    }
    if samples.is_empty() {
        None
    } else {
        Some(samples)
    }
}

/// `creditUsagePercent` is already used 0–100; the period type is weekly on
/// the measured SuperGrok accounts. Missing / out-of-range percent → no row
/// (never invent a number).
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    let Some(config) = body.get("config").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(used) = config.get("creditUsagePercent").and_then(Value::as_f64) else {
        return Vec::new();
    };
    // Negatives / NaN are refused; mild overshoot is clamped to 100.
    if !used.is_finite() || used < 0.0 {
        return Vec::new();
    }
    let resets_at_ms = period_end_ms(config).unwrap_or(0);
    let window_minutes = period_minutes(config).unwrap_or(WEEK_MINUTES);
    vec![QuotaSample {
        used_percent: used.clamp(0.0, 100.0),
        window_minutes,
        resets_at_ms,
        label: Some(label_for(window_minutes)),
        id: None,
    }]
}

fn period_end_ms(config: &serde_json::Map<String, Value>) -> Option<i64> {
    let period = config.get("currentPeriod").or_else(|| config.get("billingPeriodEnd"));
    // Prefer nested currentPeriod.end; fall back to billingPeriodEnd string.
    if let Some(end) = config
        .get("currentPeriod")
        .and_then(|p| p.get("end"))
        .and_then(Value::as_str)
    {
        return usage_core::parse_ts_ms(end);
    }
    if let Some(end) = config.get("billingPeriodEnd").and_then(Value::as_str) {
        return usage_core::parse_ts_ms(end);
    }
    let _ = period;
    None
}

fn period_minutes(config: &serde_json::Map<String, Value>) -> Option<i64> {
    let kind = config
        .get("currentPeriod")
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "USAGE_PERIOD_TYPE_WEEKLY" | "" => Some(WEEK_MINUTES),
        "USAGE_PERIOD_TYPE_DAILY" => Some(1_440),
        "USAGE_PERIOD_TYPE_MONTHLY" => Some(43_200),
        _ => {
            // Derive from start/end when the enum is unfamiliar.
            let start = config
                .get("currentPeriod")
                .and_then(|p| p.get("start"))
                .and_then(Value::as_str)
                .and_then(usage_core::parse_ts_ms)?;
            let end = period_end_ms(config)?;
            let minutes = (end - start) / 60_000;
            if minutes > 0 {
                Some(minutes)
            } else {
                None
            }
        }
    }
}

fn label_for(minutes: i64) -> String {
    match minutes {
        300 => "5 小时".into(),
        1_440 => "天".into(),
        10_080 => "周".into(),
        43_200 => "月".into(),
        other => format!("{other} 分钟"),
    }
}

/// `$GROK_HOME/auth.json` else `~/.grok/auth.json`. The file maps
/// `https://auth.x.ai::<client_id>` → `{ key, expires_at, … }`.
fn credential() -> Option<(String, String)> {
    // Match the usage adapter: trim `GROK_HOME`, treat blank as unset.
    let root = match std::env::var("GROK_HOME") {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                dirs::home_dir().map(|h| h.join(".grok"))?
            } else {
                std::path::PathBuf::from(trimmed)
            }
        }
        Err(_) => dirs::home_dir().map(|h| h.join(".grok"))?,
    };
    token_in(&std::fs::read_to_string(root.join("auth.json")).ok()?)
}

pub(crate) fn token_in(text: &str) -> Option<(String, String)> {
    let body: Value = serde_json::from_str(text).ok()?;
    let obj = body.as_object()?;
    // Prefer a non-expired OIDC entry; skip empty keys. Expired keys are
    // refused entirely — Grok owns refresh, and sending a dead bearer only
    // produces a 401 we would treat the same as "no row".
    for (account_key, entry) in obj {
        let Some(entry) = entry.as_object() else { continue };
        let Some(token) = entry.get("key").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        if let Some(expires) = entry.get("expires_at").and_then(Value::as_str) {
            if let Some(ms) = usage_core::parse_ts_ms(expires) {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                if ms <= now {
                    continue;
                }
            }
        }
        let identity = entry
            .get("user_id")
            .or_else(|| entry.get("email"))
            .or_else(|| entry.get("principal_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(account_key)
            .to_string();
        return Some((token.to_string(), identity));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured 200 body from `GET …/billing?format=credits` (account fields
    /// faked; every field name and unit is the real thing).
    const OK_SAMPLE: &str = r#"{
      "config": {
        "currentPeriod": {
          "type": "USAGE_PERIOD_TYPE_WEEKLY",
          "start": "2026-11-03T15:49:56.585+00:00",
          "end": "2026-11-10T15:49:56.585+00:00"
        },
        "creditUsagePercent": 5.0,
        "productUsage": [
          {"product": "GrokBot", "usagePercent": 2.0},
          {"product": "GrokChat", "usagePercent": 2.0},
          {"product": "Cursor", "usagePercent": 1.0}
        ],
        "isUnifiedBillingUser": true,
        "prepaidBalance": {"val": 0},
        "billingPeriodStart": "2026-11-03T15:49:56.585+00:00",
        "billingPeriodEnd": "2026-11-10T15:49:56.585+00:00"
      }
    }"#;

    #[test]
    fn weekly_window_from_credit_usage_percent() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        let s = samples_from(&body);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].used_percent, 5.0);
        assert_eq!(s[0].window_minutes, WEEK_MINUTES);
        assert_eq!(s[0].label.as_deref(), Some("周"));
        assert!(s[0].resets_at_ms > 0, "period end must parse");
        // productUsage is a share breakdown of the same pool — not extra rows.
    }

    #[test]
    fn used_percent_is_clamped_and_junk_yields_nothing() {
        let over: Value = serde_json::from_str(
            r#"{"config":{"creditUsagePercent":140.5,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-11-10T15:49:56.585+00:00"}}}"#,
        )
        .unwrap();
        assert_eq!(samples_from(&over)[0].used_percent, 100.0);
        assert!(samples_from(&Value::Null).is_empty());
        assert!(samples_from(&serde_json::from_str(r#"{"config":{}}"#).unwrap()).is_empty());
        assert!(samples_from(
            &serde_json::from_str(r#"{"config":{"creditUsagePercent":-1}}"#).unwrap()
        )
        .is_empty());
    }

    #[test]
    fn auth_json_needs_a_live_key() {
        let future = "2099-01-01T00:00:00Z";
        let past = "2020-01-01T00:00:00Z";
        let (token, id) = token_in(&format!(
            r#"{{"https://auth.x.ai::client":{{"key":"  eyJfake  ","expires_at":"{future}","user_id":"u-1"}}}}"#
        ))
        .unwrap();
        assert_eq!(token, "eyJfake");
        assert_eq!(id, "u-1");
        assert!(
            token_in(&format!(
                r#"{{"https://auth.x.ai::client":{{"key":"dead","expires_at":"{past}"}}}}"#
            ))
            .is_none(),
            "expired keys are refused"
        );
        assert!(token_in(r#"{"https://auth.x.ai::client":{"key":""}}"#).is_none());
        assert!(token_in("not json").is_none());
    }

    #[test]
    #[ignore]
    fn live_billing_endpoint_answers_for_this_account() {
        let samples = GrokBuildQuota.fetch();
        assert!(
            !samples.is_empty(),
            "no ~/.grok/auth.json key, or it expired (401)?"
        );
        for s in &samples {
            println!(
                "grokbuild quota: {:.2}% window={} label={:?} resets_at_ms={}",
                s.used_percent, s.window_minutes, s.label, s.resets_at_ms
            );
        }
    }
}

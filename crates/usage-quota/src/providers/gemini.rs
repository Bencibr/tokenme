//! Gemini CLI: deliberately **no probe**. Recorded so the next reader does not
//! re-derive it.
//!
//! ## The surface that does exist
//!
//! `POST https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota` with body
//! `{"project": "<gcp project id>"}`, `Authorization: Bearer <access_token>`,
//! answering `{"buckets":[{"modelId","remainingFraction","resetTime"}]}` —
//! `opgginc/opencode-bar:CopilotMonitor/CopilotMonitor/Providers/GeminiCLIProvider.swift:24-59`.
//! It is the only documented quota endpoint reachable with a Gemini-family OAuth
//! token, and `samples_from` below already parses it.
//!
//! ## Why it stays dark on this machine (measured 2026-09-23)
//!
//! 1. **The token on disk is expired.** `~/.gemini/oauth_creds.json` carries
//!    `expiry_date = 1788865532000` (2026-09-08) against a wall clock of
//!    1790177302996. The scope is not the problem — it is
//!    `userinfo.email openid cloud-platform userinfo.profile` — so the credential has
//!    the right shape and is simply stale. Measured live: HTTP 401
//!    `Request had invalid authentication credentials.`
//! 2. **Freshening it is out of scope for a probe.** The `refresh_token` in that file
//!    only works against Google's OAuth2 token endpoint with the client id/secret
//!    embedded in the Gemini CLI bundle, posted as
//!    `application/x-www-form-urlencoded`; `crate::http` only speaks JSON
//!    (`post_json`), and no probe here logs in, refreshes, or writes a credential
//!    back — the same rule `providers::claude` follows.
//! 3. **There is no project id on disk to send.** `~/.gemini/google_accounts.json` is
//!    `{"active":"<email>","old":[]}`, `~/.gemini/projects.json` maps a local folder
//!    to a project *label*, and `~/.gemini/config/config.json` is `userSettings` only.
//!    The documented discovery call is `v1internal:loadCodeAssist`, which needs 1.
//!
//! `~/.gemini/antigravity*/` and `~/.gemini/jetski-standalone-oauth-token` hold
//! fresher credentials, but those belong to the Antigravity CLI and to an unrelated
//! tool — not to the Gemini CLI, and not to this tool id.
//!
//! ## Wiring this up later
//!
//! `tool()` returns `"gemini"`, which is not in `usage_adapter_all::TOOL_IDS` either,
//! so that id has to be added before `providers::optional()` can list this probe.

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::QuotaProbe;

pub struct GeminiQuota;

impl QuotaProbe for GeminiQuota {
    fn tool(&self) -> &'static str {
        "gemini"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        // Expired token and no project id — see the module docs. Trying anyway would
        // only spend one 401 per refresh cycle.
        Vec::new()
    }
}

/// `remainingFraction` is the share *left* (0-1), so it inverts to used-percent.
/// Kept for whoever adds the token refresh; never called from `fetch`, hence only
/// live in the test build until that endpoint is reachable from this machine.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    let Some(buckets) = body.get("buckets").and_then(Value::as_array) else { return Vec::new() };
    let mut out = Vec::new();
    for bucket in buckets.iter().filter_map(Value::as_object) {
        let Some(remaining) = bucket.get("remainingFraction").and_then(Value::as_f64) else { continue };
        if !(0.0..=1.0).contains(&remaining) {
            continue;
        }
        // `id`/`modelId` carries the window: `gemini-5h`, `gemini-weekly`
        // (`nicezic/lean-quota-monitor:src/providers/gemini.ts:39-57`).
        let id = bucket
            .get("id")
            .or_else(|| bucket.get("modelId"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let minutes = if id.contains("week") {
            10_080
        } else if id.contains("5h") || id.contains("hour") {
            300
        } else {
            0
        };
        out.push(QuotaSample {
            used_percent: (1.0 - remaining) * 100.0,
            window_minutes: minutes,
            resets_at_ms: bucket.get("resetTime").and_then(Value::as_str).and_then(parse_ts_ms).unwrap_or(0),
            label: Some(if id.is_empty() { "Gemini".into() } else { id.to_string() }),
            id: None,
        });
    }
    out.sort_by_key(|s| s.window_minutes);
    out
}

/// The on-disk credential as `(expiry_ms, scope)` — never its token.
#[cfg(test)]
fn creds_state() -> Option<(i64, String)> {
    let path = dirs::home_dir()?.join(".gemini/oauth_creds.json");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some((body.get("expiry_date")?.as_i64()?, body.get("scope")?.as_str()?.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_answers_nothing_without_inventing_a_row() {
        assert!(GeminiQuota.fetch().is_empty(), "a fake 0 % bar is worse than no bar");
    }

    #[test]
    fn a_future_quota_surface_would_parse_straight_through() {
        let body: Value = serde_json::from_str(
            r#"{"buckets":[
                 {"id":"gemini-5h","modelId":"gemini-2.5-pro","remainingFraction":0.25,"resetTime":"2026-09-23T18:00:00Z"},
                 {"id":"gemini-weekly","remainingFraction":0.9,"resetTime":"2026-09-27T00:00:00Z"},
                 {"id":"unrelated","remainingFraction":7.5,"resetTime":"nope"},
                 {"modelId":"no-fraction"}]}"#,
        )
        .unwrap();
        let s = samples_from(&body);
        assert_eq!(s.len(), 2, "a fraction outside 0-1 is not a quota reading");
        assert_eq!((s[0].window_minutes, s[0].used_percent), (300, 75.0), "remaining inverts to used");
        assert_eq!(s[1].window_minutes, 10_080);
        assert!((s[1].used_percent - 10.0).abs() < 1e-9, "a fraction, so compare with a tolerance");
        assert_eq!(s[1].resets_at_ms, 1_790_467_200_000);
        assert!(samples_from(&Value::Null).is_empty());
        assert!(samples_from(&serde_json::from_str(r#"{"error":{"code":401}}"#).unwrap()).is_empty());
    }

    #[test]
    #[ignore]
    fn live_state_says_the_disk_credential_is_stale_not_underprivileged() {
        // Re-run this if the Gemini CLI ever writes a fresh token plus a project id.
        let Some((expiry, scope)) = creds_state() else {
            println!("gemini: no readable ~/.gemini/oauth_creds.json -> nothing to try");
            return;
        };
        let now = chrono::Utc::now().timestamp_millis();
        println!(
            "gemini: token {} (expiry {expiry} vs now {now}); cloud-platform in scope? {}",
            if expiry < now { "EXPIRED" } else { "live" },
            scope.contains("cloud-platform")
        );
        println!("gemini: fetch() is a documented no-probe; a live call also needs a cloudaicompanion project id (HTTP 401 as measured).");
    }
}

//! OpenCode Go quota: the subscription's three dollar-value windows.
//!
//! The key is the one the user already configured for the OpenCode Go provider,
//! read from `OPENCODE_API_KEY` or the tool's own `auth.json`. No key, no row.
//!
//! The request shape is `GET https://opencode.ai/zen/go/v1/usage` with
//! `Authorization: Bearer <key>` + `Accept: application/json` — identical in both
//! independent reference implementations (`slkiser/opencode-quota:src/lib/opencode-go.ts:5,153-160`,
//! `nicezic/lean-quota-monitor:docs/provider-acquisition-spec.md` "OpenCode Go").
//! A `User-Agent` is optional (the endpoint answers without it).
//!
//! ## Why this probe can answer nothing on an unsubscribed account
//!
//! Measured on this machine: the credential resolves (`auth.json` id `opencode-go`,
//! type `api`) and is a live key — `GET /zen/go/v1/models` with the same header
//! returns 200 — but `/zen/go/v1/usage` returns
//! `403 {"type":"error","error":{"type":"EntitlementError","message":"OpenCode Go
//! subscription required."}}`. That is the documented *not subscribed* answer, not a
//! wrong request: a Zen pay-as-you-go key is valid but has no Go allowance to report
//! (`slkiser/opencode-quota:src/lib/opencode-go.ts:137-146` treats exactly this body
//! as `notSubscribed`, and returns no rows). `http::get_json` drops non-2xx bodies,
//! so the 403 degrades to `Vec::new()` here the same way.

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::http::get_json;
use crate::QuotaProbe;

const ENDPOINT: &str = "https://opencode.ai/zen/go/v1/usage";
const USER_AGENT: &str = concat!("tokenme/", env!("CARGO_PKG_VERSION"));

/// The provider ids OpenCode writes for a Go key (`opencode auth login -p
/// opencode-go`), in preference order, with `opencode` kept as the legacy alias
/// (`slkiser/opencode-quota:src/lib/opencode-go-auth.ts:12`).
const AUTH_IDS: &[&str] = &["opencode-go", "opencode"];

/// `(json key, label, window)` — `rolling` is the 5-hour window, not a day:
/// `$12 rolling 5h / $30 weekly / $60 monthly`
/// (`nicezic/lean-quota-monitor:docs/provider-acquisition-spec.md:568-576`,
/// `opgginc/opencode-bar:CopilotMonitor/CopilotMonitor/Providers/OpenCodeGoProvider.swift:94-99`).
const WINDOWS: &[(&str, &str, i64)] = &[("rolling", "5 小时", 300), ("weekly", "7 天", 10_080), ("monthly", "月", 43_200)];

pub struct OpenCodeQuota;

impl QuotaProbe for OpenCodeQuota {
    fn tool(&self) -> &'static str {
        "opencode"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        live().unwrap_or_default()
    }
}

fn live() -> Option<Vec<QuotaSample>> {
    let key = api_key()?;
    let body = get_json(
        ENDPOINT,
        &[
            ("authorization", &format!("Bearer {key}")),
            ("accept", "application/json"),
            ("user-agent", USER_AGENT),
        ],
    )?;
    Some(samples_from(&body))
}

/// `{"usage":{"rolling":{"status","percent","resetsAt"}, …}}`. `percent` is
/// already used-percent (0-100), so it is never inverted. A window whose `status`
/// is `rate-limited` is exhausted whatever `percent` says
/// (`slkiser/opencode-quota:src/lib/opencode-go.ts:106-112`); the other windows in
/// the same response still count.
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    // Some deployments answer with the buckets at the top level instead.
    let usage = body.get("usage").unwrap_or(body);
    let mut out = Vec::new();
    for (bucket, label, minutes) in WINDOWS.iter().copied() {
        let Some(entry) = usage.get(bucket).and_then(Value::as_object) else { continue };
        let Some(percent) = entry.get("percent").and_then(Value::as_f64) else { continue };
        let exhausted = entry.get("status").and_then(Value::as_str) == Some("rate-limited");
        let used_percent = if exhausted { 100.0 } else { percent.clamp(0.0, 100.0) };
        out.push(QuotaSample {
            used_percent,
            window_minutes: minutes,
            resets_at_ms: entry.get("resetsAt").and_then(Value::as_str).and_then(parse_ts_ms).unwrap_or(0),
            label: Some(label.to_string()),
            id: None,
        });
    }
    out.sort_by_key(|s| s.window_minutes);
    out
}

fn api_key() -> Option<String> {
    if let Some(key) = std::env::var("OPENCODE_API_KEY").ok().filter(|k| !k.is_empty()) {
        return Some(key);
    }
    let root = std::env::var_os("OPENCODE_DATA_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share").join("opencode")))?;
    auth_key(&root.join("auth.json"))
}

/// `auth.json` maps a provider id to `{type, key}`. The Go usage endpoint only
/// accepts a Go key, so the known ids win; any other `type:"api"` entry is kept as
/// a last resort because a manually configured key may sit under another name.
pub(crate) fn auth_key(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let map = value.as_object()?;
    let is_api = |id: &str| -> Option<String> {
        let entry = map.get(id)?;
        if entry.get("type").and_then(Value::as_str) != Some("api") {
            return None;
        }
        Some(entry.get("key")?.as_str()?.trim().to_string())
    };
    for id in AUTH_IDS {
        if let Some(key) = is_api(id).filter(|k| !k.is_empty()) {
            return Some(key);
        }
    }
    let mut keys: Vec<String> = map
        .values()
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("api"))
        .filter_map(|v| v.get("key").and_then(Value::as_str))
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .collect();
    keys.sort();
    keys.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured shape of a 200 from `GET /zen/go/v1/usage` (fake numbers, real
    /// field names and timestamp dialects: `resetsAt` arrives as `Z` *and* as an
    /// offset, and extra top-level keys ride along).
    const OK_SAMPLE: &str = r#"{
      "usage": {
        "rolling":  {"status":"ok","percent":12.5,"resetsAt":"2026-08-12T12:30:00Z"},
        "weekly":   {"status":"ok","percent":45,"resetsAt":"2026-08-16T18:00:00+02:00"},
        "monthly":  {"status":"ok","percent":80,"resetsAt":"2026-09-01T00:00:00-04:00"}
      },
      "ignored": true
    }"#;

    #[test]
    fn three_windows_in_stable_order() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        let s = samples_from(&body);
        assert_eq!(
            s.iter().map(|x| (x.window_minutes, x.used_percent)).collect::<Vec<_>>(),
            vec![(300, 12.5), (10_080, 45.0), (43_200, 80.0)],
            "sorted by window, not by the order the API happened to serialise them"
        );
        assert_eq!(s[0].label.as_deref(), Some("5 小时"), "rolling is the 5-hour window, not a day");
        assert!(s[0].resets_at_ms > 1_700_000_000_000);
    }

    #[test]
    fn an_offset_reset_is_read_as_utc_not_as_local() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        let s = samples_from(&body);
        assert_eq!(s[1].resets_at_ms, chrono::DateTime::parse_from_rfc3339("2026-08-16T16:00:00Z").unwrap().timestamp_millis());
    }

    #[test]
    fn a_rate_limited_window_is_exhausted_whatever_percent_says() {
        let body: Value = serde_json::from_str(
            r#"{"usage":{"rolling":{"status":"rate-limited","percent":42,"resetsAt":"2026-08-12T12:30:00Z"},
                 "weekly":{"status":"ok","percent":7}}}"#,
        )
        .unwrap();
        let s = samples_from(&body);
        assert_eq!(s[0].used_percent, 100.0, "the vendor's stale percent loses to its status");
        assert!(s[0].resets_at_ms > 0, "the reported reset time is still kept");
        assert_eq!(s[1].used_percent, 7.0, "healthy windows in the same body stay visible");
    }

    #[test]
    fn percent_is_used_percent_and_is_clamped_not_inverted() {
        let body: Value =
            serde_json::from_str(r#"{"usage":{"monthly":{"status":"ok","percent":130}}}"#).unwrap();
        let s = samples_from(&body);
        assert_eq!(s[0].used_percent, 100.0);
        assert_eq!(s[0].resets_at_ms, 0, "a missing resetsAt is unknown, not now");
    }

    #[test]
    fn the_not_subscribed_403_envelope_yields_nothing() {
        // What this machine actually gets back (see the module docs).
        let body: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"EntitlementError","message":"OpenCode Go subscription required."}}"#,
        )
        .unwrap();
        assert!(samples_from(&body).is_empty());
    }

    #[test]
    fn partial_and_junk_bodies_are_not_errors() {
        let body: Value =
            serde_json::from_str(r#"{"usage":{"weekly":{"status":"ok","percent":3}}}"#).unwrap();
        assert_eq!(samples_from(&body).len(), 1);
        assert!(samples_from(&serde_json::from_str(r#"{"usage":{}}"#).unwrap()).is_empty());
        assert!(samples_from(&Value::Null).is_empty());
        assert!(samples_from(&serde_json::from_str(r#"{"usage":{"rolling":"nope"}}"#).unwrap()).is_empty());
    }

    #[test]
    fn the_go_credential_wins_over_an_unrelated_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"anthropic":{"type":"api","key":"sk-zzz"},"opencode-go":{"type":"api","key":"sk-go"}}"#,
        )
        .unwrap();
        assert_eq!(auth_key(&path).as_deref(), Some("sk-go"), "an alphabetically first Zen key would 403");
    }

    #[test]
    fn only_api_type_credentials_are_used_and_the_order_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"zeta":{"type":"oauth","key":"nope"},"beta":{"type":"api","key":" sk-b "},"alpha":{"type":"api","key":"sk-a"}}"#,
        )
        .unwrap();
        assert_eq!(auth_key(&path).as_deref(), Some("sk-a"), "keys are trimmed, ids sort stably");
        std::fs::write(&path, r#"{"opencode-go":{"type":"oauth","key":"sk-x"}}"#).unwrap();
        assert_eq!(auth_key(&path), None, "an oauth entry under the Go id is not a Go key");
        std::fs::write(&path, b"{").unwrap();
        assert_eq!(auth_key(&path), None, "a torn auth.json is not a crash");
    }

    #[test]
    #[ignore]
    fn live_usage_endpoint_answers_for_this_account() {
        let samples = OpenCodeQuota.fetch();
        if samples.is_empty() {
            // Measured 2026-09-23: HTTP 403 EntitlementError — this key is a Zen
            // pay-as-you-go key, so OpenCode Go has no allowance to report. The
            // request itself is proven correct by `/zen/go/v1/models` answering 200.
            println!("opencode: no Go subscription on this account (403 EntitlementError) -> no rows, by design");
            return;
        }
        for s in &samples {
            println!("opencode quota: {:.2}% window={} label={:?}", s.used_percent, s.window_minutes, s.label);
        }
    }
}

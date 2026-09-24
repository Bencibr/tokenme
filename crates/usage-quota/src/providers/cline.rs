//! Cline: the windowed limits its own account API reports.
//!
//! ## What Cline's own source says (clined/cline, read 2026-09-24)
//!
//! Cline writes no quota to disk — `~/.cline/data` holds sessions, provider
//! config and feature flags, and there is no keychain item or `state.vscdb` entry
//! for it. What it *does* keep locally is the credential its gateway login
//! produced: `providers.json` → `providers.cline.settings.auth.accessToken`, a
//! string that already carries the literal `workos:` prefix.
//!
//! The credential rules below are lifted from Cline's source, not guessed:
//!
//! * The account API's bearer keeps the `workos:` prefix. `apps/vscode/src/sdk/
//!   account-service.ts` says it outright — "IMPORTANT: Prefixed with 'workos:'
//!   so backend can route verification to WorkOS provider" — and `formatClineApiKey`
//!   (`auth/provider-auth-registry.ts`) re-adds the prefix if a stored value lost
//!   it. `auth/provider-auth-registry.ts:81-100` derives the expiry: the explicit
//!   `expiresAt` (milliseconds; `providers.json` never stores seconds) wins, then
//!   the token's own JWT `exp`, and with neither the credential counts as stale.
//! * `resolveLocalClineAuthToken` (`services/providers/local-provider-service.ts`)
//!   accepts the static `apiKey` where no OAuth token is stored, and
//!   `shared/src/storage/paths.ts` resolves the file through
//!   `CLINE_PROVIDER_SETTINGS_PATH` (a whole-path override) or `CLINE_DATA_DIR`
//!   before falling back to `~/.cline/data/settings/providers.json`. Both are
//!   mirrored here.
//! * Refreshing is deliberately not done: the client's refresh grant rotates the
//!   token and writes the new value back (`runtime-oauth-token-manager.ts`), so a
//!   probe that refreshed without persisting would log the user out of Cline.
//!   Same rule as [`super::antigravity`] and [`super::gemini`].
//!
//! ```text
//! GET https://api.cline.bot/api/v1/users/me/plan/usage-limits
//!   → {"data":{"limits":[{"type":"five_hour|weekly|monthly","percentUsed":0-100,
//!                          "resetsAt":"…"}]}}
//! ```
//!
//! `percentUsed` is already the *used* share (unlike Antigravity's
//! `remaining_fraction`), so it maps straight onto [`QuotaSample::used_percent`].
//!
//! ## The one thing the source cannot confirm
//!
//! No current Cline client calls `usage-limits` — the endpoint and its field
//! names come from a third-party measurement (`Javis603/token-monitor
//! :docs/providers/cline.md:75-84`), and Cline's clients surface ClinePass
//! limits only as error messages at request time (`ClinePassLimitError`), never
//! by polling. What the source does document is `users/me/plan` (subscription,
//! no windows) and `users/{id}/usages` (transaction lines). So this endpoint is
//! kept because it is the only windowed source there is, and the parser stays
//! tolerant of both envelope shapes (`data.limits` and bare `limits`); a shape
//! drift answers empty rather than inventing a bar.
//!
//! Only a ClinePass subscriber has windows: for everyone else the endpoint answers
//! 404 or an empty list, and this probe then answers nothing. A tool with no
//! vendor limit is exactly what [`crate::Budget`] exists for — the panel shows the
//! user's own cap instead of inventing one.
//!
//! ## The credential has a lifetime, and this probe does not extend it
//!
//! Measured on this machine (2026-09-24): the stored token had expired **19.5
//! days** earlier (the Cline CLI has not been run since), and the endpoints
//! answer `401 {"error":"Unauthorized: …"}`. So an empty answer here is the
//! login being stale, not a broken request; a machine where Cline was used in
//! the last hour answers normally — and a stale credential is refused here
//! without costing the 401.

use std::path::PathBuf;

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::http::get_json;
use crate::QuotaProbe;

pub struct ClineQuota;

const ENDPOINT: &str = "https://api.cline.bot/api/v1/users/me/plan/usage-limits";

/// Cline's own bucket names, in the order a reset arrives.
const WINDOWS: [(&str, i64, &str); 3] =
    [("five_hour", 300, "5 小时"), ("weekly", 10_080, "周"), ("monthly", 43_200, "月")];

impl QuotaProbe for ClineQuota {
    fn tool(&self) -> &'static str {
        "cline"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(token) = access_token() else { return Vec::new() };
        let Some(body) = get_json(
            ENDPOINT,
            &[
                ("authorization", &format!("Bearer {token}")),
                ("content-type", "application/json"),
                ("accept", "application/json"),
            ],
        ) else {
            return Vec::new();
        };
        samples_from(&body)
    }
}

/// The stored gateway token, verbatim — the `workos:` prefix is part of it.
///
/// Expiry is judged the way Cline's own registry derives it: the explicit
/// `expiresAt` (milliseconds; a legacy seconds-shaped value is promoted) wins,
/// else the access token's own JWT `exp`, and with neither the credential
/// counts as stale — the client treats "unknown expiry" as "refresh now", and
/// a probe that cannot refresh has no business sending a likely-dead token.
/// A stale OAuth token falls through to the static `apiKey`, the same fallback
/// `resolveLocalClineAuthToken` applies.
pub(crate) fn access_token() -> Option<String> {
    let raw = std::fs::read_to_string(settings_file()?).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let auth = value.get("providers")?.get("cline")?.get("settings")?.get("auth")?;
    let token = auth.get("accessToken").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
    if let Some(token) = token {
        let expiry = auth
            .get("expiresAt")
            .and_then(Value::as_i64)
            .map(|ms| if ms < 10_000_000_000 { ms * 1_000 } else { ms })
            .or_else(|| jwt_exp_ms(token));
        if expiry.is_none_or(|expires_ms| expires_ms >= usage_core::report::now_ms()) {
            return Some(token.to_string());
        }
    }
    let key = auth.get("apiKey").and_then(Value::as_str)?.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// The `exp` claim of an access-token JWT, in milliseconds — the SDK derives
/// the credential's expiry from it when `providers.json` has no `expiresAt`.
fn jwt_exp_ms(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = b64url_decode(payload)?;
    let claim = serde_json::from_slice::<Value>(&decoded).ok()?.get("exp")?.as_f64()?;
    (claim > 0.0).then(|| (claim * 1_000.0) as i64)
}

/// Base64url without padding, which is what a JWT segment is.
fn b64url_decode(segment: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bits = 0u32;
    let mut acc = 0u16;
    let mut out = Vec::with_capacity(segment.len() * 3 / 4);
    for c in segment.bytes() {
        let v = T.iter().position(|&x| x == c)? as u16;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The SDK's own resolution order (`shared/src/storage/paths.ts`): a whole-path
/// override, then a relocated data dir, then `~/.cline/data`.
fn settings_file() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLINE_PROVIDER_SETTINGS_PATH") {
        let p = p.trim();
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let root = match std::env::var("CLINE_DATA_DIR") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v.trim()),
        _ => dirs::home_dir()?.join(".cline").join("data"),
    };
    Some(root.join("settings/providers.json"))
}

/// Both dialects the endpoint has been seen to use: an RFC 3339 string, or a
/// unix number in seconds (promoted to ms, the same rule the adapters apply).
fn reset_ms(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::String(s)) => parse_ts_ms(s).unwrap_or(0),
        Some(Value::Number(n)) => n.as_i64().map(|v| if v < 10_000_000_000 { v * 1_000 } else { v }).unwrap_or(0),
        _ => 0,
    }
}

/// `{"data":{"limits":[…]}}`, and also a bare `{"limits":[…]}`: the endpoint has
/// answered both ways depending on the account.
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    let limits = body
        .pointer("/data/limits")
        .or_else(|| body.get("limits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<QuotaSample> = Vec::new();
    for limit in limits.iter().filter_map(Value::as_object) {
        let kind = limit.get("type").and_then(Value::as_str).unwrap_or_default();
        let Some(used) = limit.get("percentUsed").and_then(Value::as_f64) else { continue };
        if !used.is_finite() || used < 0.0 {
            continue;
        }
        let (minutes, name) = match WINDOWS.iter().find(|(k, _, _)| *k == kind) {
            Some((_, m, n)) => (*m, *n),
            // An unknown bucket is still a real window; only its label is unknown.
            None => (0, "窗口"),
        };
        out.push(QuotaSample {
            used_percent: used.min(100.0),
            window_minutes: minutes,
            resets_at_ms: reset_ms(limit.get("resetsAt").or_else(|| limit.get("resets_at"))),
            label: Some(format!("ClinePass · {name}")),
            id: None,
        });
    }
    out.sort_by_key(|s| s.window_minutes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerant on purpose: "not a JSON body at all" is one of the cases under
    /// test, and `samples_from` must answer it with nothing rather than panic.
    fn body(json: &str) -> Value {
        serde_json::from_str(json).unwrap_or(Value::Null)
    }

    #[test]
    fn the_three_vendor_windows_map_straight_through() {
        let s = samples_from(&body(
            r#"{"data":{"limits":[
                 {"type":"weekly","percentUsed":42.5,"resetsAt":"2026-09-27T00:00:00Z"},
                 {"type":"five_hour","percentUsed":8,"resetsAt":"2026-09-23T18:00:00Z"},
                 {"type":"monthly","percentUsed":100,"resetsAt":"2026-10-01T00:00:00Z"}]}}"#,
        ));
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].window_minutes, s[0].used_percent), (300, 8.0), "shortest window first");
        assert_eq!(s[0].label.as_deref(), Some("ClinePass · 5 小时"));
        assert_eq!(s[1].window_minutes, 10_080);
        assert_eq!(s[2].window_minutes, 43_200);
        assert_eq!(s[2].used_percent, 100.0);
        assert!(s[0].resets_at_ms > 1_790_000_000_000, "{:?}", s[0]);
    }

    #[test]
    fn percent_used_is_used_as_is_and_over_one_hundred_is_clamped() {
        let s = samples_from(&body(r#"{"limits":[{"type":"weekly","percentUsed":137.2}]}"#));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].used_percent, 100.0, "the vendor's own figure, capped at full");
    }

    #[test]
    fn an_unknown_bucket_or_a_bare_answer_still_produces_a_bar() {
        let s = samples_from(&body(r#"{"limits":[{"type":"daily","percentUsed":12.5,"resetsAt":"2026-09-24T00:00:00Z"}]}"#));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].window_minutes, 0, "no window length is claimed for a name we do not know");
        assert_eq!(s[0].label.as_deref(), Some("ClinePass · 窗口"));
        assert!(s[0].resets_at_ms > 0);
    }

    #[test]
    fn nothing_at_all_is_the_answer_for_an_account_without_windows() {
        for raw in [
            r#"{"data":{"limits":[]}}"#,
            r#"{"error":"not found"}"#,
            "[]",
            "not json",
        ] {
            assert!(samples_from(&body(raw)).is_empty(), "{raw} must answer nothing");
        }
        // A missing `percentUsed` is not a 0 % window.
        assert!(samples_from(&body(r#"{"limits":[{"type":"weekly"}]}"#)).is_empty());
        assert!(samples_from(&body(r#"{"limits":[{"type":"weekly","percentUsed":-3}]}"#)).is_empty());
    }

    #[test]
    fn the_credential_is_read_verbatim_from_the_gateway_profile() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings/providers.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::env::set_var("CLINE_DATA_DIR", dir.path());
        let write = |auth: serde_json::Value| {
            std::fs::write(
                &settings,
                serde_json::json!({"version":"1","providers":{"cline":{"settings":{"auth":auth}}}}).to_string(),
            )
            .unwrap();
        };
        let future = usage_core::report::now_ms() + 3_600_000;

        write(serde_json::json!({"accessToken":"  workos:eyJhbGciOi  ","expiresAt":future}));
        assert_eq!(
            access_token().as_deref(),
            Some("workos:eyJhbGciOi"),
            "the workos: prefix is part of the bearer value, and surrounding blanks are trimmed"
        );

        write(serde_json::json!({"accessToken":"   "}));
        assert_eq!(access_token(), None, "a blank token is no token");

        // The state this machine is actually in: a credential 19 days past its
        // `expiresAt`, which every endpoint answers 401 for.
        write(serde_json::json!({"accessToken":"workos:live","expiresAt":1_700_000_000_000i64}));
        assert_eq!(access_token(), None, "a stale token must cost zero 401s");

        write(serde_json::json!({"accessToken":"workos:live","expiresAt":future}));
        assert_eq!(access_token().as_deref(), Some("workos:live"), "a live token is used verbatim");

        write(serde_json::json!({"accessToken":"workos:no-expiry-field"}));
        assert_eq!(access_token().as_deref(), Some("workos:no-expiry-field"), "no expiry to judge by is not a refusal");

        std::env::remove_var("CLINE_DATA_DIR");
    }

    #[test]
    #[ignore = "calls Cline's account API with this machine's login"]
    fn the_live_account_answers_with_its_windows_or_says_it_has_none() {
        let Some(token) = access_token() else {
            println!("[cline] no gateway token in {} -> nothing to ask", settings_file().unwrap().display());
            return;
        };
        println!("[cline] token present, {} chars, prefixed {}", token.len(), token.split(':').next().unwrap_or(""));
        let body = get_json(
            ENDPOINT,
            &[("authorization", &format!("Bearer {token}")), ("content-type", "application/json"), ("accept", "application/json")],
        );
        println!("[cline] raw answer = {body:?}");
        println!("[cline] samples = {:?}", body.as_ref().map(samples_from));
    }
}

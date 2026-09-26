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
//! * Refreshing is done through the gateway the SDK itself uses
//!   (`refreshClineToken`: `POST /api/v1/auth/refresh` with the stored refresh
//!   token), and the rotated pair is persisted back into `providers.json` —
//!   the persistence is what makes this safe, because a refresh whose result
//!   is not written back consumes the rotation and logs the user out. This is
//!   the one place tokenme writes to another tool's file, and it writes only
//!   the three rotated fields, atomically, permissions preserved.
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
//! Live-verified with a fresh login (2026-09-24): the route exists but a
//! non-subscriber account gets `404 {"data":null,"error":"no plan history
//! found for user","success":false}` — the same answer `users/me/plan` gives.
//! `users/{id}/balance` (prepaid credits, `0` here) and `users/{id}/usages`
//! (per-call ledger: `creditsUsed`, `costUsd`, token counts) answer 200 but
//! carry no limit to draw a bar against: a balance has no denominator, a
//! ledger has no ceiling. So for an account without ClinePass this probe
//! answering nothing *is* the calibrated result — the vendor publishes no
//! quota for it, and [`crate::Budget`] is the honest bar. Only a ClinePass
//! subscriber's `usage-limits` returns windows.
//!
//! ## The credential has a one-hour lifetime, and the gateway extends it
//!
//! Measured on this machine (2026-09-24): a stored token expired **19.5 days**
//! earlier (the Cline CLI had not been run since) and every endpoint answered
//! `401 {"error":"Unauthorized: …"}`; a fresh login reads one hour out. An
//! hour-scale `expiresAt` is therefore the normal state: when the stored
//! token is stale or inside the SDK's five-minute refresh buffer, this probe
//! rotates it through the gateway (above) so the tier bar survives the
//! expiry; when the refresh token itself has been revoked, the answer is
//! empty and a re-login in Cline is the fix.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::http::{get_json, get_json_any_status, post_json};
use crate::QuotaProbe;

pub struct ClineQuota;

const ENDPOINT: &str = "https://api.cline.bot/api/v1/users/me/plan/usage-limits";
/// The SDK's own plan call (`fetchCurrentUserPlan` → `users/me/plan`), used
/// here for the tier name a bar is labelled with.
const PLAN_ENDPOINT: &str = "https://api.cline.bot/api/v1/users/me/plan";

/// Cline's own bucket names, in the order a reset arrives.
const WINDOWS: [(&str, i64, &str); 3] =
    [("five_hour", 300, "5 小时"), ("weekly", 10_080, "周"), ("monthly", 43_200, "月")];

impl QuotaProbe for ClineQuota {
    fn tool(&self) -> &'static str {
        "cline"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(token) = access_token() else { return Vec::new() };
        let headers = [
            ("authorization", format!("Bearer {token}")),
            ("content-type", "application/json".to_string()),
            ("accept", "application/json".to_string()),
        ];
        let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        // The tier comes first: windows are labelled with it, and an account
        // the vendor answers "no plan history" for is on the free tier, which
        // still gets a named bar instead of silently vanishing from the panel.
        let Some(tier) = plan_tier(&borrowed) else { return Vec::new() };
        let mut out = get_json(ENDPOINT, &borrowed).map(|body| samples_from(&body, &tier)).unwrap_or_default();
        if out.is_empty() {
            out.push(tier_sample(&tier));
        }
        out
    }
}

/// The account's tier, as the plan endpoint names it.
///
/// `404 {"error":"no plan history found for user"}` is the vendor's own answer
/// for an unsubscribed account — measured live — and reads as the vendor's own
/// free tier (its source names it too, in `ClineFreeModelLimitError`). `None`
/// means the request failed rather than "free": a network error must not
/// downgrade a paying account's label.
fn plan_tier(headers: &[(&str, &str)]) -> Option<String> {
    let (status, body) = get_json_any_status(PLAN_ENDPOINT, headers)?;
    tier_from_answer(status, &body)
}

/// The tier an answer states. A 2xx whose `plan` is absent or null is the
/// SDK's `UserCurrentPlan.plan: null` shape and says "no subscription" just as
/// the 404 does; any other non-2xx is a failure, not a tier.
fn tier_from_answer(status: u16, body: &Value) -> Option<String> {
    const FREE: &str = "Free";
    if !(200u16..300).contains(&status) {
        let no_plan = body.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("no plan history"));
        return no_plan.then(|| FREE.to_string());
    }
    let plan = body.pointer("/data/plan");
    let name = plan
        .and_then(|p| {
            p.get("displayName")
                .and_then(Value::as_str)
                .or_else(|| p.get("name").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|n| !n.is_empty());
    Some(name.unwrap_or(FREE).to_string())
}

/// The bar an account with no polled windows gets: the tier, and the fact that
/// the vendor publishes no queryable limit for it — the free tier does enforce
/// limits (`ClineFreeModelLimitError`), but only ever reports them at
/// request time. A bare 0 % would read as untouched allowance; the label does
/// not.
fn tier_sample(tier: &str) -> QuotaSample {
    QuotaSample {
        used_percent: 0.0,
        window_minutes: 0,
        resets_at_ms: 0,
        label: Some(format!("{tier} · 无可查限额")),
        id: Some("plan".into()),
    }
}

/// The stored gateway token, verbatim — the `workos:` prefix is part of it.
///
/// Expiry is judged the way Cline's own registry derives it: the explicit
/// `expiresAt` (milliseconds; a legacy seconds-shaped value is promoted) wins,
/// else the access token's own JWT `exp`, and with neither the credential
/// counts as stale. A token that is stale — or inside the SDK's five-minute
/// refresh buffer — is refreshed through Cline's own gateway, and the rotated
/// pair is persisted back to `providers.json` the same way the app persists
/// it; a static `apiKey` remains the fallback for profiles without OAuth.
pub(crate) fn access_token() -> Option<String> {
    let path = settings_file()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let auth = value.get("providers")?.get("cline")?.get("settings")?.get("auth")?;
    let token = auth.get("accessToken").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
    if let Some(token) = token {
        let expiry = auth
            .get("expiresAt")
            .and_then(Value::as_i64)
            .map(|ms| if ms < 10_000_000_000 { ms * 1_000 } else { ms })
            .or_else(|| jwt_exp_ms(token));
        // No expiry to judge by counts as stale: the client derives one or
        // refreshes, and this probe refuses to spend a likely-dead 401.
        if expiry.is_some_and(|expires_ms| expires_ms >= usage_core::report::now_ms() + REFRESH_BUFFER_MS) {
            return Some(token.to_string());
        }
        // Stale or about to expire: refresh through the gateway, the same call
        // the app itself makes every hour, persisting the rotated tokens.
        if let Some(refresh) =
            auth.get("refreshToken").and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty())
        {
            if let Some(fresh) = refresh_stored_tokens(&path, refresh) {
                return Some(fresh);
            }
        }
    }
    let key = auth.get("apiKey").and_then(Value::as_str)?.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// Refresh before expiry by this much — the same buffer the SDK refreshes in.
/// Refreshing only after expiry would blank the bar for one round-trip.
const REFRESH_BUFFER_MS: i64 = 5 * 60 * 1000;

const REFRESH_ENDPOINT: &str = "https://api.cline.bot/api/v1/auth/refresh";

/// The gateway URL the rotation POSTs to. Overridable for a self-hosted or
/// proxied gateway (the SDK overrides the same base), and for tests.
fn refresh_endpoint() -> String {
    std::env::var("CLINE_AUTH_REFRESH_URL")
        .ok()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| REFRESH_ENDPOINT.to_string())
}

/// One OAuth rotation: exchange the stored refresh token at Cline's own
/// gateway (`refreshClineToken` in its SDK), then write the rotated pair back
/// into `providers.json`.
///
/// The write-back is the safety, not a courtesy: refresh tokens rotate, so a
/// refresh whose result is not persisted consumes the stored token and logs
/// the user out of Cline on its next refresh. What is written mirrors the
/// app's own `saveOAuthCredentials` — new access token (with the `workos:`
/// prefix the account APIs require), the rotated refresh token when the
/// answer carries one, the parsed `expiresAt` — and nothing else. A transient
/// failure leaves the stored credentials untouched; a rejected refresh token
/// also writes nothing and simply ends the probe (re-auth is the user's next
/// step, in Cline).
fn refresh_stored_tokens(path: &Path, refresh_token: &str) -> Option<String> {
    let body = post_json(
        &refresh_endpoint(),
        &[("content-type", "application/json"), ("accept", "application/json")],
        serde_json::json!({"refreshToken": refresh_token, "grantType": "refresh_token"}),
    )?;
    let (access, rotated, expires_ms) = parse_refresh_response(&body)?;
    let mut root: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let formatted = apply_refreshed_credentials(&mut root, &access, rotated.as_deref(), expires_ms)?;
    // Atomic tmp+rename, and the credential file keeps its 0600 permissions —
    // a renamed-in temp file would otherwise land world-readable.
    let mut text = serde_json::to_vec(&root).ok()?;
    text.push(b'\n');
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &text).ok()?;
    #[cfg(unix)]
    if let Ok(original) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, original.permissions());
    }
    fs::rename(&tmp, path).ok()?;
    Some(formatted)
}

/// `{"success":true,"data":{"accessToken":…,"refreshToken"?…,"expiresAt":…}}`
/// — the shape `requireClineTokenResponse` accepts.
fn parse_refresh_response(body: &Value) -> Option<(String, Option<String>, i64)> {
    if body.get("success").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let data = body.get("data")?;
    let access = data.get("accessToken").and_then(Value::as_str)?.trim().to_string();
    if access.is_empty() {
        return None;
    }
    let rotated = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    // `expiresAt` arrives as an RFC 3339 string and is stored as milliseconds.
    let expires_ms = data.get("expiresAt").and_then(Value::as_str).and_then(parse_ts_ms)?;
    Some((access, rotated, expires_ms))
}

/// Merge one rotation into a parsed providers.json: the access token with its
/// `workos:` prefix restored (`formatClineApiKey`), the rotated refresh token
/// when present, the new expiry. Everything else — other providers, account
/// id, metadata — is left exactly as it was.
fn apply_refreshed_credentials(
    root: &mut Value,
    access: &str,
    rotated: Option<&str>,
    expires_ms: i64,
) -> Option<String> {
    let auth = root
        .get_mut("providers")?
        .get_mut("cline")?
        .get_mut("settings")?
        .get_mut("auth")?
        .as_object_mut()?;
    let formatted = if access.to_lowercase().starts_with("workos:") {
        access.to_string()
    } else {
        format!("workos:{access}")
    };
    auth.insert("accessToken".into(), Value::String(formatted.clone()));
    if let Some(r) = rotated {
        auth.insert("refreshToken".into(), Value::String(r.to_string()));
    }
    auth.insert("expiresAt".into(), Value::from(expires_ms));
    Some(formatted)
}

/// The `exp` claim of an access-token JWT, in milliseconds — the SDK derives
/// the credential's expiry from it when `providers.json` has no `expiresAt`.
fn jwt_exp_ms(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = b64url_decode(payload)?;
    let claim = serde_json::from_slice::<Value>(&decoded).ok()?.get("exp")?.as_f64()?;
    (claim > 0.0).then_some((claim * 1_000.0) as i64)
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
pub(crate) fn samples_from(body: &Value, tier: &str) -> Vec<QuotaSample> {
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
            label: Some(format!("{tier} · {name}")),
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
        let s = samples_from(
            &body(
                r#"{"data":{"limits":[
                     {"type":"weekly","percentUsed":42.5,"resetsAt":"2026-09-27T00:00:00Z"},
                     {"type":"five_hour","percentUsed":8,"resetsAt":"2026-09-23T18:00:00Z"},
                     {"type":"monthly","percentUsed":100,"resetsAt":"2026-10-01T00:00:00Z"}]}}"#,
            ),
            "ClinePass Pro",
        );
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].window_minutes, s[0].used_percent), (300, 8.0), "shortest window first");
        assert_eq!(s[0].label.as_deref(), Some("ClinePass Pro · 5 小时"), "windows carry the tier name");
        assert_eq!(s[1].window_minutes, 10_080);
        assert_eq!(s[2].window_minutes, 43_200);
        assert_eq!(s[2].used_percent, 100.0);
        assert!(s[0].resets_at_ms > 1_790_000_000_000, "{:?}", s[0]);
    }

    #[test]
    fn percent_used_is_used_as_is_and_over_one_hundred_is_clamped() {
        let s = samples_from(&body(r#"{"limits":[{"type":"weekly","percentUsed":137.2}]}"#), "ClinePass");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].used_percent, 100.0, "the vendor's own figure, capped at full");
    }

    #[test]
    fn an_unknown_bucket_or_a_bare_answer_still_produces_a_bar() {
        let s = samples_from(
            &body(r#"{"limits":[{"type":"daily","percentUsed":12.5,"resetsAt":"2026-09-24T00:00:00Z"}]}"#),
            "ClinePass",
        );
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
            // The live non-subscriber answer, measured 2026-09-24 on both plan
            // endpoints; as a body it yields no windows (the probe answers it
            // with the tier bar instead, which needs no parsing).
            r#"{"data":null,"error":"no plan history found for user","success":false}"#,
            "[]",
            "not json",
        ] {
            assert!(
                samples_from(&body(raw), "Free").is_empty(),
                "{raw} must answer nothing"
            );
        }
        // A missing `percentUsed` is not a 0 % window.
        assert!(samples_from(&body(r#"{"limits":[{"type":"weekly"}]}"#), "Free").is_empty());
        assert!(samples_from(&body(r#"{"limits":[{"type":"weekly","percentUsed":-3}]}"#), "Free").is_empty());
    }

    // ------------------------------------------------------------- the tier

    #[test]
    fn a_no_plan_history_answer_is_the_free_tier_and_other_errors_are_not_answers() {
        // Measured live: the vendor's own "unsubscribed" answer.
        let free = body(r#"{"data":null,"error":"no plan history found for user","success":false}"#);
        assert_eq!(tier_from_answer(404, &free), Some("Free".to_string()));
        assert_eq!(tier_from_answer(200, &free), Some("Free".to_string()), "the body decides, not the status");

        // The SDK's `UserCurrentPlan.plan` is nullable: a 2xx without a plan
        // object is the same "no subscription" fact.
        let null_plan = body(r#"{"data":{"plan":null},"success":true}"#);
        assert_eq!(tier_from_answer(200, &null_plan).as_deref(), Some("Free"));

        // A subscriber's plan, shaped like the SDK's UserCurrentPlan type.
        let pro = body(
            r#"{"data":{"plan":{"id":"clpass","name":"ClinePass Pro","interval":"month"},"currentPeriodEnd":"2026-10-24T00:00:00Z"},"success":true}"#,
        );
        assert_eq!(tier_from_answer(200, &pro).as_deref(), Some("ClinePass Pro"));
        let named = body(r#"{"data":{"plan":{"displayName":"Team"}},"success":true}"#);
        assert_eq!(tier_from_answer(200, &named).as_deref(), Some("Team"));

        // Anything else non-2xx is a failure, not a tier: a network-level or
        // auth error must not relabel a paying account as Free.
        let unauthorized = body(r#"{"error":"Unauthorized: Please make sure you're using the latest version of Cline"}"#);
        assert_eq!(tier_from_answer(401, &unauthorized), None);
        assert_eq!(tier_from_answer(500, &body(r#"{"error":"boom"}"#)), None);
    }

    #[test]
    fn the_no_window_tier_bar_says_what_it_is() {
        let s = tier_sample("Free");
        assert_eq!(s.label.as_deref(), Some("Free · 无可查限额"));
        assert_eq!(s.id.as_deref(), Some("plan"), "one stable row a saved ordering can pin");
        assert_eq!((s.window_minutes, s.resets_at_ms), (0, 0), "no window is claimed");
    }

    #[test]
    fn the_credential_is_read_verbatim_from_the_gateway_profile() {
        let _guard = env_lock();
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

        // The state this machine sat in for weeks: a credential 19 days past
        // its `expiresAt`, which every endpoint answers 401 for.
        write(serde_json::json!({"accessToken":"workos:live","expiresAt":1_700_000_000_000i64}));
        assert_eq!(access_token(), None, "a stale token must cost zero 401s");

        write(serde_json::json!({"accessToken":"workos:live","expiresAt":future}));
        assert_eq!(access_token().as_deref(), Some("workos:live"), "a live token is used verbatim");

        // Legacy profiles stored seconds; the SDK's migration promotes them.
        write(serde_json::json!({"accessToken":"workos:live","expiresAt":1_700_000_000i64}));
        assert_eq!(access_token(), None, "a seconds-shaped expiry is still recognised as past");

        // With no explicit `expiresAt`, the SDK derives the expiry from the
        // token's own JWT `exp` claim — and with neither, the credential
        // counts as stale (the client would refresh; we refuse instead).
        write(serde_json::json!({"accessToken": jwt("workos:", 1_700_000)}));
        assert_eq!(access_token(), None, "a JWT whose exp has passed is stale");
        write(serde_json::json!({"accessToken": jwt("workos:", (usage_core::report::now_ms() + 3_600_000) / 1000)}));
        assert_eq!(access_token().is_some(), true, "a JWT with a live exp needs no expiresAt");
        write(serde_json::json!({"accessToken":"workos:not-a-jwt"}));
        assert_eq!(access_token(), None, "no expiry to judge by is treated as stale, like the client does");

        // `resolveLocalClineAuthToken` falls back to the static key.
        write(serde_json::json!({"accessToken":"workos:stale","expiresAt":1_700_000_000_000i64,"apiKey":"sk-key"}));
        assert_eq!(access_token().as_deref(), Some("sk-key"), "a stale OAuth token yields to the static key");
        write(serde_json::json!({"apiKey":"sk-key"}));
        assert_eq!(access_token().as_deref(), Some("sk-key"), "no OAuth token is not no credential");

        // Same test, not a parallel one: these variables are process-global,
        // and a second test touching them would race this one's reads.
        let file = dir.path().join("custom.json");
        std::fs::write(&file, "{}").unwrap();
        std::env::set_var("CLINE_PROVIDER_SETTINGS_PATH", &file);
        std::env::set_var("CLINE_DATA_DIR", dir.path().join("elsewhere"));
        assert_eq!(
            settings_file().as_deref(),
            Some(file.as_path()),
            "the whole-path override wins over the data-dir override"
        );
        std::env::remove_var("CLINE_PROVIDER_SETTINGS_PATH");
        assert_eq!(
            settings_file().as_deref(),
            Some(dir.path().join("elsewhere").join("settings/providers.json").as_path()),
            "then the data-dir override applies"
        );

        std::env::remove_var("CLINE_DATA_DIR");
    }

    /// A minimal three-segment JWT with an `exp` in unix seconds.
    fn jwt(prefix: &str, exp_seconds: i64) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let encode = |bytes: &[u8]| -> String {
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                let take = chunk.len() + 1;
                for i in 0..take {
                    out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
                }
            }
            out
        };
        let payload = encode(format!(r#"{{"exp":{exp_seconds}}}"#).as_bytes());
        format!("{prefix}h.{payload}.s")
    }

    // --------------------------------------------------------- the rotation

    #[test]
    fn a_refresh_answer_yields_the_rotated_pair() {
        let ok = body(
            r#"{"success":true,"data":{"accessToken":"plain-jwt","refreshToken":"rotated","tokenType":"Bearer",
                 "expiresAt":"2026-09-24T12:00:00Z","userInfo":{"clineUserId":"usr-1"}}}"#,
        );
        let (access, rotated, expires) = parse_refresh_response(&ok).expect("valid answer parses");
        assert_eq!(access, "plain-jwt");
        assert_eq!(rotated.as_deref(), Some("rotated"), "the rotation must be persisted, or the next refresh dies");
        assert_eq!(expires, parse_ts_ms("2026-09-24T12:00:00Z").unwrap());

        // A rejected refresh token is not a credential.
        assert!(parse_refresh_response(&body(r#"{"success":false,"error":"invalid_grant"}"#)).is_none());
        // Nor a success shape without a token in it.
        assert!(parse_refresh_response(&body(r#"{"success":true,"data":{}}"#)).is_none());
        assert!(parse_refresh_response(&body("not json")).is_none());
    }

    #[test]
    fn writing_a_rotation_touches_only_the_rotated_fields() {
        let mut root = serde_json::from_str::<Value>(
            r#"{"version":"1","tray":"tokens","providers":{"cline":{"settings":{"auth":{
                 "accessToken":"workos:old","refreshToken":"old-refresh","expiresAt":1,
                 "accountId":"acc","metadata":{"provider":"cline"}},
                 "other":"kept"}},"otherProvider":{"settings":{}}}}"#,
        )
        .unwrap();
        let formatted = apply_refreshed_credentials(&mut root, "plain-jwt", Some("rotated"), 1234567890123)
            .expect("a cline auth block");
        assert_eq!(formatted, "workos:plain-jwt", "the prefix the account APIs require is restored");
        let auth = &root["providers"]["cline"]["settings"]["auth"];
        assert_eq!(auth["accessToken"], "workos:plain-jwt");
        assert_eq!(auth["refreshToken"], "rotated");
        assert_eq!(auth["expiresAt"], 1234567890123i64);
        // Everything else survives untouched.
        assert_eq!(auth["accountId"], "acc");
        assert_eq!(auth["metadata"]["provider"], "cline");
        assert_eq!(root["providers"]["cline"]["settings"]["other"], "kept");
        assert_eq!(root["providers"]["otherProvider"], serde_json::json!({"settings":{}}));
        assert_eq!(root["version"], "1");
        assert_eq!(root["tray"], "tokens");

        // An answer without a rotation keeps the stored refresh token.
        apply_refreshed_credentials(&mut root, "next", None, 1).unwrap();
        assert_eq!(root["providers"]["cline"]["settings"]["auth"]["refreshToken"], "rotated");
    }

    /// The credential file is process-global through its env overrides, so the
    /// tests that relocate it take this lock; parallel tests would otherwise
    /// read each other's fixtures.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_stale_credential_is_refreshed_and_the_rotation_lands_on_disk() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings/providers.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::env::set_var("CLINE_DATA_DIR", dir.path());
        std::fs::write(
            &settings,
            serde_json::json!({"version":"1","providers":{"cline":{"settings":{"auth":{
                "accessToken":"workos:stale","expiresAt":1_700_000_000_000i64,"refreshToken":"stored-refresh"
            }}}}})
            .to_string(),
        )
        .unwrap();

        // A local gateway speaking the measured answer shape, byte for byte.
        let answer = serde_json::json!({
            "success": true,
            "data": {"accessToken": "plain-new", "refreshToken": "rotated-refresh",
                     "tokenType": "Bearer", "expiresAt": "2026-12-24T00:00:00Z",
                     "userInfo": {"clineUserId": "usr-1"}}
        })
        .to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::env::set_var("CLINE_AUTH_REFRESH_URL", format!("http://{addr}/api/v1/auth/refresh"));
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = stream;
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            use std::io::Write as _;
            let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", answer.len(), answer);
            stream.write_all(response.as_bytes()).unwrap();
        });

        let token = access_token().expect("the rotation refreshes a stale credential");
        assert_eq!(token, "workos:plain-new", "the prefix is restored before use");
        server.join().unwrap();

        // The rotation landed on disk: new tokens, new expiry, nothing else moved.
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let auth = &after["providers"]["cline"]["settings"]["auth"];
        assert_eq!(auth["accessToken"], "workos:plain-new");
        assert_eq!(auth["refreshToken"], "rotated-refresh");
        assert_eq!(auth["expiresAt"], parse_ts_ms("2026-12-24T00:00:00Z").unwrap());
        assert_eq!(after["version"], "1");
        // And the next read serves the fresh token without another rotation.
        assert_eq!(access_token().as_deref(), Some("workos:plain-new"));

        std::env::remove_var("CLINE_AUTH_REFRESH_URL");
        std::env::remove_var("CLINE_DATA_DIR");
    }

    #[test]
    fn a_failed_refresh_leaves_the_stored_credentials_untouched() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings/providers.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::env::set_var("CLINE_DATA_DIR", dir.path());
        std::fs::write(
            &settings,
            serde_json::json!({"version":"1","providers":{"cline":{"settings":{"auth":{
                "accessToken":"workos:stale","expiresAt":1_700_000_000_000i64,"refreshToken":"stored-refresh"
            }}}}})
            .to_string(),
        )
        .unwrap();

        // A gateway that answers like the vendor does for a rejected refresh.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::env::set_var("CLINE_AUTH_REFRESH_URL", format!("http://{addr}/api/v1/auth/refresh"));
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = stream;
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            use std::io::Write as _;
            let body = r#"{"error":"invalid_grant","message":"refresh token invalid"}"#;
            let response = format!("HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", body.len(), body);
            stream.write_all(response.as_bytes()).unwrap();
        });

        assert_eq!(access_token(), None, "a rejected refresh token means re-auth");
        server.join().unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let auth = &after["providers"]["cline"]["settings"]["auth"];
        assert_eq!(auth["refreshToken"], "stored-refresh", "a failed refresh must not touch the file");
        assert_eq!(auth["accessToken"], "workos:stale");
        assert_eq!(after["version"], "1");

        std::env::remove_var("CLINE_AUTH_REFRESH_URL");
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
        let headers = [
            ("authorization", format!("Bearer {token}")),
            ("content-type", "application/json".to_string()),
            ("accept", "application/json".to_string()),
        ];
        let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let Some(tier) = plan_tier(&borrowed) else {
            println!("[cline] tier = none (request failed) -> nothing to show");
            return;
        };
        println!("[cline] tier = {tier}");
        let samples = get_json(ENDPOINT, &borrowed)
            .map(|body| samples_from(&body, &tier))
            .unwrap_or_default();
        if samples.is_empty() {
            println!("[cline] no windows -> {}", tier_sample(&tier).label.unwrap_or_default());
        }
        for s in samples {
            println!("[cline] {:>5} min  {:>6.2}%  {}", s.window_minutes, s.used_percent, s.label.unwrap_or_default());
        }
    }
}

//! Kimi Code quota: the membership windows the plan is sold on.
//!
//! ## The interface, from the vendor's own client
//!
//! `packages/oauth/src/managed-usage.ts` in `MoonshotAI/kimi-code` defines it:
//! `GET {base}/usages` with the managed OAuth bearer, base being
//! `KIMI_CODE_BASE_URL` → `https://api.kimi.com/coding/v1` (the vendor's own
//! `kimiCodeBaseUrl()` ignores region and uses exactly this default; the global
//! `https://api.kimi.ai/coding/v1` is tried second, so an account on the other
//! side still answers instead of reporting nothing).
//!
//! The token is the file the CLI writes: `~/.kimi-code/credentials/<name>.json`,
//! mode 0600, snake_case wire (`packages/oauth/src/storage.ts`), field
//! `access_token`. That token lives 15 minutes, so a bar that never renewed would
//! be blank most of the day: when `/usages` declines the current one, the probe
//! exchanges the file's `refresh_token` through the vendor's own contract and
//! writes the rotated pair back atomically — see [`refresh_credentials`]. The
//! refresh is a last resort, never a first move, and a failed exchange leaves the
//! file untouched.
//!
//! ## When no CLI credential exists: the desktop client's own key
//!
//! `Kimi.app` runs the same product behind its daimon runtime and keeps an API
//! key at `<userData>/kimi-desktop/daimon-share/daimon/config.json` →
//! `credentials.kimiCode.{apiKey,baseUrl}`. That key answers `/usages` with a
//! single summary window (`totalQuota`, scope `FEATURE_WORK`) rather than the
//! per-tier buckets above — see [`desktop_samples_from`]. The key is only read,
//! never refreshed, and this path is consulted only when the CLI's own credential
//! yields nothing; a fixture root (`KIMI_CODE_HOME`/`KIMI_DATA_DIR`) pins the
//! probe and skips it, so tests never reach into a real desktop install.
//!
//! ## Window names follow the product, not a guess
//!
//! `apps/kimi-code/src/utils/usage/usage-format.ts:103-105` renders the `/usage`
//! report as `5h limit`, `Weekly limit`, `Monthly limit` — one row per window the
//! backend served, in that order, and it *omits* a window the payload left out.
//! `limit_month_code` is not a fourth row: the same file folds it into the monthly
//! one as the kimi-vs-code split (`monthlyBreakdown`), so drawing it as its own
//! window would invent a limit the product does not show.
//!
//! `boosterWallet` (extra usage) is a wallet balance with a monthly charge limit,
//! not a token window; the panel has no row shape for a currency balance, so it is
//! deliberately not reported here.

use std::path::{Path, PathBuf};

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::http::get_json;
use crate::QuotaProbe;

/// The vendor default and its global sibling, in the order the probe tries them.
const BASES: &[&str] = &["https://api.kimi.com/coding/v1", "https://api.kimi.ai/coding/v1"];
const USAGE_PATH: &str = "/usages";
const USER_AGENT: &str = concat!("tokenme/", env!("CARGO_PKG_VERSION"));

/// `(payload key, the label the product prints, window minutes)`.
const WINDOWS: &[(&str, &str, i64)] = &[
    ("limit_5h", "5h limit", 300),
    ("limit_7d", "Weekly limit", 10_080),
    ("limit_month_total", "Monthly limit", 43_200),
];

pub struct KimiCodeQuota;

impl QuotaProbe for KimiCodeQuota {
    fn tool(&self) -> &'static str {
        "kimicode"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        live().unwrap_or_default()
    }
}

fn live() -> Option<Vec<QuotaSample>> {
    via_cli().or_else(via_desktop)
}

/// The signed-in CLI's own windows: the finer answer, so it wins whenever it has
/// one. An expired access token (the vendor's 15-minute lifetime) is refreshed
/// once — the vendor's own refresh contract rotates the refresh_token and the
/// rotation is persisted before the usages retry — and a dead login answers
/// nothing.
fn via_cli() -> Option<Vec<QuotaSample>> {
    let token = access_token()?;
    if let Some(samples) = usages_with(&token) {
        return Some(samples);
    }
    let fresh = refresh_credentials()?;
    usages_with(&fresh)
}

/// `/usages` across the base ladder. `None` means every base declined.
fn usages_with(token: &str) -> Option<Vec<QuotaSample>> {
    let headers = [
        ("authorization", format!("Bearer {token}")),
        ("accept", "application/json".to_string()),
        ("user-agent", USER_AGENT.to_string()),
    ];
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    for base in bases() {
        if let Some(body) = get_json(&format!("{base}{USAGE_PATH}"), &headers) {
            let samples = samples_from(&body);
            if !samples.is_empty() {
                return Some(samples);
            }
        }
    }
    None
}

/// The vendor's own refresh contract (`packages/oauth/src/oauth.ts:239`):
/// `POST {authHost}/api/oauth/token`, form-encoded
/// `{client_id, grant_type: "refresh_token", refresh_token}` — and the response
/// **always carries a rotated `refresh_token`** the client must persist
/// (`tokenFromResponse` rejects a response without one). The rotation is
/// written back to the credential file atomically before the usages retry, so
/// the CLI's next read picks up the fresh pair; a 401/403/invalid_grant means
/// the login itself is dead — touch nothing.
const OAUTH_HOST: &str = "https://auth.kimi.com";
const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

fn refresh_credentials() -> Option<String> {
    let path = newest_credential_path()?;
    let mut file = read_json(&path)?;
    if file.as_object().is_none() {
        return None;
    }
    let refresh_token = file.get("refresh_token").and_then(Value::as_str)?.trim().to_string();
    if refresh_token.is_empty() {
        return None;
    }
    let form = [
        ("client_id", CLIENT_ID),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
    ];
    let (_status, body) = match crate::http::post_form_any_status_read(
        &format!("{OAUTH_HOST}/api/oauth/token"),
        &[("accept", "application/json")],
        &form,
        std::time::Duration::from_secs(20),
    ) {
        Some(pair) => pair,
        None => return None,
    };
    if body.get("access_token").and_then(Value::as_str).is_none() {
        return None;
    }
    let access = body.get("access_token").and_then(Value::as_str)?.trim().to_string();
    let rotated = body.get("refresh_token").and_then(Value::as_str)?.trim().to_string();
    if access.is_empty() || rotated.is_empty() {
        return None;
    }
    let expires_in = body.get("expires_in").and_then(Value::as_i64).unwrap_or(900);
    file["access_token"] = Value::String(access.clone());
    file["refresh_token"] = Value::String(rotated);
    file["expires_in"] = Value::Number(expires_in.into());
    file["expires_at"] = Value::Number(
        (chrono::Utc::now().timestamp() + expires_in).into(),
    );
    // Atomic write: a crash mid-write must not leave the CLI without a file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&file).ok()?).ok()?;
    usage_core::replace_file(&tmp, &path).ok()?;
    Some(access)
}

/// The env override pins one base; otherwise CN then global, which is what makes
/// an account on either side answer without being told which one it is.
fn bases() -> Vec<String> {
    match std::env::var("KIMI_CODE_BASE_URL") {
        Ok(v) if !v.trim().is_empty() => vec![v.trim().trim_end_matches('/').to_string()],
        _ => BASES.iter().map(|b| b.to_string()).collect(),
    }
}

/// The newest-expiry credential file, skipping empties. A token already past
/// `expires_at` is still offered: the server's 401 is the authority on that,
/// and guessing from a local clock would hide a working session.
fn newest_credential_path() -> Option<PathBuf> {
    let dir = credentials_dir()?;
    let mut found: Vec<(i64, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else { return None };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        found.push((expiry_at(&path), path));
    }
    found.sort_by_key(|(expires, _)| *expires);
    found.pop().map(|(_, path)| path)
}

/// The stored access token, newest expiry first, skipping an empty one.
fn access_token() -> Option<String> {
    token_at(&newest_credential_path()?)
}

fn token_at(path: &Path) -> Option<String> {
    let value = read_json(path)?;
    let token = value.get("access_token").and_then(Value::as_str)?.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn expiry_at(path: &Path) -> i64 {
    read_json(path)
        .and_then(|v| v.get("expires_at").and_then(Value::as_i64))
        .unwrap_or(0)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// `<home>/credentials`, on the same root ladder the usage adapter walks.
fn credentials_dir() -> Option<PathBuf> {
    for name in ["KIMI_CODE_HOME", "KIMI_DATA_DIR"] {
        if let Ok(value) = std::env::var(name) {
            let first = value.split(',').map(str::trim).find(|s| !s.is_empty())?;
            let dir = PathBuf::from(first).join("credentials");
            return dir.is_dir().then_some(dir);
        }
    }
    let home = dirs::home_dir()?;
    [".kimi-code", ".kimi"]
        .into_iter()
        .map(|seg| home.join(seg).join("credentials"))
        .find(|p| p.is_dir())
}

/// `{"usages":{"limit_5h":{"used_ratio":0.31,"reset_time":"…"}, …}}`.
/// `used_ratio` is the *used* share (0-1) — the product renders it as usage, so it
/// is scaled, never inverted. A window the payload omits produces no row, which is
/// what an account without that tier looks like.
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    // Some deployments answer with the buckets at the top level instead.
    let usages = body.get("usages").unwrap_or(body);
    let mut out = Vec::new();
    for (key, label, minutes) in WINDOWS.iter().copied() {
        let Some(entry) = usages.get(key) else { continue };
        let Some(ratio) = entry.get("used_ratio").and_then(|v| as_ratio(v)) else { continue };
        out.push(QuotaSample {
            used_percent: (ratio * 100.0).clamp(0.0, 100.0),
            window_minutes: minutes,
            resets_at_ms: entry
                .get("reset_time")
                .and_then(Value::as_str)
                .and_then(parse_ts_ms)
                .unwrap_or(0),
            label: Some(label.to_string()),
            id: Some(key.to_string()),
        });
    }
    out.sort_by_key(|s| s.window_minutes);
    out
}

/// A ratio written as a number, or as a numeric string by a proxy that
/// stringified it. Anything else is not a window reading.
fn as_ratio(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|r| r.is_finite() && (0.0..=1.0).contains(r))
}

/// The desktop client's daimon config, `<userData>/kimi-desktop/daimon-share/
/// daimon/config.json`. `None` when a fixture root pins the probe — a test that
/// pins `KIMI_CODE_HOME`/`KIMI_DATA_DIR` must never reach into a real desktop
/// install — or when no config file exists. `KIMI_DESKTOP_CONFIG` points at one
/// directly (tests, relocated installs).
fn desktop_config_path() -> Option<PathBuf> {
    if std::env::var("KIMI_CODE_HOME").is_ok() || std::env::var("KIMI_DATA_DIR").is_ok() {
        return None;
    }
    if let Ok(path) = std::env::var("KIMI_DESKTOP_CONFIG") {
        let path = PathBuf::from(path.trim());
        return path.is_file().then_some(path);
    }
    let mut path = dirs::data_dir()?;
    for seg in ["kimi-desktop", "daimon-share", "daimon", "config.json"] {
        path.push(seg);
    }
    path.is_file().then_some(path)
}

/// The desktop runtime's summary answer, read off the key `Kimi.app` provisions
/// for it. Read-only: the key is never refreshed or written back.
fn via_desktop() -> Option<Vec<QuotaSample>> {
    let config = read_json(&desktop_config_path()?)?;
    let key = config.get("credentials")?.get("kimiCode")?;
    let token = key.get("apiKey").and_then(Value::as_str)?.trim();
    if token.is_empty() {
        return None;
    }
    let base = key
        .get("baseUrl")
        .and_then(Value::as_str)
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        .unwrap_or(BASES[0]);
    let headers = [
        ("authorization", format!("Bearer {token}")),
        ("accept", "application/json".to_string()),
        ("user-agent", USER_AGENT.to_string()),
    ];
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let body = get_json(&format!("{base}{USAGE_PATH}"), &headers)?;
    let samples = desktop_samples_from(&body);
    (!samples.is_empty()).then_some(samples)
}

/// `{"totalQuota":{"limit":"100","used":"13","remaining":"87","resetTime":"…"}}`.
/// The desktop runtime is sold on one summary window and the payload does not
/// name its length; the vendor's `parseManagedUsagePayload` defaults a nameless
/// summary to a week, and the membership it belongs to refreshes every 7 days —
/// so the row is a weekly window. `used` is derived from `remaining` when only
/// the remainder is stated; a limit of zero or absent is not a window.
pub(crate) fn desktop_samples_from(body: &Value) -> Vec<QuotaSample> {
    let Some(quota) = body.get("totalQuota").or_else(|| body.get("usage")) else { return Vec::new() };
    let Some(limit) = number(quota.get("limit")).filter(|l| *l > 0.0) else { return Vec::new() };
    let used = number(quota.get("used"))
        .or_else(|| number(quota.get("remaining")).map(|r| (limit - r).max(0.0)));
    let Some(used) = used else { return Vec::new() };
    vec![QuotaSample {
        used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
        window_minutes: 10_080,
        resets_at_ms: quota
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(parse_ts_ms)
            .unwrap_or(0),
        label: Some("Work · Weekly limit".to_string()),
        id: Some("totalQuota".to_string()),
    }]
}

/// A quota figure written as a number, or as a numeric string — the desktop
/// summary endpoint quotes them. Anything else is not a reading.
fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|n| n.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `set_var` is process-global, so the tests that pin a root or a base URL
    /// take turns rather than racing each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The vendor's snake_case wire, in the shape `parseManagedUsagePayload` reads.
    const PAYLOAD: &str = r#"{"usages":{"limit_5h":{"used_ratio":0.31,"reset_time":"2026-10-05T03:20:00Z"},
        "limit_7d":{"used_ratio":0.72,"reset_time":"2026-10-09T00:00:00Z"},
        "limit_month_total":{"used_ratio":0.18,"reset_time":"2026-11-01T00:00:00Z"},
        "limit_month_code":{"used_ratio":0.04,"reset_time":"2026-11-01T00:00:00Z"}},
        "boosterWallet":{"balance_cents":1200000,"total_cents":2000000,"monthly_charge_limit_enabled":true,
        "monthly_charge_limit_cents":5000000,"monthly_used_cents":800000,"currency":"CNY"}}"#;

    #[test]
    fn the_three_windows_the_product_draws_and_no_more() {
        let samples = samples_from(&serde_json::from_str::<Value>(PAYLOAD).unwrap());
        let rows: Vec<(&str, f64, i64)> = samples
            .iter()
            .map(|s| (s.label.as_deref().unwrap(), s.used_percent, s.window_minutes))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("5h limit", 31.0, 300),
                ("Weekly limit", 72.0, 10_080),
                ("Monthly limit", 18.0, 43_200),
            ],
            "month_code is the monthly row's breakdown, not a fourth window"
        );
        assert_eq!(samples[0].resets_at_ms, 1_791_170_400_000, "reset_time is parsed, not ignored");
        assert_eq!(samples[0].id.as_deref(), Some("limit_5h"), "the payload key names the window");
    }

    #[test]
    fn an_omitted_window_is_no_row_and_a_full_one_is_hundred_percent() {
        let body = json!({"usages": {"limit_5h": {"used_ratio": 1.0}}});
        let samples = samples_from(&body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].used_percent, 100.0);
        assert_eq!(samples[0].resets_at_ms, 0, "no reset_time is not a fake reset");
        assert!(samples_from(&json!({"usages": {}})).is_empty());
        // Buckets answered at the top level instead of nested under `usages`.
        assert_eq!(samples_from(&json!({"limit_7d": {"used_ratio": 0.5}})).len(), 1);
    }

    #[test]
    fn a_number_outside_ratio_range_or_a_garbage_one_is_not_a_window() {
        // 0-1 is the contract; a percent-scale 31 would read as 3100% used.
        assert!(samples_from(&json!({"usages": {"limit_5h": {"used_ratio": 31}}})).is_empty());
        assert!(samples_from(&json!({"usages": {"limit_5h": {"used_ratio": -0.2}}}))
            .is_empty());
        assert!(samples_from(&json!({"usages": {"limit_5h": {"used_ratio": "none"}}})).is_empty());
        // A stringified number is still a reading.
        assert_eq!(samples_from(&json!({"usages": {"limit_5h": {"used_ratio": "0.25"}}}))[0].used_percent, 25.0);
    }

    #[test]
    fn the_credential_file_is_the_one_the_cli_writes() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("credentials");
        std::fs::create_dir_all(&creds).unwrap();
        std::fs::write(
            creds.join("kimi-code.json"),
            r#"{"access_token":"at-1","refresh_token":"rt-1","expires_at":1800000000}"#,
        )
        .unwrap();
        std::fs::write(creds.join("other.json"), r#"{"access_token":""}"#).unwrap();
        std::fs::write(creds.join("later.json"), r#"{"access_token":"at-2","expires_at":1900000000}"#)
            .unwrap();
        assert_eq!(token_at(&creds.join("kimi-code.json")).as_deref(), Some("at-1"));
        assert!(token_at(&creds.join("other.json")).is_none(), "an empty token is no token");
        assert_eq!(expiry_at(&creds.join("kimi-code.json")), 1_800_000_000);
        // The longest-lived credential wins, and only its token is read: the
        // refresh token is never touched by this probe.
        std::env::set_var("KIMI_CODE_HOME", dir.path());
        assert_eq!(access_token().as_deref(), Some("at-2"));
        std::env::remove_var("KIMI_CODE_HOME");
    }

    #[test]
    fn no_credentials_dir_answers_nothing_rather_than_an_error() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("KIMI_CODE_HOME", dir.path().join("absent"));
        assert!(credentials_dir().is_none());
        assert!(KimiCodeQuota.fetch().is_empty(), "an unsigned-in tool has no quota to show");
        std::env::remove_var("KIMI_CODE_HOME");
    }

    #[test]
    fn the_base_ladder_prefers_the_override_then_cn_then_global() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("KIMI_CODE_BASE_URL", "http://127.0.0.1:1/v1/");
        assert_eq!(bases(), vec!["http://127.0.0.1:1/v1".to_string()]);
        std::env::remove_var("KIMI_CODE_BASE_URL");
        let defaults = bases();
        assert_eq!(defaults.len(), 2);
        assert!(defaults[0].ends_with("api.kimi.com/coding/v1"), "the vendor default first");
    }

    /// The desktop summary, verbatim from a live `agent-gw` answer: one quoted
    /// limit/used pair and a fractional-second reset.
    const DESKTOP_PAYLOAD: &str = r#"{"user":{"userId":"u_1","region":"cn","membership":{"level":"LEVEL_FREE"}},
        "totalQuota":{"limit":"100","used":"13","remaining":"87","resetTime":"2026-10-07T08:06:44.811902Z"},
        "authentication":{"method":"METHOD_API_KEY","scope":"FEATURE_WORK"},"subType":"TYPE_PURCHASE"}"#;

    #[test]
    fn the_desktop_summary_is_one_weekly_work_window() {
        let body: Value = serde_json::from_str(DESKTOP_PAYLOAD).unwrap();
        let samples = desktop_samples_from(&body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].used_percent, 13.0);
        assert_eq!(samples[0].window_minutes, 10_080, "the summary window is the weekly one");
        assert_eq!(samples[0].label.as_deref(), Some("Work · Weekly limit"));
        assert_eq!(samples[0].id.as_deref(), Some("totalQuota"));
        assert_eq!(samples[0].resets_at_ms, 1_791_360_404_811, "resetTime parses, fraction included");
    }

    #[test]
    fn a_remaining_only_desktop_answer_derives_the_share_and_an_overrun_clamps() {
        let samples = desktop_samples_from(&json!({"totalQuota": {"limit": 100, "remaining": "60"}}));
        assert_eq!(samples[0].used_percent, 40.0, "used is derived when only the remainder is stated");
        assert_eq!(samples[0].resets_at_ms, 0, "no resetTime is not a fake reset");
        // An overrun reads as fully used rather than past 100.
        assert_eq!(
            desktop_samples_from(&json!({"totalQuota": {"limit": "100", "used": "130"}}))[0]
                .used_percent,
            100.0
        );
        // A zero or absent limit is not a window, and neither is a foreign body.
        assert!(desktop_samples_from(&json!({"totalQuota": {"limit": 0, "used": 0}})).is_empty());
        assert!(desktop_samples_from(&json!({"unrelated": true})).is_empty());
        assert!(desktop_samples_from(&json!({"totalQuota": {"limit": 100, "used": "many"}})).is_empty());
    }

    #[test]
    fn a_desktop_config_without_the_runtime_key_is_not_a_credential() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        std::env::set_var("KIMI_DESKTOP_CONFIG", &config);
        std::fs::write(&config, r#"{"credentials":{"kimiWeb":{"accessToken":"t"}}}"#).unwrap();
        assert!(via_desktop().is_none(), "the web session token is not the runtime API key");
        std::fs::remove_file(&config).unwrap();
        assert!(via_desktop().is_none(), "an absent config answers nothing rather than erroring");
        std::env::remove_var("KIMI_DESKTOP_CONFIG");
    }

    #[test]
    fn a_pinned_fixture_root_keeps_the_desktop_config_out_of_reach() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        std::fs::write(&config, "{}").unwrap();
        std::env::set_var("KIMI_DESKTOP_CONFIG", &config);
        std::env::set_var("KIMI_CODE_HOME", dir.path().join("home"));
        assert!(desktop_config_path().is_none(), "a pinned root skips the desktop source outright");
        std::env::remove_var("KIMI_CODE_HOME");
        assert_eq!(
            desktop_config_path().as_deref(),
            Some(config.as_path()),
            "with no pinned root the override is honored"
        );
        std::env::remove_var("KIMI_DESKTOP_CONFIG");
    }
}

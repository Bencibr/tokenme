//! Claude Code quota.
//!
//! Claude Code never writes its 5-hour / weekly window into `~/.claude/projects`,
//! so spend alone can never answer "how close am I to the limit". The official
//! OAuth endpoint does, with the access token the CLI already stored for the
//! signed-in user. Nothing here logs in or refreshes a token: no usable
//! credential means no quota row.

use serde_json::Value;
use usage_core::{parse_ts_ms, QuotaSample};

use crate::http;
use crate::QuotaProbe;

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
/// Required by the OAuth API; without a User-Agent the endpoint answers 429.
const USER_AGENT: &str = concat!("tokenme/", env!("CARGO_PKG_VERSION"));
const BETA: &str = "oauth-2025-04-20";

pub struct ClaudeQuota;

impl QuotaProbe for ClaudeQuota {
    fn tool(&self) -> &'static str {
        "claude"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        live().unwrap_or_default()
    }
}

fn live() -> Option<Vec<QuotaSample>> {
    {
        let token = access_token()?;
        let body = http::get_json(
            ENDPOINT,
            &[
                ("authorization", &format!("Bearer {token}")),
                ("anthropic-beta", BETA),
                ("user-agent", USER_AGENT),
            ],
        )?;
        Some(samples_from(&body))
    }
}

/// Windows are calendar-fixed by the vendor; an unknown bucket keeps window 0 so
/// the UI shows the label instead of a wrong "N 小时窗口".
fn window_minutes(key: &str) -> i64 {
    if key.contains("hour") {
        300
    } else if key.contains("day") {
        10_080
    } else {
        0
    }
}

fn label_for(key: &str) -> String {
    match key {
        "five_hour" => "5 小时".into(),
        "seven_day" => "7 天".into(),
        "seven_day_sonnet" => "7 天 · Sonnet".into(),
        "seven_day_opus" => "7 天 · Opus".into(),
        other => other.to_string(),
    }
}

/// `utilization` is already a percentage (0-100), not a fraction.
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    let Some(obj) = body.as_object() else { return Vec::new() };
    let mut out = Vec::new();
    for (key, value) in obj {
        let Some(bucket) = value.as_object() else { continue };
        let Some(used_percent) = bucket.get("utilization").and_then(Value::as_f64) else { continue };
        let resets_at_ms = bucket
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(parse_ts_ms)
            .unwrap_or(0);
        out.push(QuotaSample {
            used_percent,
            window_minutes: window_minutes(key),
            resets_at_ms,
            label: Some(label_for(key)),
            id: None,
        });
    }
    out.sort_by_key(|s| s.window_minutes);
    out
}

/// The desktop CLI keeps its OAuth token in the macOS keychain and mirrors it to
/// a file; both hold `{"claudeAiOauth":{"accessToken": …}}`.
///
/// An **empty** `accessToken` is normal, not a bug to chase. Measured on this
/// machine: `security find-generic-password -s "Claude Code-credentials" -w`
/// succeeds (3 124 bytes) and `claudeAiOauth.accessToken` is `""`, because
/// `~/.claude/settings.json` routes the CLI through a local gateway
/// (`ANTHROPIC_BASE_URL=http://127.0.0.1:15721`, `ANTHROPIC_AUTH_TOKEN=PROXY_MANAGED`,
/// i.e. CC Switch). Such an install never holds an Anthropic OAuth token at all —
/// the proxy authenticates instead — so it has **no OAuth quota surface to probe**,
/// and returning no row is the correct answer. The endpoint needs the real
/// subscription token, which only exists on a machine where `claude /login` talks to
/// Anthropic directly (no `ANTHROPIC_BASE_URL` override).
fn access_token() -> Option<String> {
    keychain_token()
        .or_else(|| credentials_path().and_then(|p| file_token(&p)))
        .filter(|t| !t.is_empty())
}

fn credentials_path() -> Option<std::path::PathBuf> {
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
    Some(root.join(".credentials.json"))
}

fn file_token(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    token_in(&text)
}

/// Keychain first: the file is stale after a `claude login` on some builds.
#[cfg(target_os = "macos")]
fn keychain_token() -> Option<String> {
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    token_in(String::from_utf8_lossy(&out.stdout).trim())
}

#[cfg(not(target_os = "macos"))]
fn keychain_token() -> Option<String> {
    None
}

fn token_in(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()?
        .get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC_SAMPLE: &str = r#"{
      "five_hour": {"utilization": 12.5, "resets_at": "2026-09-23T18:00:00Z"},
      "seven_day": {"utilization": 88.25, "resets_at": "2026-09-27T18:00:00Z"},
      "seven_day_sonnet": {"utilization": 88.25, "resets_at": "2026-09-27T18:00:00Z"}
    }"#;

    #[test]
    fn every_reported_window_keeps_its_own_row() {
        let body: Value = serde_json::from_str(DOC_SAMPLE).unwrap();
        let samples = samples_from(&body);
        assert_eq!(samples.len(), 3, "the weekly Opus/Sonnet split is spend the user can hit");
        assert_eq!(samples.iter().filter(|s| s.window_minutes == 10_080).count(), 2);
    }

    #[test]
    fn utilization_is_a_percent_and_resets_at_is_milliseconds() {
        let body: Value = serde_json::from_str(DOC_SAMPLE).unwrap();
        let samples = samples_from(&body);
        let five = samples.iter().find(|s| s.window_minutes == 300).unwrap();
        assert_eq!(five.used_percent, 12.5, "already 0-100; must not be scaled");
        assert_eq!(five.label.as_deref(), Some("5 小时"));
        assert!(five.resets_at_ms > 1_700_000_000_000, "ms, not seconds: {}", five.resets_at_ms);
        let week = samples.iter().find(|s| s.window_minutes == 10_080).unwrap();
        assert_eq!(week.used_percent, 88.25);
    }

    #[test]
    fn unknown_buckets_keep_the_key_and_no_window() {
        let body: Value =
            serde_json::from_str(r#"{"monthly_forecast":{"utilization":3.5}}"#).unwrap();
        let samples = samples_from(&body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].window_minutes, 0);
        assert_eq!(samples[0].label.as_deref(), Some("monthly_forecast"));
        assert_eq!(samples[0].resets_at_ms, 0, "no reset known, not a fake one");
    }

    #[test]
    fn junk_payloads_yield_nothing() {
        assert!(samples_from(&Value::Null).is_empty());
        assert!(samples_from(&serde_json::from_str(r#"{"five_hour":"nope"}"#).unwrap()).is_empty());
        assert!(samples_from(&serde_json::from_str(r#"{"five_hour":{"foo":1}}"#).unwrap()).is_empty());
    }

    #[test]
    fn token_is_read_out_of_the_oauth_envelope() {
        let text = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-1","refreshToken":"x"}}"#;
        assert_eq!(token_in(text).as_deref(), Some("sk-ant-oat-1"));
        assert!(token_in(r#"{"other":1}"#).is_none());
        assert!(token_in("not json").is_none());
    }

    #[test]
    fn a_missing_credentials_file_yields_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.json");
        assert!(file_token(&missing).is_none());
        std::fs::write(&missing, b"{}").unwrap();
        assert!(file_token(&missing).is_none(), "no claudeAiOauth block is not a login");
    }

    /// What makes an install proxy-managed: `~/.claude/settings.json` →
    /// `env.ANTHROPIC_BASE_URL` (+ `ANTHROPIC_AUTH_TOKEN`), which CC Switch writes.
    #[cfg(test)]
    fn gateway_in_settings() -> Option<String> {
        let root = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
        let text = std::fs::read_to_string(root.join("settings.json")).ok()?;
        Some(serde_json::from_str::<Value>(&text).ok()?.get("env")?.get("ANTHROPIC_BASE_URL")?.as_str()?.to_string())
    }

    /// Real call against the signed-in account. Run with:
    /// `cargo test -p usage-quota -- --ignored --nocapture`
    ///
    /// Skips, rather than fails, when this install has no OAuth token to spend:
    /// see `access_token` for why a proxied (CC Switch) setup legitimately has none.
    #[test]
    #[ignore]
    fn live_endpoint_returns_this_account_windows() {
        let Some(token) = access_token() else {
            let why = match gateway_in_settings() {
                Some(url) => format!(" — settings.json routes the CLI through a gateway ({url}), so the CLI never holds an Anthropic token"),
                None => ", and no ANTHROPIC_BASE_URL override is set either".into(),
            };
            println!("SKIP claude: no non-empty claudeAiOauth.accessToken in the keychain or .credentials.json{why}.");
            println!("     An OAuth quota surface does not exist for this install; no row is the correct answer.");
            return;
        };
        drop(token); // never printed
        let samples = ClaudeQuota.fetch();
        assert!(!samples.is_empty(), "a token exists but no window came back: expired, or the endpoint moved?");
        for s in &samples {
            println!("claude quota: {:.2}% window={} label={:?} resets_at_ms={}", s.used_percent, s.window_minutes, s.label, s.resets_at_ms);
        }
    }
}

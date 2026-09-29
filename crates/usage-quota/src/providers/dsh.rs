//! DSH: the DeepSeek account balance behind the desktop agent.
//!
//! ## Why the balance endpoint and not a rate-limit window
//!
//! The desktop agent bills the user's own DeepSeek open-platform key
//! (prepaid CNY balance, pay-per-token — there is no subscription window to
//! run out of), and that key is the only credential the machine holds. The
//! platform's documented account endpoint answers it directly:
//!
//! ```text
//! GET https://api.deepseek.com/user/balance   (Bearer <key>)
//! → {"is_available":true,"balance_infos":[
//!      {"currency":"CNY","total_balance":"26.70",
//!       "granted_balance":"0.00","topped_up_balance":"26.70"}]}
//! ```
//!
//! Measured live on this machine 2026-09-29 (HTTP 200, real numbers).
//!
//! ## Where the key comes from
//!
//! The desktop app stores it in plain text in its harness credentials file —
//! `refs.DEEPSSEEK_API_KEY` in `.credentials.yaml` next to the sessions the
//! adapter reads (same home ladder: `DSH_HOME` override, then the platform
//! default). A missing key, an unparsable file, or a refused request all
//! answer `Vec::new()`; the bar the user configured elsewhere is never
//! blanked by a probe.
//!
//! ## How a balance becomes a bar
//!
//! A prepaid balance has no reset and no window, so the bar reads against the
//! money ever put in: used = 1 − remaining / (topped-up + granted). A fresh
//! top-up paints an empty bar that fills as the account drains. The label
//! carries the remaining figure itself — that is the number the user acts on.

use usage_core::QuotaSample;

use crate::{http::get_json, QuotaProbe};

const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

pub struct DshQuota;

impl QuotaProbe for DshQuota {
    fn tool(&self) -> &'static str {
        "dsh"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(key) = api_key() else { return Vec::new() };
        let authorization = format!("Bearer {key}");
        let Some(body) = get_json(
            BALANCE_URL,
            &[("authorization", authorization.as_str()), ("accept", "application/json")],
        ) else {
            return Vec::new();
        };
        sample_from(&body).into_iter().collect()
    }
}

/// The `refs.DEEPSSEEK_API_KEY` value, line-scanned: the file is small,
/// machine-written YAML and the probe must not grow a YAML dependency.
fn api_key() -> Option<String> {
    let path = credentials_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(("DEEPSEEK_API_KEY", value)) = line.trim().split_once(':') {
            let value = value.trim().trim_matches('"');
            return (!value.is_empty()).then_some(value.to_string());
        }
    }
    None
}

/// `<harness>/.credentials.yaml`, resolved like the adapter's session root:
/// the `DSH_HOME` override wins, then the platform default — `%APPDATA%` on
/// Windows (where `~/.dsh` carries no harness), `~/.dsh` elsewhere.
fn credentials_path() -> Option<std::path::PathBuf> {
    if let Some(home) = std::env::var_os("DSH_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        let path = home.join("harness").join(".credentials.yaml");
        return path.is_file().then_some(path);
    }
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if cfg!(target_os = "windows") {
        if let Some(config) = dirs::config_dir() {
            candidates.push(config.join("dsh-desktop").join("harness").join(".credentials.yaml"));
        }
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".dsh").join("harness").join(".credentials.yaml"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// The newest balance info (the endpoint answers one per currency the account
/// ever held; the first is the live one).
fn sample_from(body: &serde_json::Value) -> Option<QuotaSample> {
    if body.get("is_available").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    let info = body.pointer("/balance_infos/0")?;
    let currency = info.get("currency").and_then(serde_json::Value::as_str).unwrap_or("CNY");
    let f64_of = |k: &str| info.get(k).and_then(serde_json::Value::as_str).and_then(|s| s.trim().parse::<f64>().ok());
    let total = f64_of("total_balance")?;
    let reference = f64_of("topped_up_balance")?.max(0.0) + f64_of("granted_balance")?.max(0.0);
    let symbol = match currency {
        "CNY" => "¥",
        "USD" => "$",
        other => other,
    };
    let used_percent = if reference > 0.0 {
        ((1.0 - total / reference) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    Some(QuotaSample {
        used_percent,
        window_minutes: 0,
        resets_at_ms: 0,
        label: Some(format!("余额 {symbol}{total:.2}")),
        id: Some("dsh-balance".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r#"{"is_available":true,"balance_infos":[
        {"currency":"CNY","total_balance":"26.70","granted_balance":"0.00","topped_up_balance":"26.70"}]}"#;

    #[test]
    fn the_balance_becomes_a_bar_against_the_money_put_in() {
        let body: serde_json::Value = serde_json::from_str(LIVE).unwrap();
        let s = sample_from(&body).expect("a funded account has a bar");
        assert_eq!(s.label.as_deref(), Some("余额 ¥26.70"));
        assert_eq!(s.used_percent, 0.0, "untouched top-up reads as unused");
        assert_eq!(s.window_minutes, 0, "a balance has no reset window");
        assert_eq!(s.resets_at_ms, 0);
        assert_eq!(s.id.as_deref(), Some("dsh-balance"));
    }

    #[test]
    fn spending_fills_the_bar_and_a_grant_counts_as_money_in() {
        let drained = serde_json::json!({
            "is_available": true,
            "balance_infos": [{"currency": "CNY", "total_balance": "13.325",
                               "granted_balance": "10.00", "topped_up_balance": "16.65"}]
        });
        let s = sample_from(&drained).expect("half-spent balance");
        assert!((s.used_percent - 50.0).abs() < 0.01, "13.325 of 26.65 in: {s:?}");

        let unavailable: serde_json::Value = serde_json::from_str(r#"{"is_available":false,"balance_infos":[]}"#).unwrap();
        assert!(sample_from(&unavailable).is_none());
    }
}

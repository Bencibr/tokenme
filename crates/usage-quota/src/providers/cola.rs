//! Cola (the Electron coding agent): plan allowance and credit balance through
//! the vendor's billing API.
//!
//! ## The call
//!
//! The desktop app's billing panel reads two endpoints on the API origin (the
//! LLM gateway on `zhuoma.*` is a different service and rejects these tokens):
//!
//! ```text
//! GET https://api.colaos.ai/v1/billing/me/plan
//! GET https://api.colaos.ai/v1/billing/balance
//! headers: Authorization: Bearer <accessToken>
//! → {ok: true, data: {...}}
//! ```
//!
//! The plan answer carries the quota fact — `remaining_pct` (already remaining,
//! so the bar draws `100 - remaining_pct` used), `next_reset_at` (midnight UTC,
//! the daily reset), `plan_tier` ("breeze") and `entitlement_source`
//! ("trial"). The balance answer carries only remaining credits — no package
//! total, so an honest used-percent does not exist; that bar stays empty and
//! the label carries the real numbers, the same rule the Agnes probe follows.
//! A user-set daily spend limit (`/v1/billing/me/daily-balance-limit`) joins
//! as a third bar only when it is actually set.
//!
//! ## Where the token comes from
//!
//! Cola writes its login record to `~/.cola/auth.json` behind a
//! `cola.enc.v1:` envelope — **local obfuscation, not secret-keeping**: the
//! AES-256-GCM key is derived from four base64 chunks baked into the shipped
//! bundle (`qas` in the server binary, `va` in the asar; decoded they read
//! `cola-config:protection-v1:local-only:obfuscation`, hashed with SHA-256).
//! Layout is `base64(12B iv ‖ 16B tag ‖ ciphertext)`; the plaintext is
//! `{authTokens: {accessToken, refreshToken, expiresAt?}}` (the same reader the
//! server's own `auth.get` RPC serves). Verified against this machine's real
//! account 2026-09-30: all three endpoints answered HTTP 200.
//!
//! Two things this deliberately is not: `~/.cola/identity/gateway-token`
//! authenticates the *local* server's WebSocket only and gets 401 from this
//! API, and the china-region build keeps its home in `~/.cola-cn` — checked
//! after `~/.cola`. `COLA_HOME` overrides the directory for tests.
//!
//! ## Host-exit pause
//!
//! Registered in `host::HOST_PROCESSES` (`Cola`, `cola-server`), so the
//! panel's 退出后暂停配额 switch gates this probe like every other one: Cola
//! quit means the bars freeze at their last known answer instead of the panel
//! calling the vendor for a number that cannot change.

use std::path::PathBuf;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use serde_json::Value;
use sha2::Digest as _;
use usage_core::QuotaSample;

use crate::http::get_json;
use crate::QuotaProbe;

pub struct ColaQuota;

const API_BASE: &str = "https://api.colaos.ai";
const PLAN_PATH: &str = "/v1/billing/me/plan";
const BALANCE_PATH: &str = "/v1/billing/balance";
const DAILY_LIMIT_PATH: &str = "/v1/billing/me/daily-balance-limit";

/// The envelope's key material, exactly as shipped: four base64 chunks whose
/// decoded forms join with `:` before hashing. Keeping them encoded documents
/// where they came from; decoding them at compile time is not possible, and at
/// runtime it is two lines.
const ENVELOPE_PARTS: [&str; 4] = [
    "Y29sYS1jb25maWc=",
    "cHJvdGVjdGlvbi12MQ==",
    "bG9jYWwtb25seQ==",
    "b2JmdXNjYXRpb24=",
];

fn envelope_key() -> [u8; 32] {
    let joined: String = ENVELOPE_PARTS
        .iter()
        .map(|p| base64::engine::general_purpose::STANDARD.decode(p).unwrap())
        .map(|b| String::from_utf8(b).unwrap())
        .collect::<Vec<_>>()
        .join(":");
    sha2::Sha256::digest(joined.as_bytes()).into()
}

/// `COLA_HOME` wins (tests, side-by-side installs), then the global build's
/// `~/.cola`, then the china build's `~/.cola-cn`.
fn cola_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("COLA_HOME") {
        return (!dir.is_empty()).then(|| PathBuf::from(dir));
    }
    let home = home_dir()?;
    let global = home.join(".cola");
    if global.join("auth.json").is_file() {
        return Some(global);
    }
    let china = home.join(".cola-cn");
    if china.join("auth.json").is_file() {
        return Some(china);
    }
    None
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).or_else(|| dirs::home_dir())
}

/// One envelope, opened. Any failure reads as "no credential": a rotated
/// prefix, a changed layout, or a truncated file all end the same way, with
/// the probe silent rather than the panel wrong.
fn open_envelope(raw: &str) -> Option<String> {
    let sealed = raw.trim().strip_prefix("cola.enc.v1:")?;
    let blob = base64::engine::general_purpose::STANDARD.decode(sealed).ok()?;
    if blob.len() < 12 + 16 + 1 {
        return None;
    }
    let (iv, rest) = blob.split_at(12);
    let (tag, body) = rest.split_at(16);
    // `aead` wants the tag appended to the ciphertext; the envelope keeps
    // them apart, and forgetting this reads as "every login is unreadable".
    let mut payload = body.to_vec();
    payload.extend_from_slice(tag);
    let plain = Aes256Gcm::new_from_slice(&envelope_key())
        .ok()?
        .decrypt(Nonce::from_slice(iv), payload.as_ref())
        .ok()?;
    String::from_utf8(plain).ok()
}

/// The login record's access token — what the billing API authenticates with.
fn access_token() -> Option<String> {
    let raw = std::fs::read_to_string(cola_home()?.join("auth.json")).ok()?;
    let parsed: Value = serde_json::from_str(&open_envelope(&raw)?).ok()?;
    let token = parsed.pointer("/authTokens/accessToken")?.as_str()?;
    (!token.is_empty()).then(|| token.to_string())
}

/// `{"ok":true,"data":{...}}` → the inner object; anything else is silence.
fn data_of(body: Value) -> Option<Value> {
    let data = body.get("data")?.clone();
    body.get("ok").and_then(Value::as_bool).filter(|ok| *ok)?;
    Some(data)
}

impl QuotaProbe for ColaQuota {
    fn tool(&self) -> &'static str {
        "cola"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(token) = access_token() else { return Vec::new() };
        let auth = [("authorization", format!("Bearer {token}"))];
        let headers: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let mut out = Vec::new();
        if let Some(body) = get_json(&format!("{API_BASE}{PLAN_PATH}"), &headers) {
            out.extend(plan_samples(data_of(body).as_ref()));
        }
        if let Some(body) = get_json(&format!("{API_BASE}{BALANCE_PATH}"), &headers) {
            if let Some(balance) = data_of(body) {
                out.push(balance_sample(&balance));
            }
        }
        // The daily bar exists only for accounts that set a spend cap; the
        // default (null limit) is "no limit", which is no bar.
        if let Some(body) = get_json(&format!("{API_BASE}{DAILY_LIMIT_PATH}"), &headers) {
            if let Some(daily) = data_of(body) {
                if let Some(sample) = daily_limit_sample(&daily) {
                    out.push(sample);
                }
            }
        }
        out
    }
}

/// The plan bar: the vendor's own remaining share of the daily allowance.
fn plan_samples(plan: Option<&Value>) -> Vec<QuotaSample> {
    let Some(plan) = plan else { return Vec::new() };
    let Some(remaining) = plan.get("remaining_pct").and_then(Value::as_f64) else {
        return Vec::new();
    };
    let tier = plan.get("plan_tier").and_then(Value::as_str).unwrap_or("plan");
    let source = plan.get("entitlement_source").and_then(Value::as_str).unwrap_or("");
    let mut label = format!("{tier} 套餐 · 剩余 {}%", trim(remaining));
    if source == "trial" {
        label.push_str(" · 试用");
    }
    vec![QuotaSample {
        used_percent: (100.0 - remaining).clamp(0.0, 100.0),
        window_minutes: 0,
        resets_at_ms: parse_rfc3339(plan.get("next_reset_at").and_then(Value::as_str)).unwrap_or(0),
        label: Some(label),
        id: Some("plan".into()),
    }]
}

/// The credits bar: remaining balances with no package total, so the bar stays
/// empty and the label carries the numbers (the Agnes rule).
fn balance_sample(balance: &Value) -> QuotaSample {
    let credits = balance.get("balance_credits").and_then(Value::as_f64).unwrap_or(0.0);
    let usd = balance.get("balance_usd").and_then(Value::as_str).unwrap_or("0");
    let gift = balance.get("gift_balance_usd").and_then(Value::as_str).unwrap_or("0");
    QuotaSample {
        used_percent: 0.0,
        window_minutes: 0,
        resets_at_ms: 0,
        label: Some(format!("余额 ${} · 赠送 ${} · {} credits", trim_str(usd), trim_str(gift), trim(credits))),
        id: Some("balance".into()),
    }
}

/// The spend-cap bar, only when a cap exists (`null` limits mean the account
/// has none — most accounts, including this machine's). Credits and cents are
/// the same unit here ($0.01 per credit, per the balance answer's own
/// `balance_credits: 1000` ↔ `balance_usd: "1"`), so the credits fields
/// compare directly.
fn daily_limit_sample(daily: &Value) -> Option<QuotaSample> {
    let limit = daily
        .get("daily_limit_credits")
        .and_then(Value::as_f64)
        .or_else(|| daily.get("daily_limit_cents").and_then(Value::as_f64))?;
    if limit <= 0.0 {
        return None;
    }
    let consumed = daily
        .get("daily_consumed_credits")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    Some(QuotaSample {
        used_percent: (consumed / limit * 100.0).clamp(0.0, 100.0),
        window_minutes: 1440,
        resets_at_ms: parse_rfc3339(daily.get("next_reset_at").and_then(Value::as_str)).unwrap_or(0),
        label: Some(format!("每日限额 · 已用 {}/{}", trim(consumed), trim(limit))),
        id: Some("daily-limit".into()),
    })
}

/// `"2026-10-01T00:00:00.000Z"` → epoch ms; 0 when absent or unparseable, the
/// "no reset known" the renderers already handle.
fn parse_rfc3339(ts: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts?).ok().map(|t| t.timestamp_millis())
}

/// Whole when whole, one decimal when fractional (same rule as the others).
fn trim(n: f64) -> String {
    if n.fract() == 0.0 { format!("{n:.0}") } else { format!("{n:.1}") }
}

/// Same trim for the string-typed dollar figures the API returns.
fn trim_str(s: &str) -> String {
    s.parse::<f64>().map(trim).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The key material is pinned by construction, but the envelope reader is
    /// proven against a real round trip: encrypt with the derived key exactly
    /// the way the app does (iv ‖ tag ‖ body, standard base64) and open it.
    #[test]
    fn the_envelope_round_trips() {
        use aes_gcm::aead::Aead;
        let key = envelope_key();
        let iv: [u8; 12] = rand_iv();
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let sealed = cipher.encrypt(Nonce::from_slice(&iv), b"{\"x\":1}".as_ref()).unwrap();
        let mut blob = iv.to_vec();
        let (body, tag) = sealed.split_at(sealed.len() - 16);
        blob.extend_from_slice(tag);
        blob.extend_from_slice(body);
        let raw = format!(
            "cola.enc.v1:{}",
            base64::engine::general_purpose::STANDARD.encode(blob)
        );
        assert_eq!(open_envelope(&raw).as_deref(), Some("{\"x\":1}"));
    }

    fn rand_iv() -> [u8; 12] {
        let mut iv = [0u8; 12];
        // Deterministic in tests is fine; the tag is what authenticates.
        for (i, b) in iv.iter_mut().enumerate() {
            *b = (i * 7 + 3) as u8;
        }
        iv
    }

    /// The plan answer this machine's account actually gave (2026-09-30),
    /// trimmed to the fields the probe reads: a full trial allowance.
    const MEASURED_PLAN: &str = r#"{"ok":true,"data":{"plan_tier":"breeze","state":"ok",
        "remaining_pct":100,"next_reset_at":"2026-10-01T00:00:00.000Z",
        "overflow_to_credits":false,"entitlement_source":"trial","can_purchase":true,
        "can_upgrade":false,"subscription_expires_at":"2026-10-02T12:38:43.359Z",
        "reset_cards":{"count":0,"items":[]}}}"#;

    #[test]
    fn the_measured_plan_becomes_an_empty_bar_with_a_tier_label() {
        let body: Value = serde_json::from_str(MEASURED_PLAN).unwrap();
        let s = plan_samples(data_of(body).as_ref());
        assert_eq!(s.len(), 1, "{s:?}");
        let bar = &s[0];
        assert_eq!(bar.used_percent, 0.0);
        assert_eq!(bar.label.as_deref(), Some("breeze 套餐 · 剩余 100% · 试用"));
        // 2026-10-01T00:00:00Z
        assert_eq!(bar.resets_at_ms, 1_790_812_800_000);
        assert_eq!(bar.id.as_deref(), Some("plan"));
    }

    /// A partly spent plan draws a real bar: 30 remaining is 70 used.
    #[test]
    fn a_spent_plan_draws_used_from_remaining() {
        let plan = json!({"ok": true, "data": {"plan_tier": "breeze", "remaining_pct": 30.0,
            "next_reset_at": "2026-10-01T00:00:00Z", "entitlement_source": "subscription"}});
        let s = plan_samples(data_of(plan).as_ref());
        assert_eq!(s[0].used_percent, 70.0);
        assert_eq!(s[0].label.as_deref(), Some("breeze 套餐 · 剩余 30%"));
    }

    #[test]
    fn a_not_ok_or_shapeless_answer_is_silence() {
        assert!(plan_samples(None).is_empty());
        let bad: Value = serde_json::from_str(r#"{"ok":false}"#).unwrap();
        assert!(data_of(bad).is_none());
        let empty: Value = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(data_of(empty).is_none());
    }

    const MEASURED_BALANCE: &str = r#"{"ok":true,"data":{"gift_balance_credits":1000,
        "purchased_balance_credits":0,"balance_credits":1000,"total_granted_credits":0,
        "balance_usd":"1","gift_balance_usd":"1","purchased_balance_usd":"0"}}"#;

    #[test]
    fn the_measured_balance_reads_as_an_empty_bar_with_real_numbers() {
        let body: Value = serde_json::from_str(MEASURED_BALANCE).unwrap();
        let bar = balance_sample(&data_of(body).unwrap());
        assert_eq!(bar.used_percent, 0.0);
        assert_eq!(bar.label.as_deref(), Some("余额 $1 · 赠送 $1 · 1000 credits"));
        assert_eq!(bar.resets_at_ms, 0);
        assert_eq!(bar.id.as_deref(), Some("balance"));
    }

    #[test]
    fn no_daily_limit_means_no_daily_bar() {
        let none: Value = serde_json::from_str(
            r#"{"ok":true,"data":{"daily_limit_cents":null,"daily_consumed_credits":0}}"#,
        )
        .unwrap();
        assert!(daily_limit_sample(&data_of(none).unwrap()).is_none());
        let capped: Value = serde_json::from_str(
            r#"{"ok":true,"data":{"daily_limit_cents":500,"daily_consumed_credits":100,
               "next_reset_at":"2026-10-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let bar = daily_limit_sample(&data_of(capped).unwrap()).unwrap();
        assert_eq!(bar.used_percent, 20.0);
        assert_eq!(bar.window_minutes, 1440);
        assert_eq!(bar.id.as_deref(), Some("daily-limit"));
    }
}

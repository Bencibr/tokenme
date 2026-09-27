//! ZCode: the provider quota interface the IDE itself polls, plus the
//! per-session token budgets the CLI keeps in its own database.
//!
//! ## Live, through the app's own interface
//!
//! ZCode's usage panel is described by its own locale as 来自当前供应商额度接口,
//! and that interface is plain HTTPS — no cached LevelDB snapshot in between:
//!
//! * Current ZCode desktop (`3.14.x`): `GET https://zcode.z.ai/api/v1/zcode-plan/billing/balance?app_version=...`
//!   → `{code, data: {plans[], balances[]}}`. These are the Start Plan buckets
//!   shown by the current panel and are the primary source below.
//! * `GET {base}/api/monitor/usage/quota/limit` → `{code, data: {level,
//!   limits[]}}`, the windows the panel draws. `base` is `https://bigmodel.cn`
//!   for a BigModel coding plan and `https://api.z.ai` for a Z.ai one
//!   (`ZCODE_BIGMODEL_USAGE_QUOTA_URL` / `BIGMODEL_USAGE_QUOTA_URL` override, the
//!   same names the app itself honors). This is retained as a legacy fallback.
//! * `GET https://zcode.z.ai/api/v1/mcp/usage` → the "ZCode MCP" call meter.
//! * `GET {base}/api/biz/subscription/list` → the plan's display name
//!   (`GLM Coding Lite`), so a bar can say which tier it describes.
//!
//! The current balance endpoint uses the OAuth-issued `zcodejwttoken`; the
//! legacy quota endpoint uses the account's coding-plan API key, which the app stores in
//! `~/.zcode/v2/credentials.json` under
//! `account-provider:coding-plan:account:<plan>:account:<id>:api-key`; the
//! account's `oauth:<family>:access_token` answers the same endpoint (measured)
//! and is the fallback. Both are read through the same `enc:v1:` envelope the
//! mcp meter already opens, and nothing is ever refreshed — see `session()`.
//!
//! ## Window naming follows the product, not a guess
//!
//! The vendor's docs (zcode.z.ai/cn/docs/usage-stats) name the windows
//! 5 小时 prompt 池 / 每周额度 / 工具调用（MCP 每月额度）, and its locale does
//! the same (`entitlementFiveHourUsage` "5 小时剩余", `entitlementMonthlyMcpUsage`
//! "工具调用", `entitlementServerMcpUsage` "ZCode MCP"). There is no daily and no
//! generic "monthly token" window, so none is drawn:
//!
//! * `TOKENS_LIMIT`/`CREDIT_LIMIT` `(unit 3, number 5)` → the 5 小时 prompt 池.
//! * `TOKENS_LIMIT`/`CREDIT_LIMIT` `unit 6` → 每周 (paid tiers only).
//! * `TIME_LIMIT` `(unit 5, number 1)` → 工具调用, the monthly tool-call
//!   allowance. It is not a token window: an earlier build drew it as "1 月",
//!   which is exactly the mislabel this module no longer produces.
//!
//! * `percentage` is already the used share (measured: `currentValue: 0` of
//!   `remaining: 100` carries `percentage: 0`); the IDE subtracts it from 100 to
//!   draw its remaining-ring, a bar that shows 已用 takes it directly.
//! * `usage` is not usable as a numerator or denominator: the monthly record
//!   carries `usage: 100` beside `remaining: 100, currentValue: 0`. The fallback
//!   is `currentValue / (currentValue + remaining)`, which agrees in both.
//! * `nextResetTime` is epoch milliseconds on this endpoint (the mcp meter's
//!   `next_refresh_at`, by contrast, is seconds).

use std::path::{Path, PathBuf};

use aes_gcm::aead::Aead;
use sha2::Digest as _;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use usage_core::QuotaSample;

use crate::http::get_json;
use crate::QuotaProbe;

pub struct ZcodeQuota;

/// The provider quota meter the IDE's usage panel is built on. Live, so nothing
/// here can lag behind what ZCode itself shows.
const QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
/// Where the plan's display name ("GLM Coding Lite") comes from.
const SUBSCRIPTION_PATH: &str = "/api/biz/subscription/list";
/// The meter ZCode's own host polls every few minutes, over its own origin.
const MCP_USAGE_URL: &str = "https://zcode.z.ai/api/v1/mcp/usage";

/// ZCode 3.14.x replaced the old BigModel `limits` response with the balance
/// buckets used by its own usage panel. The app sends the client metadata below
/// as part of every request to this endpoint; in particular, the device id is
/// required by the gateway and is read from the same local state file.
const ZCODE_ORIGIN: &str = "https://zcode.z.ai";
const ZCODE_PLAN_BALANCE_PATH: &str = "/api/v1/zcode-plan/billing/balance";
const ZCODE_APP_VERSION: &str = "3.14.3";

/// The same override pair the app honors, in the same order.
const URL_OVERRIDES: [&str; 2] = ["ZCODE_BIGMODEL_USAGE_QUOTA_URL", "BIGMODEL_USAGE_QUOTA_URL"];

/// A paused target still holds its budget; a completed one has closed the books.
const LIVE: [&str; 2] = ["active", "budget_limited"];

const MINUTE: i64 = 60_000;

impl QuotaProbe for ZcodeQuota {
    fn tool(&self) -> &'static str {
        "zcode"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        // Both meters are live, so both are tried independently: an account
        // whose coding-plan key rotated out from under us still gets its MCP
        // bar, and vice versa.
        let auth = usage_auth();
        let plan = auth.as_ref().and_then(plan_name);
        let mut out = auth
            .as_ref()
            .and_then(|a| quota_windows(a, plan.as_deref()))
            .unwrap_or_default();
        if let Some(samples) = zcode_plan_windows() {
            out.extend(samples);
        }
        out.extend(mcp_meter(plan.as_deref()).unwrap_or_default());
        if let Some(db) = db_path() {
            if let Ok(conn) = Connection::open_with_flags(
                format!("file:{}?mode=ro", db.display()),
                OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) {
                out.extend(budget_samples(&conn));
            }
        }
        out
    }
}

// ------------------------------------------------------- the Start Plan bucket

// ------------------------------------------------- the provider quota interface

/// One ready-to-send credential against the quota interface.
struct UsageAuth {
    /// A complete `authorization` header value, `Bearer` included.
    authorization: String,
    /// Whether the account is on the Z.ai family; picks the origin below.
    zai: bool,
    /// Origin the quota and subscription endpoints live under.
    base: String,
}

fn base_url(zai: bool) -> String {
    if zai { "https://api.z.ai".to_string() } else { "https://bigmodel.cn".to_string() }
}

fn quota_url(zai: bool) -> String {
    quota_url_with(zai, URL_OVERRIDES.iter().find_map(|k| std::env::var(k).ok()))
}

fn quota_url_with(zai: bool, override_url: Option<String>) -> String {
    if let Some(u) = override_url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return u;
    }
    format!("{}{QUOTA_PATH}", base_url(zai))
}

/// The account's credential against the quota interface, chosen the way the app
/// chooses it: the active provider's personal coding-plan key, then any
/// personal key, then the account's access token. `store` is the decrypted-view
/// of `~/.zcode/v2/credentials.json`; `key` opens its `enc:v1:` envelopes.
fn usage_auth_from(store: &Value, key: &[u8; 32]) -> Option<UsageAuth> {
    const PREFIX: &str = "account-provider:coding-plan:account:";
    const SUFFIX: &str = ":api-key";
    let active_zai = store.get("oauth:active_provider").and_then(Value::as_str) == Some("zai");
    let mut best: Option<(u8, bool, String)> = None;
    for k in store.as_object()?.keys() {
        let Some(plan) = k.strip_prefix(PREFIX).and_then(|rest| rest.strip_suffix(SUFFIX)) else {
            continue;
        };
        let Some(value) = opened(store, k, key) else { continue };
        let zai = plan.contains("zai");
        let score = (zai == active_zai) as u8 * 2 + plan.contains("individual") as u8;
        if best.as_ref().is_none_or(|(s, _, _)| score > *s) {
            best = Some((score, zai, value));
        }
    }
    match best {
        Some((_, zai, value)) => Some(UsageAuth { authorization: format!("Bearer {value}"), zai, base: base_url(zai) }),
        None => {
            let family = if active_zai { "zai" } else { "bigmodel" };
            let value = opened(store, &format!("oauth:{family}:access_token"), key)?;
            Some(UsageAuth { authorization: format!("Bearer {value}"), zai: active_zai, base: base_url(active_zai) })
        }
    }
}

fn usage_auth() -> Option<UsageAuth> {
    let store = credential_store()?;
    usage_auth_from(&store, &envelope_key())
}

/// Read the current Start Plan balance used by ZCode's 3.14.x usage panel.
/// This is deliberately independent of `usage_auth`: the Start Plan endpoint
/// accepts the OAuth-issued ZCode JWT, while the legacy limits endpoint uses a
/// coding-plan API key.
fn zcode_plan_windows() -> Option<Vec<QuotaSample>> {
    let store = credential_store()?;
    let key = envelope_key();
    let token = opened(&store, "zcodejwttoken", &key)?;
    let device_mid = device_mid();
    let authorization = format!("Bearer {token}");
    let headers = zcode_headers(&authorization, device_mid.as_deref());
    let url = zcode_plan_balance_url();
    let body = get_json(&url, &headers)?;
    Some(samples_from_balance(&body))
}

fn zcode_plan_balance_url() -> String {
    format!(
        "{ZCODE_ORIGIN}{ZCODE_PLAN_BALANCE_PATH}?app_version={ZCODE_APP_VERSION}"
    )
}

/// These are the same source headers ZCode's Node client adds before sending
/// a request to its own origin. `X-Device-Mid` is not a secret, but omitting it
/// makes the balance gateway answer code 3001 (parameter error).
fn zcode_headers<'a>(authorization: &'a str, device_mid: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut headers = vec![
        ("authorization", authorization),
        ("accept", "application/json"),
        ("user-agent", "ZCode/3.14.3"),
        ("http-referer", ZCODE_ORIGIN),
        ("x-title", "Z Code@electron"),
        ("x-zcode-app-version", ZCODE_APP_VERSION),
        ("x-platform", zcode_platform_key()),
        ("x-client-language", "unknown"),
        ("x-client-timezone", "unknown"),
        ("x-os-category", zcode_os_category()),
    ];
    if let Some(device_mid) = device_mid {
        headers.push(("x-device-mid", device_mid));
    }
    headers
}

fn zcode_platform_key() -> &'static str {
    let platform = if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        std::env::consts::OS
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "ia32"
    } else {
        std::env::consts::ARCH
    };
    // The production targets are the only ones ZCode labels specially. Keep
    // the fallback platform/arch contract readable for other Rust targets.
    match (platform, arch) {
        ("win32", "x64") => "win32-x64",
        ("win32", "arm64") => "win32-arm64",
        ("darwin", "x64") => "darwin-x64",
        ("darwin", "arm64") => "darwin-arm64",
        ("linux", "x64") => "linux-x64",
        ("linux", "arm64") => "linux-arm64",
        _ => "unknown-unknown",
    }
}

fn zcode_os_category() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

fn device_mid() -> Option<String> {
    let raw = std::fs::read_to_string(zcode_home()?.join("v2").join("telemetry-state.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("deviceMid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The quota windows, straight from the interface the IDE reads. `None` means
/// "this account answered nothing", which is not the same as "no windows".
fn quota_windows(auth: &UsageAuth, plan: Option<&str>) -> Option<Vec<QuotaSample>> {
    let body = get_json(
        &quota_url(auth.zai),
        &[("authorization", auth.authorization.as_str()), ("accept", "application/json")],
    )?;
    // A rejected credential answers HTTP 200 with `{"code":401,…}` and no
    // `data`; the app treats that as "no answer", and so does this.
    let data = body.get("data")?;
    let level = data.get("level").and_then(Value::as_str).map(str::to_string);
    Some(samples_from_limits(
        data.get("limits").unwrap_or(&Value::Null),
        plan.or(level.as_deref()),
    ))
}

/// The plan this account is on, as the vendor's own subscription list names it.
/// Only a `VALID` entry counts; an expired one is not the plan in use.
fn plan_name(auth: &UsageAuth) -> Option<String> {
    let body = get_json(
        &format!("{}{SUBSCRIPTION_PATH}", auth.base),
        &[("authorization", auth.authorization.as_str()), ("accept", "application/json")],
    )?;
    let entries = body.get("data")?.as_array()?;
    let chosen = entries
        .iter()
        .find(|e| e.get("status").and_then(Value::as_str) == Some("VALID"))
        .or_else(|| entries.first())?;
    chosen.get("productName").and_then(Value::as_str).map(str::to_string)
}

/// The windows the vendor reports, in the vendor's own order and naming.
pub(crate) fn samples_from_limits(limits: &Value, plan: Option<&str>) -> Vec<QuotaSample> {
    limits.as_array().map(|arr| arr.iter().filter_map(|l| limit_sample(l, plan)).collect()).unwrap_or_default()
}

/// Convert the `data.plans`/`data.balances` envelope returned by ZCode 3.14.x.
/// Only balances belonging to an active Start Plan are shown; this matches the
/// vendor UI and prevents an expired or orphaned bucket from being presented as
/// current quota.
pub(crate) fn samples_from_balance(body: &Value) -> Vec<QuotaSample> {
    let data = match body.get("data") {
        Some(data) => data,
        None => return Vec::new(),
    };
    let plans = match data.get("plans").and_then(Value::as_array) {
        Some(plans) => plans,
        None => return Vec::new(),
    };
    let active = plans.iter().find(|plan| {
        plan.get("status").and_then(Value::as_str).is_some_and(|status| status.eq_ignore_ascii_case("active"))
            && is_start_plan(plan)
    });
    let Some(active) = active else { return Vec::new() };
    let plan_id = text_field(active, "plan_id");
    let user_plan_id = text_field(active, "user_plan_id");
    // The label names buckets by their model, not the plan — the plan is
    // context the group header already carries.
    let _plan_name = text_field(active, "name")
        .or_else(|| plan_id.clone())
        .unwrap_or_else(|| "ZCode Start Plan".to_string());
    data.get("balances")
        .and_then(Value::as_array)
        .map(|balances| {
            let owned: Vec<&Value> = balances
                .iter()
                .filter(|balance| {
                    let same_user_plan = user_plan_id.as_deref().is_some_and(|id| text_field(balance, "user_plan_id").as_deref() == Some(id));
                    let same_plan = plan_id.as_deref().is_some_and(|id| text_field(balance, "plan_id").as_deref() == Some(id));
                    same_user_plan || same_plan
                })
                .collect();
            let multi_bucket = owned.len() > 1;
            owned
                .iter()
                .filter_map(|balance| balance_sample(balance, multi_bucket))
                .collect()
        })
        .unwrap_or_default()
}

fn is_start_plan(plan: &Value) -> bool {
    ["plan_id", "name"].iter().filter_map(|key| text_field(plan, key)).any(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("start-plan") || value.contains("start plan")
    })
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn number_field(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(|raw| {
        raw.as_f64().or_else(|| raw.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
    })
}

fn epoch_ms(value: Option<f64>) -> i64 {
    let value = value.filter(|value| value.is_finite() && *value > 0.0).unwrap_or(0.0);
    if value <= 0.0 {
        0
    } else if value < 100_000_000_000.0 {
        (value * 1000.0) as i64
    } else {
        value as i64
    }
}

/// 6650999 → 6.65M · 300000000 → 300M — bucket sizes are token counts, and the
/// label stays inside the panel.
fn compact_units(n: f64) -> String {
    let (v, unit) = if n.abs() >= 1e9 {
        (n / 1e9, "B")
    } else if n.abs() >= 1e6 {
        (n / 1e6, "M")
    } else if n.abs() >= 1e3 {
        (n / 1e3, "K")
    } else {
        (n, "")
    };
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}{unit}")
}

fn balance_sample(balance: &Value, multi_bucket: bool) -> Option<QuotaSample> {
    let total = number_field(balance, "total_units");
    let used = number_field(balance, "used_units");
    let remaining = number_field(balance, "remaining_units");
    let denominator = total.or_else(|| Some(used? + remaining?))?;
    if denominator <= 0.0 || !denominator.is_finite() {
        return None;
    }
    let used = used.or_else(|| Some(denominator - remaining?))?.clamp(0.0, denominator);
    let start = epoch_ms(number_field(balance, "period_start"));
    let end = epoch_ms(number_field(balance, "period_end"));
    let reset = epoch_ms(number_field(balance, "expires_at")).max(end);
    let window_minutes = if end > start {
        (end - start) / MINUTE
    } else {
        0
    };
    let bucket = text_field(balance, "bucket_id")
        .or_else(|| text_field(balance, "entitlement_id"))
        .unwrap_or_else(|| "balance".to_string());
    // The row reads as a gauge, not a ledger: 已用 X/Y keeps text and bar in
    // one point of view; the model name qualifies only when several buckets
    // share the panel, so the common single-bucket account stays compact.
    let model = text_field(balance, "show_name")
        .or_else(|| text_field(balance, "entitlement_id"))
        .unwrap_or_else(|| "模型额度".to_string());
    let lead = if multi_bucket { format!("{model} · ") } else { String::new() };
    Some(QuotaSample {
        used_percent: (used / denominator * 100.0).clamp(0.0, 100.0),
        window_minutes,
        resets_at_ms: reset,
        label: Some(format!(
            "今日额度 · {lead}已用 {}/{}",
            compact_units(used),
            compact_units(denominator)
        )),
        id: Some(format!("start-plan:{bucket}")),
    })
}

fn limit_sample(limit: &Value, plan: Option<&str>) -> Option<QuotaSample> {
    let kind = limit.get("type").and_then(Value::as_str).unwrap_or("");
    let unit = limit.get("unit").and_then(Value::as_i64);
    let number = limit.get("number").and_then(Value::as_i64);
    let tokens = matches!(kind, "TOKENS_LIMIT" | "CREDIT_LIMIT");
    // ZCode's own selector matches on (type family, unit, number), and its
    // locale names what each match is (5 小时 / 每周 / 工具调用). Anything else is
    // a window this build cannot name, and a mislabelled bar is worse than none.
    let (minutes, name, id) = match (tokens, kind, unit, number) {
        (true, _, Some(3), Some(h)) => (h.max(1) * 60, format!("{h} 小时"), "5h".to_string()),
        (true, _, Some(6), _) => (10_080, "每周".to_string(), "weekly".to_string()),
        (false, "TIME_LIMIT", Some(5), Some(m)) if m > 0 => {
            (m * 43_200, "工具调用".to_string(), "tool-call".to_string())
        }
        _ => return None,
    };
    let used = used_percent(limit)?;
    let label = match plan {
        Some(plan) => format!("{name} · {plan}"),
        None => name,
    };
    Some(QuotaSample {
        used_percent: used,
        window_minutes: minutes,
        resets_at_ms: limit
            .get("nextResetTime")
            .and_then(Value::as_i64)
            .filter(|t| *t > 0)
            .map(|t| if t < 100_000_000_000 { t * 1000 } else { t })
            .unwrap_or(0),
        label: Some(label),
        id: Some(id),
    })
}

/// The used share, from whichever pair this record carries.
fn used_percent(limit: &Value) -> Option<f64> {
    if let Some(p) = limit.get("percentage").and_then(Value::as_f64) {
        if p.is_finite() {
            return Some(p.clamp(0.0, 100.0));
        }
    }
    let left = limit.get("remaining").and_then(Value::as_f64)?;
    let used = limit.get("currentValue").and_then(Value::as_f64)?;
    let total = used + left;
    (total > 0.0).then(|| (used / total * 100.0).clamp(0.0, 100.0))
}

// -------------------------------------------------------------- the MCP meter

/// ZCode's two session credentials, as the app itself stores them.
struct Session {
    zcode: String,
    bigmodel: String,
}

/// The live "ZCode MCP" meter: `{data:{level, server_time, next_refresh_at,
/// total_usage:{used, limit, remaining}}}`, measured 200 in 63 ms.
///
/// `window_minutes` is derived from the vendor's own refresh gap rather than
/// guessed: this account's `lite` tier refreshes ~40 h out, which is neither a
/// day nor a month, and a bar labelled with the wrong length is a wrong number.
fn mcp_meter(plan: Option<&str>) -> Option<Vec<QuotaSample>> {
    let session = session()?;
    let body = get_json(
        MCP_USAGE_URL,
        &[
            ("authorization", &format!("Bearer {}", session.zcode)),
            ("x-bigmodel-authorization", session.bigmodel.as_str()),
            ("bigmodel-target-type", "PERSONAL"),
            ("accept", "application/json"),
        ],
    )?;
    let mut samples = mcp_samples(&body);
    if samples.is_empty() {
        return None;
    }
    // The IDE calls this meter "ZCode MCP"; `plan` names the tier when the
    // subscription list already answered, else the meter's own `level` does.
    for s in &mut samples {
        let level = body
            .pointer("/data/level")
            .and_then(Value::as_str)
            .unwrap_or("Coding Plan");
        s.label = Some(format!("ZCode MCP · {}", plan.unwrap_or(level)));
    }
    Some(samples)
}

pub(crate) fn mcp_samples(body: &Value) -> Vec<QuotaSample> {
    mcp_of(body).into_iter().collect()
}

fn mcp_of(body: &Value) -> Option<QuotaSample> {
    let data = body.get("data")?;
    let usage = data.get("total_usage")?;
    let used = usage.get("used").and_then(Value::as_f64)?;
    let limit = usage.get("limit").and_then(Value::as_f64)?;
    if limit <= 0.0 {
        return None;
    }
    let server = data.get("server_time").and_then(Value::as_i64).unwrap_or(0);
    let refresh = data.get("next_refresh_at").and_then(Value::as_i64).unwrap_or(0);
    // Both are epoch seconds on the wire; `nextResetTime` above is milliseconds.
    let (resets_at_ms, window) = if refresh > 0 && server > 0 {
        (refresh * 1000, ((refresh - server) * 1000 / MINUTE).max(1))
    } else {
        (refresh * 1000, 0)
    };
    Some(QuotaSample {
        used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
        window_minutes: window,
        resets_at_ms,
        label: Some("ZCode MCP".to_string()),
        id: Some("mcp".into()),
    })
}

// ------------------------------------------------------------- the credential

fn credential_store() -> Option<Value> {
    let raw = std::fs::read(zcode_home()?.join("v2").join("credentials.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Read and open `~/.zcode/v2/credentials.json`.
///
/// Only the entries the two meters need are ever asked for, and only to send
/// read-only GETs.
///
/// The envelope is `enc:v1:<b64url iv>.<b64url tag>.<b64url ct>`, AES-256-GCM under
/// `sha256(secret)`, where the secret is `ZCODE_CREDENTIAL_SECRET` or — measured on
/// this machine, where the running ZCode process has no `ZCODE_*` variable at all —
/// the app's own fallback string `zcode-credential-fallback:<platform>:<home>:<user>`.
/// That is obfuscation-at-rest rather than a boundary: same user, same machine, and
/// the meters are the ones the IDE already displays.
///
/// What this never does is call `/api/v1/oauth/token`. A refresh grant rotates the
/// refresh token, and a tool that consumes the rotation without writing it back
/// logs the user out of their own account. Both stored tokens are long-lived
/// session JWTs with no `exp` claim (measured), so reading a meter needs no refresh.
fn session() -> Option<Session> {
    let store = credential_store()?;
    let key = envelope_key();
    Some(Session { zcode: opened(&store, "zcodejwttoken", &key)?, bigmodel: opened(&store, "oauth:bigmodel:access_token", &key)? })
}

fn envelope_key() -> [u8; 32] {
    let secret = std::env::var("ZCODE_CREDENTIAL_SECRET")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| credential_fallback_secret(&platform_name(), &home_dir(), &username()));
    sha2::Sha256::digest(secret.as_bytes()).into()
}

/// Must stay byte-for-byte compatible with ZCode's `os.platform()`,
/// `os.homedir()`, and `os.userInfo().username` fallback secret.
fn credential_fallback_secret(platform: &str, home: &Path, user: &str) -> String {
    format!("zcode-credential-fallback:{platform}:{}:{user}", home.display())
}

fn platform_name() -> String {
    #[cfg(target_os = "windows")]
    {
        return "win32".to_string();
    }
    #[cfg(target_os = "macos")]
    {
        return "darwin".to_string();
    }
    #[cfg(target_os = "linux")]
    {
        return "linux".to_string();
    }
    #[allow(unreachable_code)]
    std::env::consts::OS.to_string()
}

fn username() -> String {
    #[cfg(windows)]
    let names = ["USERNAME", "USER"];
    #[cfg(not(windows))]
    let names = ["USER", "USERNAME"];
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.trim().is_empty()))
        .unwrap_or_else(|| "unknown".to_string())
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(home) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return PathBuf::from(home);
        }
    }
    dirs::home_dir().unwrap_or_default()
}

fn zcode_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("ZCODE_HOME") {
        let home = home.to_string_lossy().trim().to_string();
        if !home.is_empty() {
            return Some(PathBuf::from(home));
        }
    }
    Some(home_dir().join(".zcode"))
}

/// One entry of the store, decrypted. Any failure is simply "no credential":
/// a rotated envelope, a changed format, or a different user all end the same way,
/// with the meters that can answer doing so.
fn opened(store: &Value, key_name: &str, key: &[u8; 32]) -> Option<String> {
    let value = store.get(key_name)?.as_str()?;
    let sealed = value.strip_prefix("enc:v1:")?;
    let mut parts = sealed.split('.');
    let (iv, tag, body) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let (iv, tag, mut payload) = (b64url(iv)?, b64url(tag)?, b64url(body)?);
    if iv.len() != 12 || tag.len() != 16 || payload.is_empty() {
        return None;
    }
    // `aead` wants the tag appended to the ciphertext; the envelope keeps them
    // apart, and forgetting this reads as "every credential is unreadable".
    payload.extend_from_slice(&tag);
    let plain = Aes256Gcm::new_from_slice(key)
        .ok()?
        .decrypt(Nonce::from_slice(&iv), payload.as_slice())
        .ok()?;
    String::from_utf8(plain).ok()
}

/// Base64url without padding, which is what the envelope writes.
fn b64url(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bits = 0u32;
    let mut acc = 0u16;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        let v = T.iter().position(|&x| x == c)? as u16;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// --------------------------------------------------- the CLI's per-session budgets

/// One bar per live budget, worst first (a `budget_limited` session is the one a
/// user needs to see).
pub(crate) fn budget_samples(conn: &Connection) -> Vec<QuotaSample> {
    let sql = "SELECT s.session_id, s.objective, s.status, CAST(s.token_budget AS REAL), CAST(s.tokens_used AS REAL) \
               FROM session_target s \
               WHERE s.token_budget IS NOT NULL AND CAST(s.token_budget AS REAL) > 0 \
               ORDER BY (CAST(s.tokens_used AS REAL) / CAST(s.token_budget AS REAL)) DESC";
    let Ok(mut stmt) = conn.prepare(sql) else { return Vec::new() };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
            r.get::<_, f64>(4)?,
        ))
    }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (session_id, objective, status, budget, used) in rows.flatten() {
        if !LIVE.contains(&status.as_str()) {
            continue;
        }
        let subject = objective
            .filter(|o| !o.trim().is_empty())
            .map(|o| o.chars().take(24).collect::<String>())
            .unwrap_or_else(|| "会话".to_string());
        out.push(QuotaSample {
            used_percent: (used / budget * 100.0).clamp(0.0, 100.0),
            window_minutes: 0,
            resets_at_ms: 0,
            label: Some(format!("{subject} · {status} {}/{:.0}k tokens", trim_k(used), budget / 1000.0)),
            id: session_id.map(|s| format!("session:{s}")),
        });
    }
    out
}

/// Tokens are seven digits wide; thousands keep the label inside the panel.
fn trim_k(n: f64) -> String {
    if n.abs() >= 1000.0 {
        format!("{:.0}k", n / 1000.0)
    } else {
        format!("{n:.0}")
    }
}

fn db_path() -> Option<PathBuf> {
    let db = zcode_home()?.join("cli").join("db").join("db.sqlite");
    db.is_file().then_some(db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The response measured live on this machine (2026-09-24, `lite` tier,
    /// `GET https://bigmodel.cn/api/monitor/usage/quota/limit`): a 5-hour
    /// prompt pool that is genuinely in use, and the monthly tool-call
    /// allowance. Note what it does **not** contain: no weekly window (this
    /// plan has none) and no generic monthly token window.
    const LITE: &str = r#"{"code":200,"msg":"操作成功","data":{"limits":[
        {"type":"TIME_LIMIT","unit":5,"number":1,"usage":100,"currentValue":0,"remaining":100,"percentage":0,
         "nextResetTime":1792809918998,
         "usageDetails":[{"modelCode":"search-prime","usage":0},{"modelCode":"web-reader","usage":0},{"modelCode":"zread","usage":0}]},
        {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":21,"nextResetTime":1790251055291}
      ],"level":"lite"},"success":true}"#;

    /// The paid-tier shape: the same 5-hour window **plus** a weekly one, which
    /// is the difference the user sees between plans.
    const PRO: &str = r#"{"code":200,"data":{"level":"pro","limits":[
        {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":62.5,"nextResetTime":1790242838461},
        {"type":"CREDIT_LIMIT","unit":6,"percentage":80,"nextResetTime":1790265600000}
      ]}}"#;

    fn limits_of(raw: &str) -> Value {
        serde_json::from_str::<Value>(raw).expect("fixture parses").pointer("/data/limits").expect("limits").clone()
    }

    fn labels(s: &[QuotaSample]) -> Vec<String> {
        s.iter().map(|x| x.label.clone().unwrap_or_default()).collect()
    }

    #[test]
    fn a_lite_plan_shows_its_five_hour_and_tool_call_windows_and_no_weekly() {
        let s = samples_from_limits(&limits_of(LITE), Some("GLM Coding Lite"));
        let labels = labels(&s);
        assert_eq!(s.len(), 2, "{labels:?}");
        let five = s.iter().find(|x| x.window_minutes == 300).expect("the 5-hour prompt pool");
        assert_eq!(five.id.as_deref(), Some("5h"), "the id a saved ordering keys on");
        assert_eq!(five.used_percent, 21.0, "`percentage` is already the used share");
        assert_eq!(five.resets_at_ms, 1790251055291, "the live answer carries the reset");
        assert!(labels.iter().any(|l| l == "5 小时 · GLM Coding Lite"), "{labels:?}");
        // `TIME_LIMIT` is the monthly tool-call allowance, not a token window —
        // an earlier build drew this very record as "1 月", which it is not.
        let tool = s.iter().find(|x| x.window_minutes == 43_200).expect("the monthly tool-call allowance");
        assert_eq!(tool.id.as_deref(), Some("tool-call"));
        assert_eq!(tool.used_percent, 0.0);
        assert_eq!(tool.resets_at_ms, 1792809918998);
        assert!(labels.iter().any(|l| l == "工具调用 · GLM Coding Lite"), "{labels:?}");
        assert!(!labels.iter().any(|l| l.starts_with("每周")), "{labels:?} — this plan has no weekly window");
        assert!(!labels.iter().any(|l| l.contains("月 ·")), "{labels:?} — no generic monthly token window exists");
    }

    #[test]
    fn without_a_plan_name_the_level_names_the_tier() {
        let s = samples_from_limits(&limits_of(LITE), None);
        let five = s.iter().find(|x| x.window_minutes == 300).expect("the 5-hour window");
        assert_eq!(five.label.as_deref(), Some("5 小时"), "no subscription answer, no invented name");
    }

    #[test]
    fn a_paid_plan_adds_the_weekly_window_and_keeps_the_vendor_percentage() {
        let s = samples_from_limits(&limits_of(PRO), Some("GLM Coding Pro"));
        assert_eq!(s.len(), 2);
        let five = s.iter().find(|x| x.window_minutes == 300).unwrap();
        assert_eq!(five.used_percent, 62.5, "used, not the 37.5 the IDE's remaining-ring would show");
        let week = s.iter().find(|x| x.window_minutes == 10_080).expect("unit 6 is the weekly window");
        assert_eq!(week.used_percent, 80.0, "CREDIT_LIMIT is a token-family window in ZCode's own matcher");
        assert_eq!(week.resets_at_ms, 1790265600000);
    }

    #[test]
    fn an_unknown_window_shape_is_left_alone() {
        // A unit this build cannot name must not be drawn as a guessed window.
        let odd = json!([{"type":"TOKENS_LIMIT","unit":9,"number":2,"percentage":40}]);
        assert!(samples_from_limits(&odd, None).is_empty());
        // Nor a limit with no percentage and no counter pair.
        let bare = json!([{"type":"TOKENS_LIMIT","unit":3,"number":5}]);
        assert!(samples_from_limits(&bare, None).is_empty());
        // Nor an MCP record: that meter belongs to the live ZCode MCP probe, and
        // drawing it here too would be the same bar twice.
        let mcp = json!([{"type":"MCP_USAGE_LIMIT","currentValue":0,"remaining":1000,"percentage":0}]);
        assert!(samples_from_limits(&mcp, None).is_empty());
        assert!(samples_from_limits(&Value::Null, None).is_empty());
    }

    #[test]
    fn current_start_plan_balances_become_model_quota_rows() {
        let body = json!({
            "code": 0,
            "data": {
                "plans": [{
                    "user_plan_id": "upl-1",
                    "plan_id": "zcode-v3-start-plan-0924-wk-2",
                    "name": "ZCode Weekend Build",
                    "status": "active"
                }],
                "balances": [{
                    "bucket_id": "bucket-1",
                    "user_plan_id": "upl-1",
                    "plan_id": "zcode-v3-start-plan-0924-wk-2",
                    "entitlement_id": "ent-1",
                    "show_name": "GLM-5.3-Flash",
                    "total_units": 300000000,
                    "used_units": 23109560,
                    "remaining_units": 276890440,
                    "period_start": 1790390948,
                    "period_end": 1790557200,
                    "expires_at": 1790557200
                }]
            }
        });
        let samples = samples_from_balance(&body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].id.as_deref(), Some("start-plan:bucket-1"));
        assert!((samples[0].used_percent - 7.7031866667).abs() < 0.00001);
        assert_eq!(samples[0].window_minutes, (1790557200 - 1790390948) / 60);
        assert_eq!(samples[0].resets_at_ms, 1790557200000);
        assert_eq!(samples[0].label.as_deref(), Some("今日额度 · 已用 23.11M/300M"));
    }

    #[test]
    fn balance_parser_rejects_expired_or_orphaned_plans() {
        let expired = json!({
            "data": {
                "plans": [{"plan_id": "zcode-start-plan", "name": "ZCode Start Plan", "status": "expired"}],
                "balances": [{"plan_id": "zcode-start-plan", "total_units": 100, "used_units": 1, "remaining_units": 99}]
            }
        });
        assert!(samples_from_balance(&expired).is_empty());

        let orphan = json!({
            "data": {
                "plans": [{"plan_id": "zcode-start-plan", "name": "ZCode Start Plan", "status": "active"}],
                "balances": [{"plan_id": "another-plan", "total_units": 100, "used_units": 1, "remaining_units": 99}]
            }
        });
        assert!(samples_from_balance(&orphan).is_empty());
    }

    #[test]
    fn percentage_wins_over_the_counter_pair_because_usage_is_ambiguous() {
        // `usage` is the allowance on the tool-call record, so it is never
        // consulted; the fallback uses what does agree.
        let no_pct = json!([{"type":"TIME_LIMIT","unit":5,"number":1,"currentValue":150,"remaining":450,"usage":600}]);
        let s = samples_from_limits(&no_pct, None);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].used_percent, 25.0, "150 / (150 + 450)");

        let clamped = json!([{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":140}]);
        assert_eq!(samples_from_limits(&clamped, None)[0].used_percent, 100.0);
    }

    #[test]
    fn a_reset_in_seconds_is_still_understood() {
        // The endpoint speaks milliseconds; a record in seconds must not become
        // a reset in the year 5138.
        let secs = json!([{"type":"TIME_LIMIT","unit":5,"number":1,"percentage":3,"nextResetTime":1792809918}]);
        assert_eq!(samples_from_limits(&secs, None)[0].resets_at_ms, 1792809918000);
    }

    // ------------------------------------------------------- credential choice

    fn sealed_store(entries: &[(&str, &str)], key: &[u8; 32]) -> Value {
        let cipher = Aes256Gcm::new_from_slice(key).unwrap();
        let mut store = serde_json::Map::new();
        for (name, plain) in entries {
            let mut joined = cipher.encrypt(Nonce::from_slice(&[3u8; 12]), plain.as_bytes()).unwrap();
            let tag = joined.split_off(joined.len() - 16);
            store.insert(
                name.to_string(),
                Value::String(format!(
                    "enc:v1:{}.{}.{}",
                    b64url_encode(&[3u8; 12]),
                    b64url_encode(&tag),
                    b64url_encode(&joined)
                )),
            );
        }
        Value::Object(store)
    }

    const TEAM: &str = "account-provider:coding-plan:account:bigmodel-team-coding-plan:account:1:api-key";
    const PERSONAL: &str =
        "account-provider:coding-plan:account:bigmodel-individual-coding-plan:account:1:api-key";
    const ZAI: &str = "account-provider:coding-plan:account:zai-individual-coding-plan:account:1:api-key";

    #[test]
    fn the_personal_key_of_the_active_family_wins_and_picks_its_origin() {
        let key = [7u8; 32];
        let store = sealed_store(
            &[(TEAM, "team-key"), (PERSONAL, "personal-key"), (ZAI, "zai-key")],
            &key,
        );
        let auth = usage_auth_from(&json!({ "oauth:active_provider": "bigmodel" }), &key);
        // The empty store has no sealed entries, so this must have failed…
        assert!(auth.is_none(), "no sealed entry is no credential");

        let mut full = store.as_object().cloned().unwrap();
        full.insert("oauth:active_provider".into(), json!("bigmodel"));
        let auth = usage_auth_from(&Value::Object(full), &key).expect("a credential");
        assert_eq!(auth.authorization, "Bearer personal-key", "personal beats team within the family");
        assert_eq!(auth.base, "https://bigmodel.cn");

        let mut zai_active = store.as_object().cloned().unwrap();
        zai_active.insert("oauth:active_provider".into(), json!("zai"));
        let auth = usage_auth_from(&Value::Object(zai_active), &key).expect("a credential");
        assert_eq!(auth.authorization, "Bearer zai-key", "the active family outranks a personal key of the other one");
        assert_eq!(auth.base, "https://api.z.ai");
    }

    #[test]
    fn without_an_api_key_the_account_token_answers() {
        let key = [7u8; 32];
        let mut store = sealed_store(&[("oauth:bigmodel:access_token", "oauth-tok")], &key)
            .as_object()
            .cloned()
            .unwrap();
        store.insert("oauth:active_provider".into(), json!("bigmodel"));
        let auth = usage_auth_from(&Value::Object(store), &key).expect("the measured fallback");
        assert_eq!(auth.authorization, "Bearer oauth-tok");
        assert_eq!(auth.base, "https://bigmodel.cn");
    }

    #[test]
    fn credential_fallback_matches_zcode_platform_and_identity_contract() {
        let home = Path::new(r"C:\Users\me");
        assert_eq!(
            credential_fallback_secret("win32", home, "sp"),
            r"zcode-credential-fallback:win32:C:\Users\me:sp"
        );
        assert_eq!(
            credential_fallback_secret("darwin", Path::new("/Users/me"), "sp"),
            "zcode-credential-fallback:darwin:/Users/me:sp"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_uses_node_compatible_platform_name() {
        assert_eq!(platform_name(), "win32");
    }

    #[test]
    fn the_url_override_pair_is_honored_like_the_app_honors_it() {
        assert_eq!(
            quota_url_with(false, Some("https://proxy.example/quota".into())),
            "https://proxy.example/quota"
        );
        assert_eq!(
            quota_url_with(true, Some("  ".into())),
            "https://api.z.ai/api/monitor/usage/quota/limit",
            "blank is absent"
        );
        assert_eq!(
            quota_url_with(false, None),
            "https://bigmodel.cn/api/monitor/usage/quota/limit"
        );
    }

    #[test]
    fn the_live_mcp_meter_maps_to_one_bar_with_its_real_window() {
        // Measured response, 2026-09-24, `lite` tier.
        let body: Value = serde_json::from_str(
            r#"{"code":0,"msg":"","data":{"server_time":1790224861,"next_refresh_at":1790265600,"level":"lite","total_usage":{"used":0,"limit":1000,"remaining":1000}},"logid":"x"}"#,
        )
        .unwrap();
        let s = mcp_samples(&body);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id.as_deref(), Some("mcp"));
        assert_eq!(s[0].used_percent, 0.0);
        assert_eq!(s[0].resets_at_ms, 1790265600000, "the wire is in seconds");
        assert_eq!(s[0].window_minutes, (1790265600 - 1790224861) / 60, "the vendor's own gap, not a guessed month");

        let hot: Value =
            serde_json::from_str(r#"{"data":{"level":"pro","total_usage":{"used":750,"limit":1000,"remaining":250}}}"#)
                .unwrap();
        let s = mcp_samples(&hot);
        assert_eq!(s[0].used_percent, 75.0);
        assert_eq!(s[0].window_minutes, 0, "no reset advertised, so no countdown claimed");

        // A zero allowance is not a 0 % bar, and neither is a missing meter —
        // and a body that carries an error code instead of data is not one.
        for raw in [
            r#"{"code":0,"data":{"total_usage":{"used":0,"limit":0,"remaining":0}}}"#,
            r#"{"code":0,"data":{}}"#,
            r#"{"code":3001,"msg":"parameter error"}"#,
            r#"{"code":401,"msg":"令牌已过期或验证不正确"}"#,
        ] {
            let v: Value = serde_json::from_str(raw).unwrap();
            assert!(mcp_samples(&v).is_empty(), "{raw} must answer nothing");
        }
    }

    #[test]
    fn several_buckets_carry_the_plan_name_so_the_rows_stay_distinct() {
        let body = json!({
            "data": {
                "plans": [{"plan_id": "zcode-v3-start-plan", "name": "ZCode Weekend Build", "status": "active"}],
                "balances": [
                    {"bucket_id": "b1", "plan_id": "zcode-v3-start-plan", "show_name": "GLM-5.3-Flash", "total_units": 300000000, "used_units": 30000000},
                    {"bucket_id": "b2", "plan_id": "zcode-v3-start-plan", "show_name": "GLM-5.3", "total_units": 1000000, "used_units": 500000}
                ]
            }
        });
        let s = samples_from_balance(&body);
        let labels: Vec<&str> = s.iter().map(|x| x.label.as_deref().unwrap_or_default()).collect();
        assert!(labels.iter().any(|l| l.contains("GLM-5.3-Flash · ") && l.ends_with("已用 30M/300M")), "{labels:?}");
        assert!(labels.iter().any(|l| l.contains("GLM-5.3 · ") && l.ends_with("已用 500K/1M")), "{labels:?}");
    }

    /// The envelope format, proved against itself: a value encrypted the way
    /// ZCode's writer encrypts it must read back, and anything malformed must not.
    #[test]
    fn the_credential_envelope_round_trips_and_rejects_garbage() {
        let key = [7u8; 32];
        let iv = [3u8; 12];
        let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key).unwrap();
        let secret = b"eyJhbGciOiJIUzI1NiJ9.session.jwt";
        // `aead` hands back ciphertext||tag; the envelope keeps them as separate
        // fields, in the order iv . tag . ciphertext.
        let mut joined = cipher.encrypt(Nonce::from_slice(&iv), secret.as_slice()).unwrap();
        let tag = joined.split_off(joined.len() - 16);
        let sealed = format!("enc:v1:{}.{}.{}", b64url_encode(&iv), b64url_encode(&tag), b64url_encode(&joined));
        let store = serde_json::json!({ "zcodejwttoken": sealed });
        assert_eq!(opened(&store, "zcodejwttoken", &key).as_deref().map(str::as_bytes), Some(secret.as_slice()));

        // Every way a real file can disappoint us.
        let plain = serde_json::json!({ "zcodejwttoken": "not-encrypted" });
        assert_eq!(opened(&plain, "zcodejwttoken", &key), None, "an unsealed value is not ours to read");
        let wrong_key = [8u8; 32];
        assert_eq!(opened(&store, "zcodejwttoken", &wrong_key), None, "a rotated secret fails closed");
        for bad in [
            "enc:v1:aaa.bbb",
            "enc:v1:aaa.bbb.ccc.ddd",
            "enc:v1:!!!.bbb.ccc",
            "enc:v1:c2hvcnQ.bbb.c2hvcnQ=",
        ] {
            let v = serde_json::json!({ "zcodejwttoken": bad });
            assert_eq!(opened(&v, "zcodejwttoken", &key), None, "{bad} must not decode");
        }
    }

    fn b64url_encode(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
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
    }

    fn conn(with_budgets: bool) -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE session_target (session_id TEXT, objective TEXT, status TEXT, token_budget INTEGER, tokens_used INTEGER NOT NULL DEFAULT 0);",
        )
        .unwrap();
        if with_budgets {
            c.execute_batch(
                "INSERT INTO session_target VALUES
                   ('s1','修登录页的刷新竞态','active',2000000,1500000),
                   ('s2','','budget_limited',500000,500000),
                   ('s3','done one','complete',100000,99999),
                   ('s4','no budget','active',NULL,49789353),
                   ('s5','zero budget','paused',0,10),
                   ('s6','over cap','active',100000,400000);",
            )
            .unwrap();
        }
        c
    }

    #[test]
    fn only_live_budgeted_targets_are_bars_and_the_worst_comes_first() {
        let s = budget_samples(&conn(true));
        let labels: Vec<&str> = s.iter().map(|x| x.label.as_deref().unwrap_or_default()).collect();
        assert_eq!(s.len(), 3, "{labels:?}");
        assert!(labels[0].starts_with("over cap"), "{labels:?}");
        assert_eq!(s[0].used_percent, 100.0, "clamped, never 400%");
        assert!(labels.iter().any(|l| l.starts_with("修登录页")), "{labels:?}");
        assert!(labels.iter().any(|l| l.starts_with("会话 · budget_limited")), "empty objective still gets a bar: {labels:?}");
        assert!(labels.iter().all(|l| !l.contains("done one") && !l.contains("no budget") && !l.contains("zero budget")), "{labels:?}");
        let main = s.iter().find(|x| x.label.as_deref().is_some_and(|l| l.starts_with("修登录页"))).expect("the active target");
        assert_eq!(main.used_percent, 75.0);
        assert_eq!(main.id.as_deref(), Some("session:s1"), "session bars key on the session, not the moving token count");
        // Session budgets are not vendor windows at all: no length, no reset.
        assert_eq!(main.window_minutes, 0);
        assert_eq!(main.resets_at_ms, 0);
    }

    #[test]
    fn an_open_ended_objective_is_not_a_quota() {
        let c = conn(false);
        assert!(budget_samples(&c).is_empty(), "the shape this machine really has");
        c.execute_batch("INSERT INTO session_target VALUES ('x','y','active',NULL,49789353);")
            .unwrap();
        assert!(budget_samples(&c).is_empty(), "tokens used without a budget is spend, not a limit");
    }

    #[test]
    fn a_database_without_the_table_answers_nothing() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE other (x INTEGER);").unwrap();
        assert!(budget_samples(&c).is_empty());
    }

    #[test]
    #[ignore = "reads ~/.zcode and calls the vendor's live quota interface"]
    fn the_live_interface_reports_this_account() {
        println!("[zcode] auth = {:?}", usage_auth().map(|a| (a.authorization.len(), a.base)));
        for s in QuotaProbe::fetch(&ZcodeQuota) {
            println!("[zcode] {:>6} min  {:>6.2}%  reset={:>13}  {}", s.window_minutes, s.used_percent, s.resets_at_ms, s.label.unwrap_or_default());
        }
    }

}

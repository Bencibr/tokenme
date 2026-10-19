//! ZCode: the provider quota interface the IDE itself polls, plus the
//! per-session token budgets the CLI keeps in its own database.
//!
//! ## Live, through the app's own interface
//!
//! ZCode's usage panel is described by its own locale as 来自当前供应商额度接口,
//! and that interface is plain HTTPS — no cached LevelDB snapshot in between:
//!
//! * Current ZCode desktop (`3.14.x`/`3.15.x`): `GET https://zcode.z.ai/api/v1/zcode-plan/billing/balance?app_version=...`
//!   → `{code, data: {plans[], balances[]}}`. These are the Start Plan buckets
//!   shown by the current panel and are the primary source below.
//! * `GET {base}/api/monitor/usage/quota/limit` → `{code, data: {level,
//!   limits[]}}`, the windows the panel draws. `base` is `https://bigmodel.cn`
//!   for a BigModel coding plan and `https://api.z.ai` for a Z.ai one
//!   (`ZCODE_BIGMODEL_USAGE_QUOTA_URL` / `BIGMODEL_USAGE_QUOTA_URL` override, the
//!   same names the app itself honors). `open.bigmodel.cn` answers the same
//!   endpoint and is tried when the primary base does not.
//! * `GET https://zcode.z.ai/api/v1/mcp/usage` → the "ZCode MCP" call meter.
//! * `GET {base}/api/biz/subscription/list` → the plan's display name
//!   (`GLM Coding Lite`), so a bar can say which tier it describes.
//!
//! ## Forks are discovered by shape, not by product name
//!
//! ZCode has a public source tree and many rebranded forks. They usually keep
//! the credential envelope and the quota interfaces but rename the data root,
//! the executable, the key file, or the version. Hardcoding any one of those
//! makes the next fork invisible, so this module treats every one of them as a
//! candidate:
//!
//! * **Home**: `ZCODE_CLI_CREDENTIALS_FILE`, `ZCODE_DATA_BASE_DIR`, the
//!   `dataBaseDir` pointer in `~/.zcode/v2/setting.json`, `ZCODE_HOME`, the
//!   default `~/.zcode`, then any one-level directory under the home/config
//!   dirs whose `v2/credentials.json` carries a ZCode-shaped key set. The
//!   content — not the directory name — decides, so a renamed root is found
//!   without a new build.
//! * **Key**: `ZCODE_CREDENTIAL_SECRET`, any `*key*` file beside the
//!   credentials (some forks write a `credential-key`), the legacy fallback
//!   secret, and plaintext values. Every key is tried, because one store can
//!   contain values written by different generations of the client.
//! * **Version**: `ZCODE_APP_VERSION`, `ZCODE_MCE_OUTBOUND_APP_VERSION`, the
//!   `mceOutboundAppVersion` setting, then the built-in default.
//! * **Auth/origin**: Bearer then `x-api-key`; `bigmodel.cn` then
//!   `open.bigmodel.cn` (or `api.z.ai`).
//!
//! The current balance endpoint uses the OAuth-issued `zcodejwttoken`; the
//! legacy quota endpoint uses the account's coding-plan API key, which the app stores in
//! `{home}/v2/credentials.json` under
//! `account-provider:coding-plan:account:<plan>:account:<id>:api-key`; the
//! account's `oauth:<family>:access_token` answers the same endpoint (measured)
//! and is the fallback. Both are read through the same `enc:v1:` envelope, and
//! nothing is ever refreshed — see `session()`.
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

use std::cmp::Reverse;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine as _;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use sha2::Digest as _;
use usage_core::QuotaSample;

use std::sync::atomic::{AtomicBool, Ordering};

use crate::http::get_json;
use crate::QuotaProbe;

pub struct ZcodeQuota;

/// Set while a reachable credential store holds accounts yet not one live
/// meter returned a number — an outage the caller rides on the last known
/// answer (see [`QuotaProbe::outage`]). Measured 2026-10-13: the weekly pool
/// hit 100% and all meters went empty within one poll cycle; banking a 0%
/// (or the local budget alone) over the real answer is the failure this
/// flag exists to prevent.
static OUTAGE: AtomicBool = AtomicBool::new(false);

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
/// Used only when neither the environment nor the client's own settings name a
/// version. Forks usually bump `ZCODE_APP_VERSION` or write
/// `mceOutboundAppVersion`, and both are preferred over this.
const DEFAULT_APP_VERSION: &str = "3.14.3";

/// The same override pair the app honors, in the same order.
const URL_OVERRIDES: [&str; 2] = ["ZCODE_BIGMODEL_USAGE_QUOTA_URL", "BIGMODEL_USAGE_QUOTA_URL"];

/// A paused target still holds its budget; a completed one has closed the books.
const LIVE: [&str; 2] = ["active", "budget_limited"];

const MINUTE: i64 = 60_000;

/// How many bytes of a candidate `credentials.json` are inspected before the
/// file is trusted as a ZCode store. Real files are a few kilobytes; the bound
/// keeps a stray multi-megabyte JSON out of the probe.
const FINGERPRINT_LIMIT: u64 = 1_000_000;

impl QuotaProbe for ZcodeQuota {
    fn tool(&self) -> &'static str {
        "zcode"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let store = credential_store();
        let mut out = Vec::new();
        if let Some(store) = &store {
            let keys = envelope_keys(store);
            // Both meters are live, so both are tried independently: an account
            // whose coding-plan key rotated out from under us still gets its MCP
            // bar, and vice versa.
            let auths = usage_auths_from(&store.value, &keys);
            let plan = plan_name(&auths);
            out.extend(quota_windows(&auths, plan.as_deref()).unwrap_or_default());
            out.extend(zcode_plan_windows(store, &keys).unwrap_or_default());
            out.extend(mcp_meter(&store.value, &keys, plan.as_deref()).unwrap_or_default());
        }
        // A credential store that holds accounts while not one live meter
        // returned a number is an outage, not an empty account. Returning
        // nothing lets the caller hold the last known windows until the
        // vendor answers — including skipping the local budget samples, so
        // an outage never banks a partial answer over the real one.
        let outage = out.is_empty() && store_has_accounts();
        OUTAGE.store(outage, Ordering::Relaxed);
        if outage {
            return Vec::new();
        }
        // A store whose credentials are unreadable can still carry the CLI's
        // per-session budgets, and a fork may keep the db while moving (or
        // rotating) its credential file.
        let home = store.as_ref().map(|s| s.home.clone()).or_else(db_home);
        if let Some(db) = home.as_deref().and_then(db_path_in) {
            if let Ok(conn) = Connection::open_with_flags(
                format!("file:{}?mode=ro", db.display()),
                OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) {
                out.extend(budget_samples(&conn));
            }
        }
        out
    }

    fn outage(&self) -> bool {
        OUTAGE.load(Ordering::Relaxed)
    }
}

/// Whether any reachable credential root holds an account identity at all —
/// the difference between "the meters could not answer" (an outage) and
/// "there is no account here" (the tool is simply not signed in).
fn store_has_accounts() -> bool {
    credential_store().is_some_and(|store| {
        let keys = envelope_keys(&store);
        !usage_auths_from(&store.value, &keys).is_empty()
    })
}

// ------------------------------------------------------- the Start Plan bucket

// ------------------------------------------------- the provider quota interface

/// One ready-to-send credential against the quota interface. `value` is the
/// complete header value (Bearer included) so it can be borrowed directly.
#[derive(Clone, PartialEq, Eq)]
struct UsageAuth {
    value: String,
    style: AuthStyle,
    /// Whether the account is on the Z.ai family; picks the origin below.
    zai: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthStyle {
    Bearer,
    XApiKey,
}

impl UsageAuth {
    fn bearer(token: String, zai: bool) -> Self {
        Self {
            value: format!("Bearer {token}"),
            style: AuthStyle::Bearer,
            zai,
        }
    }

    /// zcode2api measured `x-api-key` on the same account family, and forks
    /// have swapped the two auth styles. It is only tried after Bearer fails.
    fn api_key(token: String, zai: bool) -> Self {
        Self {
            value: token,
            style: AuthStyle::XApiKey,
            zai,
        }
    }
}

fn auth_headers(auth: &UsageAuth) -> Vec<(&'static str, &str)> {
    match auth.style {
        AuthStyle::Bearer => {
            vec![
                ("authorization", auth.value.as_str()),
                ("accept", "application/json"),
            ]
        }
        AuthStyle::XApiKey => {
            vec![
                ("x-api-key", auth.value.as_str()),
                ("accept", "application/json"),
            ]
        }
    }
}

fn push_auth(out: &mut Vec<UsageAuth>, auth: UsageAuth) {
    if !out.iter().any(|seen| seen == &auth) {
        out.push(auth);
    }
}

/// The account's credentials against the quota interface, in the order the app
/// itself would choose them: the active provider's personal coding-plan key,
/// then any personal key, then the account's access token. `store` is the
/// decrypted-view of `credentials.json`; `keys` open its `enc:v1:` envelopes.
fn usage_auths_from(store: &Value, keys: &[[u8; 32]]) -> Vec<UsageAuth> {
    const PREFIX: &str = "account-provider:coding-plan:account:";
    const SUFFIX: &str = ":api-key";
    let Some(object) = store.as_object() else {
        return Vec::new();
    };
    let active_zai = opened(store, "oauth:active_provider", keys)
        .map(|value| provider_name(&value).eq_ignore_ascii_case("zai"))
        .unwrap_or(false);
    let mut scored: Vec<(u8, String, bool)> = Vec::new();
    for key in object.keys() {
        let Some(plan) = key
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_suffix(SUFFIX))
        else {
            continue;
        };
        let Some(value) = opened(store, key, keys) else {
            continue;
        };
        let zai = plan.contains("zai");
        let score = (zai == active_zai) as u8 * 2 + plan.contains("individual") as u8;
        scored.push((score, value, zai));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = Vec::new();
    let mut scored = scored.into_iter();
    // The best account key comes first — it is what the app would use. The
    // account's OAuth token follows before lower-ranked keys, so a stale team
    // key cannot crowd the fallback out of the request budget.
    if let Some((_, token, zai)) = scored.next() {
        push_auth(&mut out, UsageAuth::bearer(token.clone(), zai));
        push_auth(&mut out, UsageAuth::api_key(token, zai));
    }
    // The active family first, then the other one; a store written by a fork
    // that renamed the family still has an access token we can try.
    let families: [&str; 2] = if active_zai {
        ["zai", "bigmodel"]
    } else {
        ["bigmodel", "zai"]
    };
    for family in families {
        if let Some(token) = opened(store, &format!("oauth:{family}:access_token"), keys) {
            push_auth(&mut out, UsageAuth::bearer(token, family == "zai"));
        }
    }
    for key in object.keys() {
        if !key.starts_with("oauth:") || !key.ends_with(":access_token") {
            continue;
        }
        let Some(token) = opened(store, key, keys) else {
            continue;
        };
        push_auth(&mut out, UsageAuth::bearer(token, key.contains(":zai:")));
    }
    for (_, token, zai) in scored {
        push_auth(&mut out, UsageAuth::bearer(token.clone(), zai));
        push_auth(&mut out, UsageAuth::api_key(token, zai));
    }
    out
}

/// `oauth:active_provider` is normally `bigmodel`/`zai`; some versions store it
/// as a JSON string (`"zai"`). Both spell the same family.
fn provider_name(value: &str) -> String {
    let trimmed = value.trim();
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::String(name)) => name,
        _ => trimmed.to_string(),
    }
}

/// Read the current Start Plan balance used by ZCode's usage panel. This is
/// deliberately independent of the legacy limits endpoint: the Start Plan
/// endpoint accepts the OAuth-issued ZCode JWT, while the legacy limits endpoint
/// uses a coding-plan API key.
fn zcode_plan_windows(store: &CredentialStore, keys: &[[u8; 32]]) -> Option<Vec<QuotaSample>> {
    let token = opened(&store.value, "zcodejwttoken", keys)?;
    let authorization = format!("Bearer {token}");
    let device_mid = device_mid(&store.home)
        .or_else(|| candidate_homes().iter().find_map(|home| device_mid(home)));
    // The client version is part of the request identity. A fork may reject a
    // stale default, so the local version is tried first and the defaults only
    // after an answer that carries no `data` at all.
    for version in app_versions() {
        let headers = zcode_headers(&authorization, device_mid.as_deref(), &version);
        let Some(body) = get_json_owned(&zcode_plan_balance_url(&version), &headers) else {
            continue;
        };
        if body.get("data").is_some() {
            return Some(samples_from_balance(&body));
        }
    }
    None
}

fn zcode_plan_balance_url(version: &str) -> String {
    format!("{ZCODE_ORIGIN}{ZCODE_PLAN_BALANCE_PATH}?app_version={version}")
}

/// These are the same source headers ZCode's Node client adds before sending
/// a request to its own origin. `X-Device-Mid` is not a secret, but omitting it
/// makes the balance gateway answer code 3001 (parameter error).
fn zcode_headers(
    authorization: &str,
    device_mid: Option<&str>,
    app_version: &str,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("authorization".to_string(), authorization.to_string()),
        ("accept".to_string(), "application/json".to_string()),
        ("user-agent".to_string(), format!("ZCode/{app_version}")),
        ("http-referer".to_string(), ZCODE_ORIGIN.to_string()),
        ("x-title".to_string(), "Z Code@electron".to_string()),
        ("x-zcode-app-version".to_string(), app_version.to_string()),
        ("x-platform".to_string(), zcode_platform_key().to_string()),
        ("x-client-language".to_string(), "unknown".to_string()),
        ("x-client-timezone".to_string(), "unknown".to_string()),
        ("x-os-category".to_string(), zcode_os_category().to_string()),
    ];
    if let Some(device_mid) = device_mid {
        headers.push(("x-device-mid".to_string(), device_mid.to_string()));
    }
    headers
}

/// `get_json` takes borrowed pairs; the headers above own their `String`s so a
/// derived user agent fits. One conversion keeps the HTTP helper unchanged.
fn get_json_owned(url: &str, headers: &[(String, String)]) -> Option<Value> {
    let refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    get_json(url, &refs)
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

fn device_mid(home: &Path) -> Option<String> {
    let raw = fs::read_to_string(home.join("v2").join("telemetry-state.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("deviceMid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Client versions worth sending to the balance endpoint: the running client's
/// own value first, then the outbound version the client records, then the built-in
/// default. Deduplicated so a fork that sets both does not cost two requests.
fn app_versions() -> Vec<String> {
    let mut out = Vec::new();
    for key in ["ZCODE_APP_VERSION", "ZCODE_MCE_OUTBOUND_APP_VERSION"] {
        if let Some(value) = env_nonempty(key) {
            push_version(&mut out, value);
        }
    }
    for home in candidate_homes() {
        if let Some(value) = setting_app_version(&home) {
            push_version(&mut out, value);
        }
    }
    push_version(&mut out, DEFAULT_APP_VERSION.to_string());
    if out.is_empty() {
        out.push(DEFAULT_APP_VERSION.to_string());
    }
    out
}

fn push_version(out: &mut Vec<String>, value: String) {
    let value = value.trim().to_string();
    if value.is_empty()
        || value.len() > 64
        || !value.chars().all(|c| c.is_ascii_graphic())
        || out.contains(&value)
    {
        return;
    }
    out.push(value);
}

fn setting_app_version(home: &Path) -> Option<String> {
    let raw = fs::read_to_string(home.join("v2").join("setting.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("mceOutboundAppVersion")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The quota windows, straight from the interface the IDE reads. `None` means
/// "this account answered nothing", which is not the same as "no windows".
/// The candidate list is short and ordered: a normal account succeeds on the
/// first request, while a fork with a rotated key/style/base still has a path.
fn quota_windows(auths: &[UsageAuth], plan: Option<&str>) -> Option<Vec<QuotaSample>> {
    for auth in auths.iter().take(6) {
        for url in quota_urls(auth.zai) {
            let Some(body) = get_json(&url, &auth_headers(auth)) else {
                continue;
            };
            // A rejected credential answers HTTP 200 with `{"code":401,…}` and no
            // `data`; the app treats that as "no answer", and so does this.
            let Some(data) = body.get("data") else {
                continue;
            };
            let level = data
                .get("level")
                .and_then(Value::as_str)
                .map(str::to_string);
            return Some(samples_from_limits(
                data.get("limits").unwrap_or(&Value::Null),
                plan.or(level.as_deref()),
            ));
        }
    }
    None
}

fn quota_urls(zai: bool) -> Vec<String> {
    quota_urls_with(
        zai,
        URL_OVERRIDES.iter().find_map(|k| std::env::var(k).ok()),
    )
}

fn quota_urls_with(zai: bool, override_url: Option<String>) -> Vec<String> {
    if let Some(url) = override_url
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return vec![url];
    }
    legacy_bases(zai)
        .into_iter()
        .map(|base| format!("{base}{QUOTA_PATH}"))
        .collect()
}

/// Origins that answer the legacy monitor endpoint. `open.bigmodel.cn` is the
/// same gateway used by zcode-switch; a fork that hardcodes it keeps working.
fn legacy_bases(zai: bool) -> Vec<&'static str> {
    if zai {
        vec!["https://api.z.ai"]
    } else {
        vec!["https://bigmodel.cn", "https://open.bigmodel.cn"]
    }
}

/// The plan this account is on, as the vendor's own subscription list names it.
/// Only a `VALID` entry counts; an expired one is not the plan in use.
fn plan_name(auths: &[UsageAuth]) -> Option<String> {
    // The name is cosmetic; two auth candidates are enough to learn it even
    // when a fork swapped Bearer for `x-api-key`.
    for auth in auths.iter().take(3) {
        for base in legacy_bases(auth.zai) {
            let Some(body) = get_json(&format!("{base}{SUBSCRIPTION_PATH}"), &auth_headers(auth))
            else {
                continue;
            };
            let Some(entries) = body.get("data").and_then(Value::as_array) else {
                continue;
            };
            let Some(chosen) = entries
                .iter()
                .find(|e| e.get("status").and_then(Value::as_str) == Some("VALID"))
                .or_else(|| entries.first())
            else {
                continue;
            };
            if let Some(name) = chosen.get("productName").and_then(Value::as_str) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// The windows the vendor reports, in the vendor's own order and naming.
pub(crate) fn samples_from_limits(limits: &Value, plan: Option<&str>) -> Vec<QuotaSample> {
    limits
        .as_array()
        .map(|arr| arr.iter().filter_map(|l| limit_sample(l, plan)).collect())
        .unwrap_or_default()
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
        plan.get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("active"))
            && is_start_plan(plan)
    });
    let Some(active) = active else {
        return Vec::new();
    };
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
                    let same_user_plan = user_plan_id.as_deref().is_some_and(|id| {
                        text_field(balance, "user_plan_id").as_deref() == Some(id)
                    });
                    let same_plan = plan_id
                        .as_deref()
                        .is_some_and(|id| text_field(balance, "plan_id").as_deref() == Some(id));
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
    ["plan_id", "name"]
        .iter()
        .filter_map(|key| text_field(plan, key))
        .any(|value| {
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
        raw.as_f64()
            .or_else(|| raw.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
    })
}

fn epoch_ms(value: Option<f64>) -> i64 {
    let value = value
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(0.0);
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
    let used = used
        .or_else(|| Some(denominator - remaining?))?
        .clamp(0.0, denominator);
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
    // The row reads as a gauge, not a ledger: X/Y beside the bar says used vs
    // total without a ledger verb; the model name qualifies only when several
    // buckets share the panel, so the common single-bucket account stays compact.
    let model = text_field(balance, "show_name")
        .or_else(|| text_field(balance, "entitlement_id"))
        .unwrap_or_else(|| "模型额度".to_string());
    let lead = if multi_bucket {
        format!("{model} · ")
    } else {
        String::new()
    };
    Some(QuotaSample {
        used_percent: (used / denominator * 100.0).clamp(0.0, 100.0),
        window_minutes,
        resets_at_ms: reset,
        label: Some(format!(
            "Start Plan · {lead}{}/{}",
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
fn mcp_meter(store: &Value, keys: &[[u8; 32]], plan: Option<&str>) -> Option<Vec<QuotaSample>> {
    let session = session(store, keys)?;
    let authorization = format!("Bearer {}", session.zcode);
    let body = get_json(
        MCP_USAGE_URL,
        &[
            ("authorization", authorization.as_str()),
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
    let refresh = data
        .get("next_refresh_at")
        .and_then(Value::as_i64)
        .unwrap_or(0);
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

/// One parsed credential store: the home directory that owns it (for
/// `telemetry-state.json`, `setting.json` and the CLI db) and the JSON map.
struct CredentialStore {
    home: PathBuf,
    path: PathBuf,
    value: Value,
}

/// Read `credentials.json` from the first plausible home. A fork is found by
/// the shape of its store, not by the name of its directory: official clients
/// and rebranded forks all keep `v2/credentials.json` (or point
/// `ZCODE_CLI_CREDENTIALS_FILE` at it), and their envelopes share `enc:v1:`.
fn credential_store() -> Option<CredentialStore> {
    if let Some(path) = explicit_credential_file() {
        if let Some(store) = load_store(path) {
            return Some(store);
        }
    }
    for home in explicit_homes() {
        if let Some(store) = load_store(home.join("v2").join("credentials.json")) {
            return Some(store);
        }
    }
    // No explicit pointer: the default home and any rebranded sibling compete
    // on recency, so a machine with both ZCodium and ZCode reads the one that
    // was actually writing.
    let mut fallback: Vec<PathBuf> = Vec::new();
    if let Some(home) = default_home() {
        push_home(&mut fallback, home);
    }
    for home in scanned_homes() {
        push_home(&mut fallback, home);
    }
    fallback.sort_by_key(|home| Reverse(store_mtime(home)));
    for home in fallback {
        let path = home.join("v2").join("credentials.json");
        // A present-but-empty store must not shadow a valid sibling: the
        // fingerprint decides, and `load_store` rejects a map with no strings.
        if !is_zcode_store(&path) {
            continue;
        }
        if let Some(store) = load_store(path) {
            return Some(store);
        }
    }
    None
}

fn load_store(path: PathBuf) -> Option<CredentialStore> {
    let raw = fs::read(&path).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    // The store is `Record<string, string>`; a logged-out or unrelated JSON is
    // not a credential source. Skipping it lets discovery continue.
    let object = value.as_object()?;
    if !object.values().any(Value::is_string) {
        return None;
    }
    let home = path.parent()?.parent()?.to_path_buf();
    Some(CredentialStore { home, path, value })
}

fn explicit_credential_file() -> Option<PathBuf> {
    env_nonempty("ZCODE_CLI_CREDENTIALS_FILE")
        .map(|raw| expand_path(&raw))
        .filter(|path| path.is_file())
}

/// Pointers the official app and its forks honor. These win over discovery
/// because they are the user's explicit choice.
fn explicit_homes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(base) = env_nonempty("ZCODE_DATA_BASE_DIR") {
        push_home(&mut out, expand_path(&base).join(".zcode"));
    }
    if let Some(base) = setting_data_base_dir() {
        push_home(&mut out, base.join(".zcode"));
    }
    if let Some(home) = env_nonempty("ZCODE_HOME") {
        push_home(&mut out, expand_path(&home));
    }
    out
}

fn default_home() -> Option<PathBuf> {
    Some(home_dir().join(".zcode"))
}

/// Every home worth asking about: explicit pointers, the default, then
/// rebranded roots found by shape.
fn candidate_homes() -> Vec<PathBuf> {
    let mut out = explicit_homes();
    if let Some(home) = default_home() {
        push_home(&mut out, home);
    }
    for home in scanned_homes() {
        push_home(&mut out, home);
    }
    out
}

/// One-level scan under the home and platform config/data dirs. The directory
/// name is a cheap prefilter; [`is_zcode_store`] is the real test, so
/// `.zcodium-exp` and future rebrands need no new list.
fn scanned_homes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for base in scan_bases() {
        for home in family_dirs(&base) {
            push_home(&mut out, home);
        }
    }
    out.sort_by_key(|home| Reverse(store_mtime(home)));
    out
}

fn scan_bases() -> Vec<PathBuf> {
    let mut out = Vec::new();
    push_home(&mut out, home_dir());
    for base in [dirs::config_dir(), dirs::data_dir(), dirs::data_local_dir()]
        .into_iter()
        .flatten()
    {
        push_home(&mut out, base);
    }
    out
}

fn family_dirs(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| {
                    let lower = name.to_ascii_lowercase();
                    lower.contains("zcode") || lower.contains("zcodium")
                })
                .unwrap_or(false)
        })
        .collect()
}

fn is_zcode_store(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if meta.len() > FINGERPRINT_LIMIT {
        return false;
    }
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let text = String::from_utf8_lossy(&bytes);
    text.contains("enc:v1:")
        || text.contains("zcodejwttoken")
        || text.contains("account-provider:coding-plan")
        || text.contains("oauth:")
}

fn store_mtime(home: &Path) -> u64 {
    [
        home.join("v2").join("credentials.json"),
        home.join("cli").join("db").join("db.sqlite-wal"),
        home.join("cli").join("db").join("db.sqlite"),
    ]
    .iter()
    .filter_map(|path| fs::metadata(path).ok()?.modified().ok())
    .filter_map(|time| time.duration_since(UNIX_EPOCH).ok())
    .map(|time| time.as_millis() as u64)
    .max()
    .unwrap_or(0)
}

fn db_home() -> Option<PathBuf> {
    let mut homes = candidate_homes();
    homes.sort_by_key(|home| Reverse(store_mtime(home)));
    homes
        .into_iter()
        .find(|home| home.join("cli").join("db").join("db.sqlite").is_file())
}

fn db_path_in(home: &Path) -> Option<PathBuf> {
    let db = home.join("cli").join("db").join("db.sqlite");
    db.is_file().then_some(db)
}

fn push_home(out: &mut Vec<PathBuf>, home: PathBuf) {
    if !out.contains(&home) {
        out.push(home);
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var_os(key)
        .map(|value| value.to_string_lossy().trim().to_string())
        .filter(|value| !value.is_empty())
}

fn expand_path(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return home_dir().join(rest);
    }
    PathBuf::from(raw)
}

fn setting_data_base_dir() -> Option<PathBuf> {
    let raw = fs::read_to_string(home_dir().join(".zcode").join("v2").join("setting.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let raw = value.get("dataBaseDir")?.as_str()?.trim();
    (!raw.is_empty()).then(|| expand_path(raw))
}

/// Read and open `{home}/v2/credentials.json`.
///
/// Only the entries the meters need are ever asked for, and only to send
/// read-only GETs.
///
/// The envelope is `enc:v1:<b64url iv>.<b64url tag>.<b64url ct>`, AES-256-GCM.
/// The key is `sha256(secret)`, where `secret` is `ZCODE_CREDENTIAL_SECRET`, any
/// `*key*` file beside the store (some forks write `credential-key`), or the app's own
/// fallback string `zcode-credential-fallback:<platform>:<home>:<user>`. Every
/// candidate is tried: a store can mix values written by different generations
/// of the client, and a fork may rotate the key without rewriting everything.
/// Plaintext values are returned as-is, exactly like the official cipher.
///
/// What this never does is call `/api/v1/oauth/token`. A refresh grant rotates the
/// refresh token, and a tool that consumes the rotation without writing it back
/// logs the user out of their own account. Both stored tokens are long-lived
/// session JWTs with no `exp` claim (measured), so reading a meter needs no refresh.
fn session(store: &Value, keys: &[[u8; 32]]) -> Option<Session> {
    Some(Session {
        zcode: opened(store, "zcodejwttoken", keys)?,
        bigmodel: opened(store, "oauth:bigmodel:access_token", keys)?,
    })
}

fn envelope_keys(store: &CredentialStore) -> Vec<[u8; 32]> {
    let mut out = Vec::new();
    if let Ok(secret) = std::env::var("ZCODE_CREDENTIAL_SECRET") {
        let trimmed = secret.trim();
        if !trimmed.is_empty() {
            push_key(&mut out, sha256(&secret));
            if trimmed != secret {
                push_key(&mut out, sha256(trimmed));
            }
        }
    }
    // Any small, key-named file beside (or just above) the credentials. A
    // fork's `credential-key` is the measured case; naming it generically
    // covers the next fork that stores its master key under a different
    // adjective.
    let mut dirs: Vec<&Path> = Vec::new();
    if let Some(dir) = store.path.parent() {
        dirs.push(dir);
    }
    if !dirs.iter().any(|dir| *dir == store.home.as_path()) {
        dirs.push(store.home.as_path());
    }
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !path.is_file() || !name.to_ascii_lowercase().contains("key") {
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if meta.len() > 256 {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let raw = raw.trim();
            // A JSON/settings file is not a key; a base64url secret is.
            if raw.is_empty()
                || raw.len() > 128
                || !raw
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_+/=".contains(c))
            {
                continue;
            }
            push_key(&mut out, sha256(raw));
            // A fork may store the 32-byte AES key itself instead of a master
            // secret to hash. Trying the decoded bytes costs one failed decrypt
            // when the file is a secret.
            if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw) {
                if let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice()) {
                    push_key(&mut out, key);
                }
            }
        }
    }
    push_key(
        &mut out,
        sha256(&credential_fallback_secret(
            &platform_name(),
            &home_dir(),
            &username(),
        )),
    );
    out
}

fn sha256(value: &str) -> [u8; 32] {
    sha2::Sha256::digest(value.as_bytes()).into()
}

fn push_key(out: &mut Vec<[u8; 32]>, key: [u8; 32]) {
    if !out.contains(&key) {
        out.push(key);
    }
}

/// Must stay byte-for-byte compatible with ZCode's `os.platform()`,
/// `os.homedir()`, and `os.userInfo().username` fallback secret.
fn credential_fallback_secret(platform: &str, home: &Path, user: &str) -> String {
    format!(
        "zcode-credential-fallback:{platform}:{}:{user}",
        home.display()
    )
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
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
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

/// One entry of the store, decrypted. `keys` is the candidate chain: different
/// values in one file can have been written under different secrets, so success
/// is per value, not per store. An unsealed value is plaintext, which is exactly
/// what the official cipher returns.
fn opened(store: &Value, key_name: &str, keys: &[[u8; 32]]) -> Option<String> {
    let value = store.get(key_name)?.as_str()?;
    if !value.starts_with("enc:v1:") {
        return Some(value.to_string());
    }
    let sealed = value.strip_prefix("enc:v1:")?;
    let mut parts = sealed.split('.');
    let (iv, tag, body) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let (iv, tag, mut payload) = (b64url(iv)?, b64url(tag)?, b64url(body)?);
    if iv.len() != 12 || tag.len() != 16 {
        return None;
    }
    // `aead` wants the tag appended to the ciphertext; the envelope keeps them
    // apart, and forgetting this reads as "every credential is unreadable".
    payload.extend_from_slice(&tag);
    for key in keys {
        let Ok(cipher) = Aes256Gcm::new_from_slice(key) else {
            continue;
        };
        let Ok(plain) = cipher.decrypt(Nonce::from_slice(&iv), payload.as_slice()) else {
            continue;
        };
        if let Ok(text) = String::from_utf8(plain) {
            return Some(text);
        }
    }
    None
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
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
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
            label: Some(format!(
                "{subject} · {status} {}/{:.0}k tokens",
                trim_k(used),
                budget / 1000.0
            )),
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
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
        serde_json::from_str::<Value>(raw)
            .expect("fixture parses")
            .pointer("/data/limits")
            .expect("limits")
            .clone()
    }

    fn labels(s: &[QuotaSample]) -> Vec<String> {
        s.iter()
            .map(|x| x.label.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn a_lite_plan_shows_its_five_hour_and_tool_call_windows_and_no_weekly() {
        let s = samples_from_limits(&limits_of(LITE), Some("GLM Coding Lite"));
        let labels = labels(&s);
        assert_eq!(s.len(), 2, "{labels:?}");
        let five = s
            .iter()
            .find(|x| x.window_minutes == 300)
            .expect("the 5-hour prompt pool");
        assert_eq!(
            five.id.as_deref(),
            Some("5h"),
            "the id a saved ordering keys on"
        );
        assert_eq!(
            five.used_percent, 21.0,
            "`percentage` is already the used share"
        );
        assert_eq!(
            five.resets_at_ms, 1790251055291,
            "the live answer carries the reset"
        );
        assert!(
            labels.iter().any(|l| l == "5 小时 · GLM Coding Lite"),
            "{labels:?}"
        );
        // `TIME_LIMIT` is the monthly tool-call allowance, not a token window —
        // an earlier build drew this very record as "1 月", which it is not.
        let tool = s
            .iter()
            .find(|x| x.window_minutes == 43_200)
            .expect("the monthly tool-call allowance");
        assert_eq!(tool.id.as_deref(), Some("tool-call"));
        assert_eq!(tool.used_percent, 0.0);
        assert_eq!(tool.resets_at_ms, 1792809918998);
        assert!(
            labels.iter().any(|l| l == "工具调用 · GLM Coding Lite"),
            "{labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l.starts_with("每周")),
            "{labels:?} — this plan has no weekly window"
        );
        assert!(
            !labels.iter().any(|l| l.contains("月 ·")),
            "{labels:?} — no generic monthly token window exists"
        );
    }

    #[test]
    fn without_a_plan_name_the_level_names_the_tier() {
        let s = samples_from_limits(&limits_of(LITE), None);
        let five = s
            .iter()
            .find(|x| x.window_minutes == 300)
            .expect("the 5-hour window");
        assert_eq!(
            five.label.as_deref(),
            Some("5 小时"),
            "no subscription answer, no invented name"
        );
    }

    #[test]
    fn a_paid_plan_adds_the_weekly_window_and_keeps_the_vendor_percentage() {
        let s = samples_from_limits(&limits_of(PRO), Some("GLM Coding Pro"));
        assert_eq!(s.len(), 2);
        let five = s.iter().find(|x| x.window_minutes == 300).unwrap();
        assert_eq!(
            five.used_percent, 62.5,
            "used, not the 37.5 the IDE's remaining-ring would show"
        );
        let week = s
            .iter()
            .find(|x| x.window_minutes == 10_080)
            .expect("unit 6 is the weekly window");
        assert_eq!(
            week.used_percent, 80.0,
            "CREDIT_LIMIT is a token-family window in ZCode's own matcher"
        );
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
        let mcp =
            json!([{"type":"MCP_USAGE_LIMIT","currentValue":0,"remaining":1000,"percentage":0}]);
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
        assert_eq!(
            samples[0].label.as_deref(),
            Some("Start Plan · 23.11M/300M")
        );
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
        assert_eq!(
            samples_from_limits(&secs, None)[0].resets_at_ms,
            1792809918000
        );
    }

    // ------------------------------------------------------- credential choice

    fn sealed_store(entries: &[(&str, &str)], key: &[u8; 32]) -> Value {
        let cipher = Aes256Gcm::new_from_slice(key).unwrap();
        let mut store = serde_json::Map::new();
        for (name, plain) in entries {
            let mut joined = cipher
                .encrypt(Nonce::from_slice(&[3u8; 12]), plain.as_bytes())
                .unwrap();
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

    const TEAM: &str =
        "account-provider:coding-plan:account:bigmodel-team-coding-plan:account:1:api-key";
    const PERSONAL: &str =
        "account-provider:coding-plan:account:bigmodel-individual-coding-plan:account:1:api-key";
    const ZAI: &str =
        "account-provider:coding-plan:account:zai-individual-coding-plan:account:1:api-key";

    #[test]
    fn the_personal_key_of_the_active_family_wins_and_picks_its_origin() {
        let key = [7u8; 32];
        let store = sealed_store(
            &[
                (TEAM, "team-key"),
                (PERSONAL, "personal-key"),
                (ZAI, "zai-key"),
            ],
            &key,
        );
        let empty = usage_auths_from(&json!({ "oauth:active_provider": "bigmodel" }), &[key]);
        // The empty store has no sealed entries, so this must have failed…
        assert!(empty.is_empty(), "no sealed entry is no credential");

        let mut full = store.as_object().cloned().unwrap();
        full.insert("oauth:active_provider".into(), json!("bigmodel"));
        let auths = usage_auths_from(&Value::Object(full), &[key]);
        let auth = auths.first().expect("a credential");
        assert_eq!(
            auth.value, "Bearer personal-key",
            "personal beats team within the family"
        );
        assert!(!auth.zai);
        assert!(
            auths
                .iter()
                .any(|a| a.style == AuthStyle::XApiKey && a.value == "personal-key"),
            "the api-key style is tried after the Bearer one"
        );

        let mut zai_active = store.as_object().cloned().unwrap();
        zai_active.insert("oauth:active_provider".into(), json!("zai"));
        let auths = usage_auths_from(&Value::Object(zai_active), &[key]);
        let auth = auths.first().expect("a credential");
        assert_eq!(
            auth.value, "Bearer zai-key",
            "the active family outranks a personal key of the other one"
        );
        assert!(auth.zai);
    }

    #[test]
    fn without_an_api_key_the_account_token_answers() {
        let key = [7u8; 32];
        let mut store = sealed_store(&[("oauth:bigmodel:access_token", "oauth-tok")], &key)
            .as_object()
            .cloned()
            .unwrap();
        store.insert("oauth:active_provider".into(), json!("bigmodel"));
        let auths = usage_auths_from(&Value::Object(store), &[key]);
        let auth = auths.first().expect("the measured fallback");
        assert_eq!(auth.value, "Bearer oauth-tok");
        assert!(!auth.zai);
    }

    #[test]
    fn credential_fallback_matches_zcode_platform_and_identity_contract() {
        let home = Path::new(r"C:\Users\dev");
        assert_eq!(
            credential_fallback_secret("win32", home, "dev"),
            r"zcode-credential-fallback:win32:C:\Users\dev:dev"
        );
        assert_eq!(
            credential_fallback_secret("darwin", Path::new("/Users/dev"), "dev"),
            "zcode-credential-fallback:darwin:/Users/dev:dev"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_node_compatible_platform_name() {
        assert_eq!(platform_name(), "darwin");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_uses_node_compatible_platform_name() {
        assert_eq!(platform_name(), "linux");
    }

    #[cfg(windows)]
    #[test]
    fn windows_uses_node_compatible_platform_name() {
        assert_eq!(platform_name(), "win32");
    }

    #[test]
    fn the_url_override_pair_is_honored_like_the_app_honors_it() {
        assert_eq!(
            quota_urls_with(false, Some("https://proxy.example/quota".into())),
            vec!["https://proxy.example/quota".to_string()]
        );
        assert_eq!(
            quota_urls_with(true, Some("  ".into())),
            vec!["https://api.z.ai/api/monitor/usage/quota/limit".to_string()],
            "blank is absent"
        );
        assert_eq!(
            quota_urls_with(false, None),
            vec![
                "https://bigmodel.cn/api/monitor/usage/quota/limit".to_string(),
                "https://open.bigmodel.cn/api/monitor/usage/quota/limit".to_string(),
            ]
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
        assert_eq!(
            s[0].window_minutes,
            (1790265600 - 1790224861) / 60,
            "the vendor's own gap, not a guessed month"
        );

        let hot: Value = serde_json::from_str(
            r#"{"data":{"level":"pro","total_usage":{"used":750,"limit":1000,"remaining":250}}}"#,
        )
        .unwrap();
        let s = mcp_samples(&hot);
        assert_eq!(s[0].used_percent, 75.0);
        assert_eq!(
            s[0].window_minutes, 0,
            "no reset advertised, so no countdown claimed"
        );

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
        let labels: Vec<&str> = s
            .iter()
            .map(|x| x.label.as_deref().unwrap_or_default())
            .collect();
        assert!(
            labels
                .iter()
                .any(|l| l.contains("GLM-5.3-Flash · ") && l.ends_with("30M/300M")),
            "{labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|l| l.contains("GLM-5.3 · ") && l.ends_with("500K/1M")),
            "{labels:?}"
        );
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
        let mut joined = cipher
            .encrypt(Nonce::from_slice(&iv), secret.as_slice())
            .unwrap();
        let tag = joined.split_off(joined.len() - 16);
        let sealed = format!(
            "enc:v1:{}.{}.{}",
            b64url_encode(&iv),
            b64url_encode(&tag),
            b64url_encode(&joined)
        );
        let store = serde_json::json!({ "zcodejwttoken": sealed });
        assert_eq!(
            opened(&store, "zcodejwttoken", &[key])
                .as_deref()
                .map(str::as_bytes),
            Some(secret.as_slice())
        );

        // Every way a real file can disappoint us.
        let plain = serde_json::json!({ "zcodejwttoken": "not-encrypted" });
        assert_eq!(
            opened(&plain, "zcodejwttoken", &[key]).as_deref(),
            Some("not-encrypted"),
            "an unsealed value is plaintext, exactly like the official cipher"
        );
        let wrong_key = [8u8; 32];
        assert_eq!(
            opened(&store, "zcodejwttoken", &[wrong_key]),
            None,
            "a rotated secret fails closed"
        );
        for bad in [
            "enc:v1:aaa.bbb",
            "enc:v1:aaa.bbb.ccc.ddd",
            "enc:v1:!!!.bbb.ccc",
            "enc:v1:c2hvcnQ.bbb.c2hvcnQ=",
        ] {
            let v = serde_json::json!({ "zcodejwttoken": bad });
            assert_eq!(
                opened(&v, "zcodejwttoken", &[key]),
                None,
                "{bad} must not decode"
            );
        }
    }

    #[test]
    fn a_key_file_beside_the_store_unlocks_new_envelopes() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".zcode");
        let v2 = home.join("v2");
        fs::create_dir_all(&v2).unwrap();
        fs::write(v2.join("credential-key"), "master-secret\n").unwrap();
        let master = sha256("master-secret");
        let store = sealed_store(&[("zcodejwttoken", "jwt-value")], &master);
        fs::write(
            v2.join("credentials.json"),
            serde_json::to_vec(&store).unwrap(),
        )
        .unwrap();

        let loaded = load_store(v2.join("credentials.json")).expect("the store parses");
        assert_eq!(loaded.home, home);
        let keys = envelope_keys(&loaded);
        assert!(keys.contains(&master), "the key file is a candidate");
        assert_eq!(
            opened(&loaded.value, "zcodejwttoken", &keys).as_deref(),
            Some("jwt-value"),
            "the credential-key decrypts a value written under it"
        );

        // A fork may keep the 32-byte AES key itself, encoded as base64url.
        let raw_key = [5u8; 32];
        fs::write(
            v2.join("raw-key"),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw_key),
        )
        .unwrap();
        assert!(
            envelope_keys(&loaded).contains(&raw_key),
            "a directly stored key is a candidate too"
        );
    }

    #[test]
    fn one_store_can_mix_values_from_different_key_generations() {
        let old = [7u8; 32];
        let new = [9u8; 32];
        let mut map = sealed_store(&[("zcodejwttoken", "new-jwt")], &new)
            .as_object()
            .cloned()
            .unwrap();
        map.extend(
            sealed_store(&[("oauth:bigmodel:access_token", "old-jwt")], &old)
                .as_object()
                .cloned()
                .unwrap(),
        );
        let store = Value::Object(map);
        let keys = [new, old];
        assert_eq!(
            opened(&store, "zcodejwttoken", &keys).as_deref(),
            Some("new-jwt")
        );
        assert_eq!(
            opened(&store, "oauth:bigmodel:access_token", &keys).as_deref(),
            Some("old-jwt"),
            "success is per value, not per store"
        );
    }

    #[test]
    fn a_rebranded_home_is_found_by_its_content_not_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let rebranded = dir.path().join(".zcodium-exp");
        fs::create_dir_all(rebranded.join("v2")).unwrap();
        fs::write(
            rebranded.join("v2").join("credentials.json"),
            r#"{"zcodejwttoken":"plain-token"}"#,
        )
        .unwrap();
        let foreign = dir.path().join(".zcode-lookalike");
        fs::create_dir_all(foreign.join("v2")).unwrap();
        fs::write(
            foreign.join("v2").join("credentials.json"),
            r#"{"hello":"world"}"#,
        )
        .unwrap();

        let homes = family_dirs(dir.path());
        assert!(
            homes.contains(&rebranded),
            "the product token prefilters it"
        );
        assert!(is_zcode_store(
            &rebranded.join("v2").join("credentials.json")
        ));
        assert!(
            !is_zcode_store(&foreign.join("v2").join("credentials.json")),
            "a ZCode-named directory with a foreign JSON is not a store"
        );
    }

    #[test]
    fn the_outbound_version_is_read_from_settings() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".zcode");
        fs::create_dir_all(home.join("v2")).unwrap();
        fs::write(
            home.join("v2").join("setting.json"),
            r#"{"mceOutboundAppVersion":"3.15.1"}"#,
        )
        .unwrap();
        assert_eq!(setting_app_version(&home).as_deref(), Some("3.15.1"));

        let mut versions = Vec::new();
        push_version(&mut versions, " 3.15.1 ".into());
        push_version(&mut versions, "3.15.1".into());
        push_version(&mut versions, "bad version".into());
        assert_eq!(
            versions,
            vec!["3.15.1".to_string()],
            "trimmed, deduped, no control/space"
        );
    }

    #[test]
    fn the_auth_style_switches_the_header_it_sends() {
        let bearer = UsageAuth::bearer("tok".into(), false);
        assert_eq!(auth_headers(&bearer)[0], ("authorization", "Bearer tok"));
        let key = UsageAuth::api_key("tok".into(), false);
        assert_eq!(auth_headers(&key)[0], ("x-api-key", "tok"));
    }

    #[test]
    fn the_active_provider_is_understood_with_or_without_json_quotes() {
        assert_eq!(provider_name("zai"), "zai");
        assert_eq!(provider_name("\"zai\""), "zai");
        assert_eq!(provider_name(" bigmodel "), "bigmodel");
    }

    #[test]
    fn an_empty_store_is_not_a_credential_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2").join("credentials.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{}").unwrap();
        assert!(
            load_store(path).is_none(),
            "an empty store must not shadow a sibling"
        );
    }

    fn b64url_encode(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
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
        let labels: Vec<&str> = s
            .iter()
            .map(|x| x.label.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(s.len(), 3, "{labels:?}");
        assert!(labels[0].starts_with("over cap"), "{labels:?}");
        assert_eq!(s[0].used_percent, 100.0, "clamped, never 400%");
        assert!(
            labels.iter().any(|l| l.starts_with("修登录页")),
            "{labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|l| l.starts_with("会话 · budget_limited")),
            "empty objective still gets a bar: {labels:?}"
        );
        assert!(
            labels.iter().all(|l| !l.contains("done one")
                && !l.contains("no budget")
                && !l.contains("zero budget")),
            "{labels:?}"
        );
        let main = s
            .iter()
            .find(|x| {
                x.label
                    .as_deref()
                    .is_some_and(|l| l.starts_with("修登录页"))
            })
            .expect("the active target");
        assert_eq!(main.used_percent, 75.0);
        assert_eq!(
            main.id.as_deref(),
            Some("session:s1"),
            "session bars key on the session, not the moving token count"
        );
        // Session budgets are not vendor windows at all: no length, no reset.
        assert_eq!(main.window_minutes, 0);
        assert_eq!(main.resets_at_ms, 0);
    }

    #[test]
    fn an_open_ended_objective_is_not_a_quota() {
        let c = conn(false);
        assert!(
            budget_samples(&c).is_empty(),
            "the shape this machine really has"
        );
        c.execute_batch("INSERT INTO session_target VALUES ('x','y','active',NULL,49789353);")
            .unwrap();
        assert!(
            budget_samples(&c).is_empty(),
            "tokens used without a budget is spend, not a limit"
        );
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
        if let Some(store) = credential_store() {
            let keys = envelope_keys(&store);
            let auths = usage_auths_from(&store.value, &keys);
            println!(
                "[zcode] home = {:?} keys = {} auths = {}",
                store.home,
                keys.len(),
                auths.len()
            );
        } else {
            println!("[zcode] no credential store found");
        }
        for s in QuotaProbe::fetch(&ZcodeQuota) {
            println!(
                "[zcode] {:>6} min  {:>6.2}%  reset={:>13}  {}",
                s.window_minutes,
                s.used_percent,
                s.resets_at_ms,
                s.label.unwrap_or_default()
            );
        }
    }
}

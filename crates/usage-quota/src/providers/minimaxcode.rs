//! MiniMax Code quota: the Token Plan windows the subscription is sold on.
//!
//! ## The interface, from the vendor's own client
//!
//! `packages/tui/src/account/matrix-account-client.ts` in
//! `MiniMax-AI/minimax-code` defines it: `GET {origin}/v1/api/openplatform/coding_plan/remains`
//! with the OAuth bearer and no request body. The origin is regional
//! (`OPEN_PLATFORM_ORIGINS` there): `https://platform.minimax.io` for `en`,
//! `https://www.minimaxi.com` for `cn`.
//!
//! The credential is the file the app writes, the single source both this probe
//! and the desktop app read: `<dataDir>/auth/<buildEnv>/<region>/<client>/auth.json`
//! — on this machine `~/.minimax/auth/prod/en/mcode-public/auth.json` — whose
//! `records` map holds an `accessToken` beside a `refreshToken` and the epoch it
//! dies at. The access token lives about an hour; the record with the longest
//! remaining life wins, because a machine that has signed in more than once has
//! more than one.
//!
//! ## The refresh token rotates, and the rotation belongs to the app
//!
//! `POST {accountOrigin}/oauth2/token` (`grant_type=refresh_token`,
//! `client_id=mcode-public`, scope `agent.default`, audience `agent-backend` —
//! the vendor's `@mavis/oauth-core` client) mints a fresh pair. **The rotation is
//! single-use**: verified live on 2026-10-05, where re-using a rotated token
//! answers `400 invalid_grant — this refresh token can no longer be used, start a
//! new authorization`. A rotation kept to ourselves would therefore sign the
//! desktop app out, so this probe writes every rotation back in the vendor's own
//! on-disk format: their `auth.lock` mutex, their `generation + 1` bump, their
//! atomic 0600 writes across `auth.json` and `auth-state.json` (the app re-reads
//! the file on each use and observes it). The exchange runs only when the stored
//! token is within [`REFRESH_MARGIN_MS`] of expiry — a token with hours left is
//! used as it is. When the exchange fails, nothing is written and the stored
//! token is still tried: the server stays the authority on whether it lives.
//!
//! ## Window names follow the product, not a guess
//!
//! The same package renders the status line as `5h` and `Week`
//! (`packages/tui/src/tui/shell/chrome.ts:533-534`), and its `quotaAlertWindow()`
//! returns `undefined` for a window whose `status` is 3 — **an unlimited window is
//! omitted, not drawn at 0 %**, which is what this probe does too.
//!
//! ## Two resource paths, one credential
//!
//! A MiniMax Code account can hold a **Token Plan** (the 5-hour and weekly windows)
//! and **credits** (purchased balance plus daily check-in points) at the same time,
//! and they are different endpoints on different hosts:
//! `GET {platform}/v1/api/openplatform/coding_plan/remains` and
//! `GET {gateway}/minimax-cloud/api/v1/credit/details`. Both answer to a plain
//! `Authorization: Bearer` GET — verified live on this machine on 2026-10-05, where
//! the plan call answers `2062 no active token plan subscription` while the credit
//! call answers a real wallet. The desktop app reaches the credit call through an
//! axios instance that also sends the vendor's `yy` / `x-signature` attribution
//! headers (MD5 of a path+body+timestamp string against two literals in its own
//! source); the server does not require them for these reads, so this probe sends
//! none and never impersonates the first-party client.
//!
//! The daily check-in *panel* (`/minimax-cloud/api/v1/signin/status`) is a 7-day
//! award calendar, not an allowance — it answers `invalid timezone_id` without the
//! client's locale params and has no cap to draw a bar against, so it is not probed.
//!
//! ## What is read, and what is not
//!
//! `remaining_percent` is the *remaining* share, so the panel's `used_percent` is its
//! complement. A third entry, the `video` item, is a call **count**
//! (`current_interval_usage_count` of `…_total_count`) on a model matched by name,
//! and a quota row has no shape for a counter — it is reported nowhere rather than
//! squeezed into a percentage.
//!
//! Team-workspace plans need the `X-Group-Id` header the vendor fills from
//! `/matrix/api/v1/commerce/get_membership_info`, and that call *is* gated by the
//! attribution signature. A personal plan answers without it, so this probe reads
//! the personal account and says nothing about a team one rather than forging a
//! first-party request.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};
use usage_core::QuotaSample;

use crate::http::{get_json, post_form_any_status};
use crate::QuotaProbe;

const PATH: &str = "/v1/api/openplatform/coding_plan/remains";
/// The account's credit wallets — purchased and daily check-in points alike.
const CREDITS_PATH: &str = "/minimax-cloud/api/v1/credit/details";
const BASE_EN: &str = "https://platform.minimax.io";
const BASE_CN: &str = "https://www.minimaxi.com";
/// `PUBLIC_GATEWAY_ORIGINS[…]["prod"]` in `packages/tui/src/runtime/public-gateway.ts`.
const AGENT_EN: &str = "https://agent.minimax.io";
const AGENT_CN: &str = "https://agent.minimaxi.com";
const USER_AGENT: &str = concat!("tokenme/", env!("CARGO_PKG_VERSION"));

/// The vendor's OAuth client values (`@mavis/oauth-core` contracts), sent verbatim
/// on the refresh exchange.
const OAUTH_CLIENT_ID: &str = "mcode-public";
const OAUTH_SCOPE: &str = "agent.default";
const OAUTH_AUDIENCE: &str = "agent-backend";
/// A stored token this close to expiry is exchanged before the probe rides it into
/// a mid-flight 401.
const REFRESH_MARGIN_MS: i64 = 120_000;
/// The vendor's lock protocol numbers: `CrossProcessAuthLock` treats a lock older
/// than 30 s as abandoned; a probe waits briefly, then leaves it for next pass.
const LOCK_STALE: Duration = Duration::from_secs(30);
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// `(payload field prefix, the label the product prints, window minutes, reset field)`.
const WINDOWS: &[(&str, &str, i64, &str)] = &[
    ("current_interval", "5h", 300, "end_time"),
    ("current_weekly", "Week", 10_080, "weekly_end_time"),
];
/// The `status` value the vendor's own client reads as "this window has no limit".
const UNLIMITED: f64 = 3.0;

pub struct MiniMaxCodeQuota;

impl QuotaProbe for MiniMaxCodeQuota {
    fn tool(&self) -> &'static str {
        "minimaxcode"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        live().unwrap_or_default()
    }
}

fn live() -> Option<Vec<QuotaSample>> {
    let mut cred = credential()?;
    if cred.expires_at_ms.saturating_sub(now_ms()) < REFRESH_MARGIN_MS {
        // A failed exchange writes nothing and changes nothing: the stored token
        // below is still tried, because the server is the authority on its life.
        if let Some(fresh) = refresh(&cred) {
            cred.access = fresh;
        }
    }
    // Two resource paths, two hosts, one credential: the plan windows live on the
    // open-platform origin and the credit wallets on the gateway origin. Either can
    // answer alone — a plan-less account with credits (this machine) and a
    // subscribed account without both both render truthfully.
    let mut out = Vec::new();
    if let Some(rows) =
        fetched(&cred.access, &plan_bases(cred.region.as_deref()), PATH, samples_from)
    {
        out.extend(rows);
    }
    if let Some(rows) =
        fetched(&cred.access, &agent_bases(cred.region.as_deref()), CREDITS_PATH, credits_from)
    {
        out.extend(rows);
    }
    (!out.is_empty()).then_some(out)
}

/// Try each origin until one answers with rows. Both endpoints are plain
/// `Authorization: Bearer` GETs — verified live here, including the credit one the
/// desktop app reaches through its signed axios instance: the signature is a
/// first-party attribution tag the server does not require for these reads.
fn fetched(
    token: &str,
    origins: &[String],
    path: &str,
    parse: fn(&Value) -> Vec<QuotaSample>,
) -> Option<Vec<QuotaSample>> {
    let headers = [
        ("authorization", format!("Bearer {token}")),
        ("accept", "application/json".to_string()),
        ("user-agent", USER_AGENT.to_string()),
    ];
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    for base in origins {
        if let Some(body) = get_json(&format!("{base}{path}"), &headers) {
            let samples = parse(&body);
            if !samples.is_empty() {
                return Some(samples);
            }
        }
    }
    None
}

/// The env override pins one origin. Otherwise the credential's own region decides
/// it — the app stores per-region credentials precisely because a token does not
/// work on the other side — and an unknown region tries both, so an account on
/// either side answers instead of reporting nothing.
fn plan_bases(region: Option<&str>) -> Vec<String> {
    if let Ok(pinned) = std::env::var("MINIMAX_CODE_QUOTA_BASE") {
        let single = pinned.trim().trim_end_matches('/');
        if !single.is_empty() {
            return vec![single.to_string()];
        }
    }
    match region {
        Some("cn") => vec![BASE_CN.to_string()],
        Some("en") | Some("global") => vec![BASE_EN.to_string()],
        _ => vec![BASE_EN.to_string(), BASE_CN.to_string()],
    }
}

/// The gateway host's two regions. A pinned base stands in for both hosts, so one
/// fixture server can answer either probe.
fn agent_bases(region: Option<&str>) -> Vec<String> {
    if let Ok(pinned) = std::env::var("MINIMAX_CODE_QUOTA_BASE") {
        let single = pinned.trim().trim_end_matches('/');
        if !single.is_empty() {
            return vec![single.to_string()];
        }
    }
    match region {
        Some("cn") => vec![AGENT_CN.to_string()],
        Some("en") | Some("global") => vec![AGENT_EN.to_string()],
        _ => vec![AGENT_EN.to_string(), AGENT_CN.to_string()],
    }
}

/// One `auth.json` record: the access token, the refresh token that rotates it,
/// when it dies, and where the file lives so a rotation can be written back.
struct Credential {
    access: String,
    refresh: Option<String>,
    expires_at_ms: i64,
    path: Option<PathBuf>,
    build_env: Option<String>,
    region: Option<String>,
}

/// The stored credential with the longest remaining life across every install.
fn credential() -> Option<Credential> {
    if let Ok(token) = std::env::var("MAVIS_ACCESS_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            // An env token has no file to rotate and no region of its own; it is
            // used exactly as the vendor's own tooling accepts it.
            return Some(Credential {
                access: token,
                refresh: None,
                expires_at_ms: i64::MAX,
                path: None,
                build_env: None,
                region: env_region(),
            });
        }
    }
    let mut found: Vec<Credential> = Vec::new();
    for home in data_dirs() {
        for path in auth_files(&home.join("auth")) {
            if let Some(cred) = credential_from_path(&path) {
                found.push(cred);
            }
        }
    }
    // Longest remaining life last, so the pick below takes it.
    found.sort_by_key(|c| c.expires_at_ms);
    found.pop()
}

/// The longest-lived record a file holds. The record *keys* embed a NUL byte, so
/// the values are what gets walked.
fn credential_from_path(path: &Path) -> Option<Credential> {
    let value = read_json(path)?;
    let records = value.get("records").and_then(Value::as_object)?;
    let mut best: Option<Credential> = None;
    for record in records.values() {
        let Some(access) = record
            .get("accessToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            continue;
        };
        let expires = record.get("expiresAtMs").and_then(Value::as_f64).unwrap_or(0.0) as i64;
        let refresh = record
            .get("refreshToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        if best.as_ref().is_none_or(|b| expires >= b.expires_at_ms) {
            best = Some(Credential {
                access: access.to_string(),
                refresh,
                expires_at_ms: expires,
                path: Some(path.to_path_buf()),
                build_env: build_env_of(path),
                region: region_of(path),
            });
        }
    }
    best
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// A signed-in env token still has to say which side of the wall it belongs to.
fn env_region() -> Option<String> {
    region_name(std::env::var("MCODE_REGION").ok().as_deref())
        .or_else(|| region_name(std::env::var("MAVIS_REGION").ok().as_deref()))
}

/// Only the three regions the vendor's own table knows.
fn region_name(value: Option<&str>) -> Option<String> {
    let named = value.map(|v| v.trim().to_lowercase());
    named.filter(|v| matches!(v.as_str(), "cn" | "en" | "global"))
}

/// The vendor's data-dir ladder, same order as the usage adapter's:
/// `MINIMAX_DATA_DIR` → `MAVIS_DATA_DIR` → `~/.minimax[-profile]` and the legacy
/// `~/.mavis[-profile]`, which the migration leaves as a symlink to the new one.
fn data_dirs() -> Vec<PathBuf> {
    for name in ["MINIMAX_DATA_DIR", "MAVIS_DATA_DIR"] {
        if let Ok(value) = std::env::var(name) {
            let single = value.trim();
            if !single.is_empty() {
                let dir = PathBuf::from(single);
                return dir.is_dir().then(|| vec![dir]).unwrap_or_default();
            }
        }
    }
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let profile = std::env::var("MAVIS_PROFILE").unwrap_or_default();
    let profile = profile.trim();
    let named = |base: &str| -> PathBuf {
        if profile.is_empty() {
            home.join(base)
        } else {
            home.join(format!("{base}-{profile}"))
        }
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for base in [".minimax", ".mavis"] {
        let dir = named(base);
        if !dir.is_dir() {
            continue;
        }
        // One directory reached twice through the compat symlink is one install.
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if !out.iter().any(|seen: &PathBuf| seen.canonicalize().unwrap_or_default() == key) {
            out.push(dir);
        }
    }
    out
}

/// `<home>/auth/<buildEnv>/<region>/<client>/auth.json`.
fn auth_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(envs) = children(dir) else { return out };
    for env_dir in envs {
        let Some(regions) = children(&env_dir) else { continue };
        for region in regions {
            let Some(clients) = children(&region) else { continue };
            for client in clients {
                let file = client.join("auth.json");
                if file.is_file() {
                    out.push(file);
                }
            }
        }
    }
    out
}

fn children(dir: &Path) -> Option<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dir).ok()?;
    Some(entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
}

/// `…/auth/prod/en/mcode-public/auth.json` → `Some("en")`.
fn region_of(path: &Path) -> Option<String> {
    let region = path.parent()?.parent()?;
    region_name(region.file_name().and_then(|n| n.to_str()))
}

/// `…/auth/prod/en/mcode-public/auth.json` → `Some("prod")`.
fn build_env_of(path: &Path) -> Option<String> {
    let build_env = path.parent()?.parent()?.parent()?;
    let name = build_env.file_name()?.to_str()?.trim().to_lowercase();
    (!name.is_empty()).then_some(name)
}

/// The `auth` home above `<buildEnv>/<region>/<client>/auth.json` — where the
/// vendor's `auth.lock` mutex lives.
fn auth_home(path: &Path) -> Option<PathBuf> {
    path.parent()?.parent()?.parent()?.parent().map(Path::to_path_buf)
}

/// Exchange the stored refresh token for a fresh access token **and write the
/// rotation back in the vendor's own format**, under their lock file: the token is
/// single-use (see the module docs), so a rotation kept private would sign the
/// desktop app out.
///
/// `None` on any failure — and on every failure path nothing has been written:
/// no refresh token, no file to write back to, an unknown region, a lock that will
/// not come free, a superseded token, or the server's own `invalid_grant`.
fn refresh(cred: &Credential) -> Option<String> {
    let refresh_token = cred.refresh.as_deref()?;
    let path = cred.path.as_deref()?;
    let endpoint =
        token_endpoint(cred.region.as_deref(), cred.build_env.as_deref()?)?;
    let _lock = AuthLock::acquire(&auth_home(path)?.join("auth.lock"))?;
    // Under the lock, re-read: a concurrent refresh (the app's own core) may have
    // replaced the record between the read that started this pass and this moment.
    // A grant minted from a superseded token must never overwrite a newer one.
    let current = credential_from_path(path)?;
    if current.refresh.as_deref() != Some(refresh_token) {
        return None;
    }
    let (status, body) = post_form_any_status(
        &endpoint,
        &[("accept", "application/json")],
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", OAUTH_CLIENT_ID),
            ("scope", OAUTH_SCOPE),
            ("audience", OAUTH_AUDIENCE),
        ],
    )?;
    // A non-2xx carries the vendor's own verdict — `invalid_grant` is "start a new
    // authorization", the case only a human can renew. Nothing is written.
    if !(200..300).contains(&status) {
        return None;
    }
    let grant = grant_from(&body, refresh_token)?;
    persist(path, &grant, refresh_token)?;
    Some(grant.access)
}

/// The account origins the vendor's `@mavis/oauth-core` resolves per region and
/// build environment (`ACCOUNT_ORIGINS`), plus its own `MCODE_OAUTH_TOKEN_ENDPOINT`
/// override — the one hook a local fixture server can stand behind. An unknown
/// region or environment means no exchange is attempted at all.
fn token_endpoint(region: Option<&str>, build_env: &str) -> Option<String> {
    if let Ok(pinned) = std::env::var("MCODE_OAUTH_TOKEN_ENDPOINT") {
        let pinned = pinned.trim();
        if !pinned.is_empty() {
            return Some(pinned.to_string());
        }
    }
    let origin = match (region?, build_env) {
        ("cn", "dev" | "test") => "https://account-test.xaminim.com",
        ("cn", "staging") => "https://account-pre.xaminim.com",
        ("cn", "prod") => "https://account.minimax.cn",
        ("en" | "global", "dev" | "test") => "https://account-overseas-test.xaminim.com",
        ("en" | "global", "staging") => "https://account-overseas-pre.xaminim.com",
        ("en" | "global", "prod") => "https://account.minimax.io",
        _ => return None,
    };
    Some(format!("{origin}/oauth2/token"))
}

/// A token grant, in the contract the vendor's own `parseTokenGrant` enforces:
/// bearer type, a positive `expires_in`, `agent.default` among the scopes, and
/// `refresh_token` falling back to the one that was sent when the answer omits it.
struct Grant {
    access: String,
    refresh: String,
    expires_at_ms: i64,
    scopes: Vec<String>,
    subject: Option<String>,
    account_id: Option<String>,
}

fn grant_from(body: &Value, previous_refresh: &str) -> Option<Grant> {
    let access = string(body, "access_token")?;
    let refresh = string(body, "refresh_token").unwrap_or_else(|| previous_refresh.to_string());
    let token_type = string(body, "token_type")?;
    if !token_type.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let expires_in = body.get("expires_in").and_then(Value::as_f64)?;
    if !(expires_in > 0.0 && expires_in.is_finite()) {
        return None;
    }
    let claims = jwt_claims(&access);
    let scopes = parse_scopes(body.get("scope"))
        .or_else(|| {
            claims.as_ref().and_then(|c| {
                parse_scopes(c.get("scope")).or_else(|| parse_scopes(c.get("scp")))
            })
        })
        .unwrap_or_default();
    if !scopes.iter().any(|s| s == OAUTH_SCOPE) {
        return None;
    }
    Some(Grant {
        access,
        refresh,
        expires_at_ms: now_ms() + (expires_in as i64) * 1000,
        scopes,
        subject: claims.as_ref().and_then(|c| string(c, "sub")),
        account_id: claims.as_ref().and_then(|c| string(c, "account_id")),
    })
}

fn parse_scopes(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::String(s) => Some(s.split_whitespace().map(str::to_string).collect()),
        Value::Array(items) if items.iter().all(Value::is_string) => {
            Some(items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        }
        _ => None,
    }
}

/// The access token's own JWT payload, when it is shaped like one.
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn string(value: &Value, key: &str) -> Option<String> {
    let s = value.get(key)?.as_str()?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// The vendor's own commit sequence, mirrored: state turns `refreshing`, the
/// credential record is rewritten with `generation + 1` (same `loginEpoch` — this
/// is the same login, not a new one), then state commits as `authenticated`. Their
/// `FileStore` re-reads the file on every use, so the app sees exactly what its own
/// refresh would have written. Every write is atomic and 0600.
fn persist(path: &Path, grant: &Grant, expected_refresh: &str) -> Option<()> {
    let mut payload: Value = read_json(path)?;
    let records = payload.get_mut("records")?.as_object_mut()?;
    let record = records
        .values_mut()
        .find(|r| r.get("refreshToken").and_then(Value::as_str) == Some(expected_refresh))?;
    let state_path = path.parent()?.join("auth-state.json");
    let mut state = read_json(&state_path);
    let generation = state.as_ref().map(generation_of).unwrap_or(0).max(generation_of(record)) + 1;
    if let Some(s) = state.as_mut() {
        s["status"] = json!("refreshing");
        write_private(&state_path, s)?;
    }
    record["accessToken"] = json!(grant.access);
    record["refreshToken"] = json!(grant.refresh);
    record["tokenType"] = json!("Bearer");
    record["scopes"] = json!(grant.scopes);
    record["expiresAtMs"] = json!(grant.expires_at_ms);
    record["generation"] = json!(generation);
    if let Some(subject) = &grant.subject {
        record["subject"] = json!(subject);
    }
    if let Some(account_id) = &grant.account_id {
        record["accountId"] = json!(account_id);
    }
    write_private(path, &payload)?;
    if let Some(s) = state.as_mut() {
        s["status"] = json!("authenticated");
        s["generation"] = json!(generation);
        s["expiresAtMs"] = json!(grant.expires_at_ms);
        s["scopes"] = json!(grant.scopes);
        write_private(&state_path, s)?;
    }
    Some(())
}

fn generation_of(value: &Value) -> i64 {
    value.get("generation").and_then(Value::as_i64).unwrap_or(0)
}

/// tmp + fsync + rename at 0600 — the vendor's `atomicWritePrivateFile` — in their
/// `FileStore`/`AuthStateStore` wire shape: 2-space JSON with a trailing newline.
fn write_private(path: &Path, value: &Value) -> Option<()> {
    use std::io::Write;
    let dir = path.parent()?;
    let text = format!("{}\n", serde_json::to_string_pretty(value).ok()?);
    let tmp = dir.join(format!(".tokenme-{}.tmp", std::process::id()));
    let mut file = std::fs::File::create(&tmp).ok()?;
    file.write_all(text.as_bytes()).ok()?;
    file.sync_all().ok()?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).ok()?;
    }
    std::fs::rename(&tmp, path).ok()?;
    Some(())
}

/// The vendor's cross-process mutex, mirrored: `proper-lockfile` locks
/// `<file>.lock` by creating that directory, so this takes the same directory a
/// running app would contend on — a stale one (their 30 s window) is taken over,
/// a held one is waited on briefly and then left for the next pass.
struct AuthLock {
    dir: PathBuf,
}

impl AuthLock {
    fn acquire(lock_file: &Path) -> Option<AuthLock> {
        Self::acquire_with(lock_dir(lock_file), LOCK_STALE, LOCK_TIMEOUT)
    }

    fn acquire_with(dir: PathBuf, stale: Duration, timeout: Duration) -> Option<AuthLock> {
        let deadline = Instant::now() + timeout;
        let mut delay = Duration::from_millis(5);
        loop {
            match std::fs::create_dir(&dir) {
                Ok(()) => return Some(AuthLock { dir }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let age = std::fs::metadata(&dir).and_then(|m| m.modified()).ok().and_then(
                        |t| t.elapsed().ok(),
                    );
                    if age.is_some_and(|age| age > stale) {
                        let _ = std::fs::remove_dir(&dir);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(delay);
                    delay = Duration::from_millis(
                        ((delay.as_millis() as f64) * 1.15) as u64 + 1,
                    )
                    .min(Duration::from_millis(250));
                }
                Err(_) => return None,
            }
        }
    }
}

impl Drop for AuthLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}

fn lock_dir(lock_file: &Path) -> PathBuf {
    let mut name = lock_file.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    lock_file.with_file_name(name)
}

/// `{model_remains:[{current_interval_status, current_interval_remaining_percent,
/// end_time, current_weekly_*, weekly_end_time}], base_resp:{status_code, status_msg}}`.
///
/// A non-zero `base_resp.status_code` is an answer, not a failure: `2062` is
/// "no active token plan subscription", which is what this machine's account says.
/// It produces no rows, same as an account whose payload left a window out.
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    if base_failed(body) {
        return Vec::new();
    }
    let Some(primary) =
        body.get("model_remains").and_then(Value::as_array).and_then(|rows| rows.first())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (prefix, label, minutes, reset_key) in WINDOWS.iter().copied() {
        // Unlimited is not a percentage: the product hides the row, so this does.
        if number(primary, &format!("{prefix}_status")) == Some(UNLIMITED) {
            continue;
        }
        let Some(remaining) = remaining_percent(primary, prefix) else { continue };
        out.push(QuotaSample {
            used_percent: (100.0 - remaining).clamp(0.0, 100.0),
            window_minutes: minutes,
            resets_at_ms: number(primary, reset_key).map(normalise_ms).unwrap_or(0),
            label: Some(label.to_string()),
            id: Some(prefix.to_string()),
        });
    }
    out.sort_by_key(|s| s.window_minutes);
    out
}

/// `{details:[{credit_type, granted_amount:"800.00", remaining_amount:"738.34",
/// consumed_amount:"61.66", granted_at_ms:…, expire_at_ms:…}], total_count, base_resp}`
/// — the real shape read off this machine's account on 2026-10-05.
///
/// Amounts arrive as **strings with two decimals**, so they are read as numbers and
/// summed rather than compared. The report keys quota rows by
/// `(tool, window_minutes, label)`, so two wallets sharing a period would collapse
/// into one row anyway: the wallets are therefore summed here — granted, spent, and
/// the earliest expiry as the reset — which is the one reading that survives that
/// merge. The vendor's per-type names (`Purchased`, `Check-in`) are keyed by a
/// `credit_type` *number* this probe does not decode, so no type name is invented
/// into a label.
pub(crate) fn credits_from(body: &Value) -> Vec<QuotaSample> {
    if base_failed(body) {
        return Vec::new();
    }
    let Some(wallets) = body.get("details").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut granted = 0.0;
    let mut spent = 0.0;
    let mut window = 0i64;
    let mut resets = i64::MAX;
    for wallet in wallets {
        let Some(g) = number(wallet, "granted_amount").filter(|g| *g > 0.0) else { continue };
        // A wallet with no stated period is not a window: folding its money into a
        // percentage while inventing a reset time would be the worse mistake.
        let (Some(opened), Some(ends)) = (stamp(wallet, "granted_at_ms"), stamp(wallet, "expire_at_ms"))
        else {
            continue;
        };
        let used = match (number(wallet, "consumed_amount"), number(wallet, "remaining_amount")) {
            (Some(consumed), _) => consumed,
            (None, Some(left)) => (g - left).max(0.0),
            (None, None) => 0.0,
        }
        .clamp(0.0, g);
        granted += g;
        spent += used;
        window = window.max(((ends - opened) / 60_000).max(1));
        resets = resets.min(ends);
    }
    if granted <= 0.0 {
        return Vec::new();
    }
    vec![QuotaSample {
        used_percent: (spent / granted * 100.0).clamp(0.0, 100.0),
        window_minutes: window,
        resets_at_ms: if resets == i64::MAX { 0 } else { resets },
        label: Some("Credits".to_string()),
        id: Some("credits".to_string()),
    }]
}

/// `base_resp.status_code` is the payload's own verdict: a non-zero code is an
/// answer ("no active token plan subscription"), not a transport failure.
fn base_failed(body: &Value) -> bool {
    body.get("base_resp")
        .and_then(|b| b.get("status_code"))
        .and_then(Value::as_f64)
        .is_some_and(|code| code != 0.0)
}

/// An epoch stamp in milliseconds; anything absent, zero or negative is no stamp.
fn stamp(value: &Value, key: &str) -> Option<i64> {
    number(value, key).filter(|v| *v > 0.0).map(|v| v.floor() as i64)
}

/// `…_remaining_percent` is the reading of record. The older wire instead answered a
/// remaining *count* against a total, which the vendor's own client divides out
/// (`readQuotaWindow`: `legacyRemaining / total * 100` — in that shape
/// `…_usage_count` names the share left, not the share spent).
fn remaining_percent(primary: &Value, prefix: &str) -> Option<f64> {
    if let Some(pct) = number(primary, &format!("{prefix}_remaining_percent")) {
        return (0.0..=100.0).contains(&pct).then_some(pct);
    }
    let total = number(primary, &format!("{prefix}_total_count"))?;
    let left = number(primary, &format!("{prefix}_usage_count"))?;
    if total > 0.0 && (0.0..=total).contains(&left) {
        Some((left / total) * 100.0)
    } else {
        None
    }
}

/// Seconds or milliseconds, decided by magnitude: a reset filed in 1970 would render
/// as "expired" beside a window that still has hours left.
fn normalise_ms(value: f64) -> i64 {
    let n = value.floor() as i64;
    if n.abs() < 100_000_000_000 {
        n * 1000
    } else {
        n
    }
}

/// A number written as a JSON number, or as a numeric string.
fn number(value: &Value, key: &str) -> Option<f64> {
    value
        .get(key)
        .and_then(|raw| {
            raw.as_f64().or_else(|| raw.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        })
        .filter(|n| n.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `set_var` is process-global, so the tests that pin a root or a base take turns.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The wire shape `readQuotaWindow` reads: a 5-hour window a third spent, a
    /// weekly one three quarters spent, and a video item that is a call count.
    const PAYLOAD: &str = r#"{"model_remains":[{
        "current_interval_status":1,"current_interval_remaining_percent":67,"end_time":1791160800,
        "current_weekly_status":1,"current_weekly_remaining_percent":"25.5",
        "weekly_end_time":1791525600000,"model_name":"MiniMax-M3"},
        {"model_name":"MiniMax-Video-01","current_interval_status":1,
         "current_interval_total_count":50,"current_interval_usage_count":12}],
        "base_resp":{"status_code":0,"status_msg":"success"}}"#;

    #[test]
    fn the_two_windows_the_status_line_draws_and_no_more() {
        let samples = samples_from(&serde_json::from_str::<Value>(PAYLOAD).unwrap());
        let rows: Vec<(&str, f64, i64)> = samples
            .iter()
            .map(|s| (s.label.as_deref().unwrap(), s.used_percent, s.window_minutes))
            .collect();
        assert_eq!(rows, vec![("5h", 33.0, 300), ("Week", 74.5, 10_080)]);
        // `remaining_percent` is what is left, so the panel's used share is its
        // complement — reading it straight would show 67 % used at a third spent.
        assert_eq!(samples[0].resets_at_ms, 1_791_160_800_000, "a seconds reset is scaled");
        assert_eq!(samples[1].resets_at_ms, 1_791_525_600_000, "a milliseconds reset is not");
        assert_eq!(samples[0].id.as_deref(), Some("current_interval"));
        assert_eq!(samples.len(), 2, "the video item is a call count, not a percentage");
    }

    #[test]
    fn an_unlimited_window_is_omitted_and_a_legacy_pair_still_reads() {
        let body = json!({"model_remains": [{
            "current_interval_status": 3,
            "current_weekly_status": 1,
            "current_weekly_total_count": 200,
            "current_weekly_usage_count": 150,
        }], "base_resp": {"status_code": 0}});
        let samples = samples_from(&body);
        assert_eq!(samples.len(), 1, "status 3 has no limit to draw");
        assert_eq!(samples[0].label.as_deref(), Some("Week"));
        assert_eq!(samples[0].used_percent, 25.0, "150 of 200 left is a quarter spent");
        assert_eq!(samples[0].resets_at_ms, 0, "no end_time is not a fake reset");
    }

    #[test]
    fn no_plan_and_no_rows_are_answers_not_failures() {
        // Exactly what this machine's account answers today.
        let no_plan = json!({"model_remains": null,
            "base_resp": {"status_code": 2062, "status_msg": "no active token plan subscription"}});
        assert!(samples_from(&no_plan).is_empty());
        assert!(samples_from(&json!({"model_remains": [], "base_resp": {"status_code": 0}})).is_empty());
        assert!(samples_from(&json!({"base_resp": {"status_code": 0}})).is_empty());
        // A percent-scale or out-of-range reading is not a window.
        assert!(samples_from(&json!({"model_remains": [{"current_interval_remaining_percent": 6700}]})).is_empty());
        assert!(samples_from(&json!({"model_remains": [{"current_interval_remaining_percent": -1}]})).is_empty());
        assert!(samples_from(&json!({"model_remains": [{"current_interval_remaining_percent": "??"}]})).is_empty());
        // A total of zero cannot be divided out.
        assert!(samples_from(&json!({"model_remains": [{"current_interval_total_count": 0,
            "current_interval_usage_count": 5}]})).is_empty());
    }

    /// The app's own record key embeds a NUL byte, which is why the credentials are
    /// read by walking `records`' values instead of guessing a file or key name.
    fn auth_json(short: &str, long: &str) -> String {
        format!(
            "{{\"schemaVersion\":1,\"records\":{{\"com.minimax.mcode.oauth.prod.en\\u0000short\":\
             {{\"accessToken\":\"{short}\",\"refreshToken\":\"rt-1\",\"expiresAtMs\":1791160000000}},\
             \"com.minimax.mcode.oauth.prod.en\\u0000long\":\
             {{\"accessToken\":\"{long}\",\"refreshToken\":\"rt-2\",\"expiresAtMs\":1900000000000}}}}}}"
        )
    }

    #[test]
    fn the_credential_is_read_from_the_directory_the_app_writes() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MAVIS_ACCESS_TOKEN");
        let dir = tempfile::tempdir().unwrap();
        let client = dir.path().join("auth").join("prod").join("en").join("mcode-public");
        std::fs::create_dir_all(&client).unwrap();
        std::fs::write(client.join("auth.json"), auth_json("tok-short", "tok-long")).unwrap();
        // A sibling tree with nothing usable in it must not win.
        let dead = dir.path().join("auth").join("prod").join("cn").join("other");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("auth.json"), r#"{"records":{"r":{"accessToken":""}}}"#).unwrap();

        let cred = credential_from_path(&client.join("auth.json")).unwrap();
        assert_eq!(cred.access, "tok-long", "the longest-lived record wins");
        assert_eq!(cred.expires_at_ms, 1_900_000_000_000);
        assert_eq!(cred.refresh.as_deref(), Some("rt-2"), "the record's refresh token is carried");
        assert!(
            credential_from_path(&dead.join("auth.json")).is_none(),
            "an empty token is no token"
        );
        // `auth/<buildEnv>/<region>/<client>` — the region is what picks the origin,
        // the build env what picks the account host.
        assert_eq!(region_of(&client.join("auth.json")).as_deref(), Some("en"));
        assert_eq!(build_env_of(&client.join("auth.json")).as_deref(), Some("prod"));
        assert_eq!(region_of(&dead.join("auth.json")).as_deref(), Some("cn"));

        std::env::set_var("MINIMAX_DATA_DIR", dir.path());
        let cred = credential().expect("a signed-in install has a token");
        assert_eq!(cred.access, "tok-long");
        assert_eq!(cred.region.as_deref(), Some("en"));
        assert_eq!(cred.refresh.as_deref(), Some("rt-2"));
        assert_eq!(cred.path.as_deref(), Some(client.join("auth.json").as_path()));
        std::env::remove_var("MINIMAX_DATA_DIR");
    }

    #[test]
    fn no_signed_in_install_answers_nothing_rather_than_an_error() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MAVIS_ACCESS_TOKEN");
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("MINIMAX_DATA_DIR", dir.path().join("absent"));
        assert!(data_dirs().is_empty());
        assert!(credential().is_none());
        assert!(MiniMaxCodeQuota.fetch().is_empty(), "signed out means no quota to show");
        std::env::remove_var("MINIMAX_DATA_DIR");
    }

    #[test]
    fn the_base_follows_the_credentials_region_unless_pinned() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("MINIMAX_CODE_QUOTA_BASE", "http://127.0.0.1:1/");
        assert_eq!(plan_bases(None), vec!["http://127.0.0.1:1".to_string()]);
        assert_eq!(agent_bases(None), vec!["http://127.0.0.1:1".to_string()]);
        std::env::remove_var("MINIMAX_CODE_QUOTA_BASE");
        assert_eq!(plan_bases(Some("cn")), vec![BASE_CN.to_string()]);
        assert_eq!(plan_bases(Some("en")), vec![BASE_EN.to_string()]);
        assert_eq!(agent_bases(Some("cn")), vec![AGENT_CN.to_string()]);
        assert_eq!(agent_bases(Some("en")), vec![AGENT_EN.to_string()]);
        // An unknown region tries both sides rather than reporting nothing.
        assert_eq!(plan_bases(None), vec![BASE_EN.to_string(), BASE_CN.to_string()]);
        assert_eq!(agent_bases(None), vec![AGENT_EN.to_string(), AGENT_CN.to_string()]);
    }

    /// Verbatim shape from this machine's live answer, with synthetic figures.
    const WALLETS: &str = r#"{"details":[{
        "credit_type":2,"granted_amount":"800.00","remaining_amount":"600.00",
        "consumed_amount":"200.00","granted_at_ms":1791158777221,"expire_at_ms":1793721600000},
        {"credit_type":1,"granted_amount":"200.00","remaining_amount":"150.00",
         "granted_at_ms":1791000000000,"expire_at_ms":1792000000000}],
        "total_count":2,"base_resp":{"status_code":0,"status_msg":"ok"}}"#;

    #[test]
    fn credit_wallets_sum_into_one_credits_row() {
        let samples = credits_from(&serde_json::from_str::<Value>(WALLETS).unwrap());
        assert_eq!(samples.len(), 1, "the report would merge same-window rows anyway");
        let row = &samples[0];
        // 200 spent of 800 granted, plus 50 of 200: 250 of 1000.
        assert_eq!(row.used_percent, 25.0);
        assert_eq!(row.label.as_deref(), Some("Credits"));
        assert_eq!(row.id.as_deref(), Some("credits"));
        // The earliest expiry is the thing that actually disappears first, and the
        // period is the longest wallet's own span (granted→expire, not a month we
        // assumed: 2 562 822 779 ms ≈ 42 713 minutes).
        assert_eq!(row.resets_at_ms, 1_792_000_000_000);
        assert_eq!(row.window_minutes, 42_713);
        // The second wallet has no consumed_amount: it is read as granted − remaining.
    }

    #[test]
    fn a_wallet_without_a_period_or_a_grant_is_not_a_row() {
        // Expired-free but period-free: nothing honest to draw.
        assert!(credits_from(&json!({"details": [{"granted_amount": "10.00", "remaining_amount": "3.00"}]}))
            .is_empty());
        assert!(credits_from(&json!({"details": [{"granted_amount": "0.00", "remaining_amount": "0.00",
            "granted_at_ms": 1, "expire_at_ms": 2}]}))
            .is_empty());
        assert!(credits_from(&json!({"details": []})).is_empty());
        assert!(credits_from(&json!({"base_resp": {"status_code": 1400001001}})).is_empty());
        assert!(credits_from(&json!({})).is_empty());
        // An unparsable amount string is not a zero balance: the row is dropped,
        // so a wrong number never replaces a missing one.
        assert!(credits_from(&json!({"details": [{"granted_amount": "?", "remaining_amount": "5",
            "granted_at_ms": 1_791_158_777_221i64, "expire_at_ms": 1_793_721_600_000i64}]}))
            .is_empty());
    }

    #[test]
    fn spent_never_reads_below_zero_or_above_the_grant() {
        let row = &credits_from(&json!({"details": [{"granted_amount": "100", "consumed_amount": "-5",
            "granted_at_ms": 1_791_158_777_221i64, "expire_at_ms": 1_793_721_600_000i64}]}))[0];
        assert_eq!(row.used_percent, 0.0, "a negative spend is 0 % used, not -5 %");
        let row = &credits_from(&json!({"details": [{"granted_amount": "100", "consumed_amount": "900",
            "granted_at_ms": 1_791_158_777_221i64, "expire_at_ms": 1_793_721_600_000i64}]}))[0];
        assert_eq!(row.used_percent, 100.0, "a spend above the grant still fills the bar");
    }

    // ---- the refresh exchange ----

    /// A login as the app files it: two records, unknown fields, the NUL-byte
    /// record keys, and a state file that has to keep agreeing on the generation.
    /// The record the probe will pick (longest-lived) is the one with `old-refresh`.
    fn signed_in(dir: &Path) -> (PathBuf, PathBuf) {
        let client = dir.join("auth").join("prod").join("en").join("mcode-public");
        std::fs::create_dir_all(&client).unwrap();
        let file = client.join("auth.json");
        std::fs::write(
            &file,
            r#"{"schemaVersion":1,"records":{
            "com.minimax.mcode.oauth.prod.en\u0000first":{"accessToken":"old-access","refreshToken":"old-refresh","tokenType":"Bearer","clientId":"mcode-public","scopes":["agent.default"],"audience":"agent-backend","expiresAtMs":1800000000000,"generation":2,"loginEpoch":"epoch-1","unknownField":"kept"},
            "com.minimax.mcode.oauth.prod.en\u0000other":{"accessToken":"other-access","refreshToken":"other-refresh","expiresAtMs":1000}}}"#,
        )
        .unwrap();
        let state = client.join("auth-state.json");
        std::fs::write(
            &state,
            r#"{"schemaVersion":2,"status":"authenticated","storeKind":"file","clientId":"mcode-public","scopes":["agent.default"],"audience":"agent-backend","buildEnv":"prod","region":"en","generation":2,"expiresAtMs":1800000000000}"#,
        )
        .unwrap();
        (file, state)
    }

    #[test]
    fn the_rotated_pair_is_written_back_in_the_vendors_own_format() {
        let dir = tempfile::tempdir().unwrap();
        let (file, state) = signed_in(dir.path());
        let cred = credential_from_path(&file).unwrap();
        assert_eq!(cred.refresh.as_deref(), Some("old-refresh"), "the longest-lived record is picked");
        let grant = grant_from(
            &json!({"access_token": "new-access", "refresh_token": "new-refresh",
                "token_type": "Bearer", "expires_in": 3600, "scope": "agent.default"}),
            "old-refresh",
        )
        .unwrap();
        persist(&file, &grant, "old-refresh").unwrap();

        let payload: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let first = &payload["records"]["com.minimax.mcode.oauth.prod.en\u{0}first"];
        assert_eq!(first["accessToken"], "new-access");
        assert_eq!(first["refreshToken"], "new-refresh");
        assert_eq!(first["generation"], 3, "max(state, record) + 1");
        assert_eq!(first["loginEpoch"], "epoch-1", "the same login, not a new one");
        assert_eq!(first["unknownField"], "kept", "fields this probe does not know survive");
        assert_eq!(first["clientId"], "mcode-public");
        let other = &payload["records"]["com.minimax.mcode.oauth.prod.en\u{0}other"];
        assert_eq!(other["accessToken"], "other-access", "another record is none of our business");
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(written["status"], "authenticated");
        assert_eq!(written["generation"], 3, "the app compares the two generations");
        assert_eq!(written["expiresAtMs"], first["expiresAtMs"]);
        // Their wire shape: 2-space JSON, trailing newline, no temp file left.
        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(raw.ends_with("}\n"), "{raw}");
        assert!(raw.contains("\n  \"schemaVersion\""), "pretty-printed like FileStore writes it");
        assert!(!std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().contains("tokenme")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "private like the app's own writes");
        }
    }

    #[test]
    fn a_superseded_refresh_token_is_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let (file, _) = signed_in(dir.path());
        let before = std::fs::read_to_string(&file).unwrap();
        let grant = grant_from(
            &json!({"access_token": "new-access", "token_type": "Bearer", "expires_in": 3600,
                "scope": "agent.default"}),
            "old-refresh",
        )
        .unwrap();
        assert_eq!(grant.refresh, "old-refresh", "an omitted refresh_token keeps the sent one");
        assert!(persist(&file, &grant, "a-token-the-file-does-not-hold").is_none());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            before,
            "a superseded rotation changes nothing"
        );
    }

    #[test]
    fn the_grant_contract_is_the_one_the_vendor_parses() {
        let grant = grant_from(
            &json!({"access_token": "a", "refresh_token": "r", "token_type": "bearer",
                "expires_in": 3600, "scope": "agent.default extra"}),
            "old",
        )
        .unwrap();
        assert_eq!(grant.scopes, vec!["agent.default".to_string(), "extra".to_string()]);
        assert!(grant.expires_at_ms > now_ms() + 3_500_000, "expires_in is seconds from now");
        // A scope list, not a string, is still scopes.
        assert!(grant_from(
            &json!({"access_token": "a", "token_type": "Bearer", "expires_in": 1,
                "scope": ["agent.default"]}),
            "old"
        )
        .is_some());
        // The JWT's own claims stand in when the body says nothing — and name the
        // account the way the vendor's `credentialFromGrant` reads them.
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"scope":"agent.default","sub":"user-1","account_id":"acct-9"}"#);
        let jwt = format!("{header}.{claims}.sig");
        let grant =
            grant_from(&json!({"access_token": jwt, "token_type": "Bearer", "expires_in": 60}), "old")
                .unwrap();
        assert_eq!(grant.subject.as_deref(), Some("user-1"));
        assert_eq!(grant.account_id.as_deref(), Some("acct-9"));
        // Everything the vendor refuses is refused here too.
        for body in [
            json!({"access_token": "a", "token_type": "mac", "expires_in": 1, "scope": "agent.default"}),
            json!({"access_token": "a", "token_type": "Bearer", "expires_in": 0, "scope": "agent.default"}),
            json!({"access_token": "a", "token_type": "Bearer", "expires_in": 1, "scope": "other.scope"}),
            json!({"token_type": "Bearer", "expires_in": 1, "scope": "agent.default"}),
        ] {
            assert!(grant_from(&body, "old").is_none(), "{body} is not a grant");
        }
    }

    #[test]
    fn the_account_origin_follows_region_and_build_env() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MCODE_OAUTH_TOKEN_ENDPOINT");
        assert_eq!(
            token_endpoint(Some("en"), "prod").as_deref(),
            Some("https://account.minimax.io/oauth2/token")
        );
        assert_eq!(
            token_endpoint(Some("cn"), "prod").as_deref(),
            Some("https://account.minimax.cn/oauth2/token")
        );
        assert!(token_endpoint(None, "prod").is_none(), "an unknown region must not guess");
        assert!(token_endpoint(Some("en"), "qa").is_none(), "nor an unknown build env");
    }

    #[test]
    fn a_credential_without_a_file_never_refreshes() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("MAVIS_ACCESS_TOKEN", "env-token");
        let cred = credential().unwrap();
        assert!(cred.refresh.is_none() && cred.path.is_none());
        assert!(refresh(&cred).is_none(), "an env token has no store to rotate");
        std::env::remove_var("MAVIS_ACCESS_TOKEN");
    }

    #[test]
    fn the_lock_is_the_vendors_mkdir_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let lock_file = dir.path().join("auth.lock");
        let held = AuthLock::acquire(&lock_file).unwrap();
        assert!(dir.path().join("auth.lock.lock").is_dir(), "proper-lockfile's <file>.lock dir");
        let busy = AuthLock::acquire_with(
            lock_dir(&lock_file),
            Duration::from_secs(30),
            Duration::from_millis(50),
        );
        assert!(busy.is_none(), "a held lock is not taken");
        drop(held);
        assert!(!dir.path().join("auth.lock.lock").is_dir(), "released on drop");
        // A stale directory — older than the vendor's 30 s window — is taken over.
        std::fs::create_dir(dir.path().join("auth.lock.lock")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(AuthLock::acquire_with(
            lock_dir(&lock_file),
            Duration::from_millis(1),
            Duration::from_millis(50)
        )
        .is_some());
    }

    /// A one-shot HTTP server standing in for `account.minimax.io/oauth2/token`,
    /// handing the request it saw back over the channel.
    fn token_server(
        status: &'static str,
        body: String,
    ) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut sock, _)) = listener.accept() {
                let mut raw: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = sock.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let len: usize = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse().ok())
                            })
                            .unwrap_or(0);
                        if raw.len() >= head_end + 4 + len {
                            break;
                        }
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&raw).to_string());
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        (port, rx)
    }

    #[test]
    fn the_refresh_exchange_posts_the_vendors_form_and_writes_the_rotation() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MAVIS_ACCESS_TOKEN");
        let dir = tempfile::tempdir().unwrap();
        let (file, _state) = signed_in(dir.path());
        let (port, rx) = token_server(
            "200 OK",
            r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh","token_type":"Bearer","expires_in":3600,"scope":"agent.default"}"#
                .to_string(),
        );
        std::env::set_var(
            "MCODE_OAUTH_TOKEN_ENDPOINT",
            format!("http://127.0.0.1:{port}/oauth2/token"),
        );
        let cred = credential_from_path(&file).unwrap();
        let access = refresh(&cred).expect("a live refresh token mints a token");
        assert_eq!(access, "fresh-access");
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        std::env::remove_var("MCODE_OAUTH_TOKEN_ENDPOINT");
        assert!(request.starts_with("POST /oauth2/token"), "endpoint: {request}");
        for field in [
            "grant_type=refresh_token",
            "refresh_token=old-refresh",
            "client_id=mcode-public",
            "scope=agent.default",
            "audience=agent-backend",
        ] {
            assert!(request.contains(field), "the vendor's form field {field} is missing: {request}");
        }
        let payload: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let first = &payload["records"]["com.minimax.mcode.oauth.prod.en\u{0}first"];
        assert_eq!(first["accessToken"], "fresh-access");
        assert_eq!(first["refreshToken"], "fresh-refresh");
        assert!(!dir.path().join("auth").join("auth.lock.lock").exists(), "lock released");
    }

    #[test]
    fn an_invalid_grant_writes_nothing() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (file, state) = signed_in(dir.path());
        let before_file = std::fs::read_to_string(&file).unwrap();
        let before_state = std::fs::read_to_string(&state).unwrap();
        let (port, _rx) = token_server(
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"this refresh token can no longer be used, start a new authorization"}"#
                .to_string(),
        );
        std::env::set_var(
            "MCODE_OAUTH_TOKEN_ENDPOINT",
            format!("http://127.0.0.1:{port}/oauth2/token"),
        );
        let cred = credential_from_path(&file).unwrap();
        assert!(refresh(&cred).is_none(), "the server's verdict is final");
        std::env::remove_var("MCODE_OAUTH_TOKEN_ENDPOINT");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before_file, "nothing is written");
        assert_eq!(std::fs::read_to_string(&state).unwrap(), before_state);
    }

    // ---- live canary: never runs in the normal gates ----

    /// Rotates the **real** on-disk credential against the real vendor endpoint and
    /// writes the rotation back — the whole production chain, on demand. Ignored by
    /// default and doubly gated on `MCODE_LIVE_ROTATE=1`. It consumes the single-use
    /// refresh token on purpose; the write-back is what keeps the desktop app signed
    /// in, so a failure here means the chain is dead and one browser re-login is due.
    ///
    /// ```sh
    /// MCODE_LIVE_ROTATE=1 cargo test -p usage-quota --lib \
    ///     live_rotation_against_the_real_endpoint -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "rotates the real single-use refresh token; needs MCODE_LIVE_ROTATE=1"]
    fn live_rotation_against_the_real_endpoint() {
        if std::env::var("MCODE_LIVE_ROTATE").as_deref() != Ok("1") {
            eprintln!("skipped: set MCODE_LIVE_ROTATE=1 to rotate the real credential");
            return;
        }
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MAVIS_ACCESS_TOKEN");
        assert!(
            std::env::var("MCODE_OAUTH_TOKEN_ENDPOINT").is_err(),
            "a pinned token endpoint would rotate against a fixture and write the real store"
        );
        let cred = credential().expect("a signed-in install on this machine");
        let path = cred.path.clone().expect("a file-backed credential");
        let state_path = path.parent().unwrap().join("auth-state.json");
        let before = read_json(&path).unwrap();
        let before_state = read_json(&state_path).unwrap_or(Value::Null);
        let before_rec = before["records"]
            .as_object()
            .unwrap()
            .values()
            .find(|r| r.get("refreshToken").and_then(Value::as_str) == cred.refresh.as_deref())
            .expect("the credential's record is on disk");
        let before_gen = generation_of(before_rec).max(generation_of(&before_state));
        eprintln!(
            "live rotation: gen {before_gen}, access expires in {} min, region {:?}, env {:?}",
            cred.expires_at_ms.saturating_sub(now_ms()) / 60_000,
            cred.region,
            cred.build_env
        );
        let access = refresh(&cred).expect(
            "the real exchange must succeed; None here means the on-disk chain is dead \
             and the desktop app needs one browser re-login",
        );
        let after = read_json(&path).unwrap();
        let state = read_json(&state_path).unwrap();
        let record = after["records"]
            .as_object()
            .unwrap()
            .values()
            .find(|r| r.get("accessToken").and_then(Value::as_str) == Some(access.as_str()))
            .expect("the minted access token is on disk");
        assert_eq!(generation_of(record), before_gen + 1, "the record generation is bumped by one");
        assert_eq!(
            state["generation"].as_i64(),
            Some(before_gen + 1),
            "the state agrees on the generation"
        );
        assert_eq!(state["status"].as_str(), Some("authenticated"));
        assert!(
            record["expiresAtMs"].as_i64().unwrap_or(0) > now_ms(),
            "the persisted pair outlives the moment it was written"
        );
        assert!(
            record.get("refreshToken").and_then(Value::as_str).is_some(),
            "the rotated refresh token is persisted — the chain survives"
        );
        eprintln!("live rotation: gen {before_gen} -> {}, both files agree, state authenticated", before_gen + 1);
    }
}

//! Qoder: the account's own usage interface, with the IDE's encrypted snapshot
//! as the offline fallback.
//!
//! Qoder meters in **credits**, never tokens: `~/.qoder/projects/**/*.jsonl` is
//! Claude-shaped but every `*_tokens` field is 0 on this machine (verified
//! 6,722 usage records), and the only number that moves is `usage.credits`.
//!
//! ## Live, through the account's own interface
//!
//! The panel the IDE opens ("我的用量") polls
//! `GET https://openapi.qoder.sh/sash/api/v2/me/usage` (`account.getQuotaUsage`,
//! measured 200 in 6 ms) with `Authorization: Bearer <token>`,
//! `Cosy-ClientType: 10`, `User-Agent: Qoder`. It answers
//! `{"displayMode":"qoder","qoderUsage":{"userType","expiresAt",
//! "userQuota","addOnQuota","dedicatedResourcePackages":[…],
//! "orgResourcePackage":{…}}}`, and the panel's own zh locale names those
//! meters: `userQuota` 套餐内 Credits, `addOnQuota` 资源包, dedicated
//! 个人专属资源包, `orgResourcePackage` 共享资源包. A cache of that panel —
//! `secret://aicoding.auth.creditUsage` — only ever holds `userQuota`, so an
//! account with a resource pack reads as "no quota at all" from the snapshot:
//! this is why the live call comes first.
//!
//! The bearer is the account's own login record. The older VS Code-fork IDE
//! keeps it under `secret://aicoding.auth.userInfo` in its `state.vscdb`; the
//! 0.4 desktop app (which replaced it — this machine's `Qoder.app` writes
//! `com.qoder.app.stable`) keeps it in `auth.v1.dat` beside its Chromium
//! profile. Both records are Electron `safeStorage` envelopes: `v10` +
//! AES-128-CBC under a keychain passphrase on macOS (`Qoder Safe Storage` /
//! `Qoder Key` for the IDE, `Qoder App Safe Storage` / `Qoder App Key` for the
//! app), AES-256-GCM under a DPAPI key in `Local State` on Windows. Expiry
//! gates the call: the old record names it `expireTime` (epoch milliseconds,
//! stored as a JSON string), the 0.4 record `expiresAt` (RFC 3339), and
//! **nothing here ever refreshes**: a refresh grant may rotate `refreshToken`,
//! and a tool that consumes the rotation without writing it back logs the user
//! out of their own client. An expired token is simply "no live sample".
//!
//! Reading the envelope costs one `security` subprocess per cache miss (TTL in
//! [`crate::TTL`]); the keychain may ask the user for permission the first time
//! — that prompt is the OS's, not ours, and a denial is just "no sample".

use std::path::Path;

use serde_json::Value;
use usage_core::QuotaSample;

use crate::http::get_json;
use crate::QuotaProbe;

pub struct QoderQuota;

/// The account's login record: `token`, `refreshToken`, `expireTime`.
const USER_KEY: &str = "secret://aicoding.auth.userInfo";
/// The IDE's cached panel state — plan credits only, no resource packs.
const QUOTA_KEY: &str = "secret://aicoding.auth.creditUsage";
/// The request the IDE's own usage panel issues.
const USAGE_URL: &str = "https://openapi.qoder.sh/sash/api/v2/me/usage";
/// Chromium's `expiresAt` sentinel for "this plan does not expire" (year 9999).
const NO_EXPIRY_MS: i64 = 253_402_214_400_000;

/// Both editions keep their global state under their own support directory.
const SUPPORT_DIRS: [&str; 3] = ["Qoder", "Qoder CN", "QoderCN"];
/// The 0.4 desktop app's login record, beside its Chromium profile.
const AUTH_FILE: &str = "auth.v1.dat";
/// `(service, account)` as the Electron keychain entries: the desktop app's
/// own pair first (it is the install that is actually being used), then the
/// older VS Code-fork IDE's. A passphrase only opens the records of its own
/// generation, so trying both is the discovery.
const KEYCHAIN_IDS: [(&str, &str); 2] = [
    ("Qoder App Safe Storage", "Qoder App Key"),
    ("Qoder Safe Storage", "Qoder Key"),
];

/// The login record behind one install's `auth.v1.dat` — platform keepers
/// first (the Windows DPAPI path reads through its own helper), the macOS
/// keychain pair second, other platforms unanswered.
fn discover_login_info(dir: &std::path::Path) -> Option<Value> {
    #[cfg(windows)]
    {
        read_windows_user_info(dir)
    }
    #[cfg(target_os = "macos")]
    {
        KEYCHAIN_IDS
            .iter()
            .find_map(|(service, account)| keychain_pass(service, account).and_then(|p| app_user_info(dir, &p)))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = dir;
        None
    }
}

impl QuotaProbe for QoderQuota {
    fn tool(&self) -> &'static str {
        "qoder"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        // The desktop app's own Chromium profile (`com.qoder.app.*`) is the
        // live install; its `auth.v1.dat` is read through each platform's
        // safeStorage keeper.
        for dir in app_dirs() {
            if !dir.join(AUTH_FILE).is_file() {
                continue;
            }
            #[cfg(windows)]
            let info = read_windows_user_info(&dir);
            #[cfg(target_os = "macos")]
            let info = KEYCHAIN_IDS
                .iter()
                .find_map(|(service, account)| keychain_pass(service, account).and_then(|p| app_user_info(&dir, &p)));
            #[cfg(not(any(windows, target_os = "macos")))]
            let info: Option<Value> = None;
            if let Some(info) = info {
                let checkin = qoder_daily_checkin(&info, false);
                if let Some(samples) = live_usage(&info).filter(|s| !s.is_empty()) {
                    return with_checkin_mark(samples, &checkin);
                }
            }
        }

        for db in state_dbs() {
            let conn = match open(&db) {
                Some(c) => c,
                None => continue,
            };
            for (service, account) in KEYCHAIN_IDS {
                let Some(pass) = keychain_pass(service, account) else { continue };
                // The same passphrase opens both records, so one connection and
                // one subprocess answer the live meter and its fallback.
                if let Some(info) = read_secret(&conn, USER_KEY).and_then(|b| decrypt(&pass, &b)).and_then(|p| parse(&p))
                {
                    let checkin = qoder_daily_checkin(&info, false);
                    if let Some(samples) = live_usage(&info).filter(|s| !s.is_empty()) {
                        return with_checkin_mark(samples, &checkin);
                    }
                }
                if let Some(value) =
                    read_secret(&conn, QUOTA_KEY).and_then(|b| decrypt(&pass, &b)).and_then(|p| parse(&p))
                {
                    let samples = samples_from_snapshot(&value);
                    if !samples.is_empty() {
                        return samples;
                    }
                }
            }
        }
        Vec::new()
    }
}

/// The desktop app's Chromium profile directory, which it named after its
/// bundle id (`com.qoder.app.stable`) rather than the product. Kept tolerant
/// of beta/dev channels, and of an explicit `APPDATA` fallback on Windows,
/// because `dirs` can be unavailable in a restricted service environment.
fn app_dirs() -> Vec<std::path::PathBuf> {
    let mut bases = Vec::new();
    if let Some(base) = dirs::data_dir() {
        bases.push(base);
    }
    #[cfg(windows)]
    {
        if let Some(base) = std::env::var_os("APPDATA").map(std::path::PathBuf::from) {
            if !bases.iter().any(|p| p == &base) {
                bases.push(base);
            }
        }
    }

    let mut out = Vec::new();
    for base in bases {
        for name in [
            "com.qoder.app.stable",
            "com.qoder.app.beta",
            "com.qoder.app.dev",
        ] {
            let candidate = base.join(name);
            if !out.iter().any(|p| p == &candidate) {
                out.push(candidate);
            }
        }
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_channel = entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("com.qoder.app.");
                if is_channel && !out.iter().any(|p| p == &path) {
                    out.push(path);
                }
            }
        }
    }
    out
}

/// Read a Windows Qoder login record without spawning PowerShell or a helper
/// process. DPAPI is deliberately called in-process so the result is tied to
/// the same Windows user that owns Qoder's profile.
#[cfg(windows)]
fn read_windows_user_info(dir: &Path) -> Option<Value> {
    use base64::Engine;

    let local_state: Value =
        serde_json::from_slice(&std::fs::read(dir.join("Local State")).ok()?).ok()?;
    let encrypted_key = local_state.pointer("/os_crypt/encrypted_key")?.as_str()?;
    let wrapped = base64::engine::general_purpose::STANDARD
        .decode(encrypted_key)
        .ok()?;
    let wrapped = wrapped.strip_prefix(b"DPAPI")?;
    let key = dpapi_unprotect(wrapped)?;
    if key.len() != 32 {
        return None;
    }

    let encrypted_auth = std::fs::read(dir.join(AUTH_FILE)).ok()?;
    let plaintext = decrypt_windows_v10(&key, &encrypted_auth)?;
    let info: Value = serde_json::from_slice(&plaintext).ok()?;
    info.get("token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())?;
    Some(info)
}

/// The desktop app's record under its own keychain passphrase: the same
/// Chromium `v10` envelope, read-only like everything else here.
#[cfg(target_os = "macos")]
fn app_user_info(dir: &Path, pass: &str) -> Option<Value> {
    let blob = std::fs::read(dir.join(AUTH_FILE)).ok()?;
    decrypt(pass, &blob).and_then(|p| parse(&p))
}

/// Chromium's Windows `safeStorage` key is a DPAPI blob prefixed by ASCII
/// `DPAPI`. `CryptUnprotectData` allocates the output with LocalAlloc; copy it
/// before freeing it so no Windows-owned pointer escapes this function.
#[cfg(windows)]
fn dpapi_unprotect(data: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    let cb_data = u32::try_from(data.len()).ok()?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: cb_data,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        )
    };
    if ok == 0 || output.pbData.is_null() {
        return None;
    }

    let plaintext =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Some(plaintext)
}

/// Current Chromium/Electron Windows envelopes are `v10 || nonce(12) ||
/// AES-256-GCM(ciphertext || tag)`, with empty additional authenticated data.
#[cfg(windows)]
fn decrypt_windows_v10(key: &[u8], blob: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::{
        aead::{Aead, KeyInit},
        Aes256Gcm, Nonce,
    };

    let payload = blob.strip_prefix(b"v10")?;
    if payload.len() <= 12 {
        return None;
    }
    let (nonce, ciphertext) = payload.split_at(12);
    if ciphertext.len() < 16 {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    cipher.decrypt(Nonce::from_slice(nonce), ciphertext).ok()
}

fn state_dbs() -> Vec<std::path::PathBuf> {
    let Some(base) = dirs::data_dir() else { return Vec::new() };
    SUPPORT_DIRS
        .iter()
        .map(|d| base.join(d).join("User").join("globalStorage").join("state.vscdb"))
        .filter(|p| p.is_file())
        .collect()
}

fn open(path: &Path) -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

/// The `Buffer` JSON Electron stores for a `secret://` key. Read-only: the IDE
/// writes this file while it runs, so a locked db is a miss, not an error.
fn read_secret(conn: &rusqlite::Connection, key: &str) -> Option<Vec<u8>> {
    let raw: String = conn
        .query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |r| r.get(0))
        .ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let bytes: Vec<u8> = value.get("data")?.as_array()?.iter().map(|n| n.as_u64().unwrap_or(0) as u8).collect();
    Some(bytes)
}

#[cfg(target_os = "macos")]
fn keychain_pass(service: &str, account: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pass = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    (!pass.is_empty()).then_some(pass)
}

// Qoder stores this Electron safeStorage key in the platform credential
// manager.  Windows uses DPAPI rather than the macOS Keychain command above;
// until a Windows DPAPI reader is available, make the limitation explicit and
// keep the probe a quiet, read-only miss instead of spawning a Unix command.
#[cfg(not(target_os = "macos"))]
fn keychain_pass(_service: &str, _account: &str) -> Option<String> {
    None
}

/// Chromium's macOS `safeStorage` envelope. No per-record nonce: the IV is fixed,
/// which is why the whole plaintext follows the 3-byte `v10` tag (measured here —
/// unlike Linux there is no 32-byte hash prefix to skip).
fn decrypt(pass: &str, blob: &[u8]) -> Option<String> {
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    let ct = blob.strip_prefix(b"v10".as_slice()).unwrap_or(blob);
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(pass.as_bytes(), b"saltysalt", 1003, &mut key);
    let iv = [0x20u8; 16];
    let plain = cbc::Decryptor::<aes::Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ct)
        .ok()?;
    String::from_utf8(plain).ok()
}

fn parse(plain: &str) -> Option<Value> {
    serde_json::from_str(plain).ok()
}

/// Whole when it is whole, one decimal when the plan really is fractional:
/// `{:.0}` rounds half-to-even, so 1500.5 would silently read back as 1500.
fn trim(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n:.1}")
    }
}

// ------------------------------------------------------------- the live meters

/// The meters the account's usage interface reports, in the panel's own order
/// and naming. `None` is "this account answered nothing drawable" — an
/// enterprise `displayMode` or a free account with no pack at all, which is
/// silence rather than a made-up bar.
fn live_usage(info: &Value) -> Option<Vec<QuotaSample>> {
    let token = info.get("token").and_then(Value::as_str).filter(|t| !t.is_empty())?;
    // The IDE refreshes this token when it lapses; a probe must not, so an
    // expired one means "no live sample", never a refresh grant.
    if !token_is_fresh(info, chrono::Utc::now().timestamp_millis()) {
        return None;
    }
    let body = get_json(
        USAGE_URL,
        &[
            ("authorization", &format!("Bearer {token}")),
            ("cosy-client-type", "10"),
            ("user-agent", "Qoder"),
            ("accept", "application/json"),
        ],
    )?;
    let usage = body.get("qoderUsage").filter(|_| body.get("displayMode") == Some(&Value::String("qoder".into())))?;
    Some(samples_from_usage(usage))
}

/// `expireTime` (old IDE: a JSON string of epoch milliseconds) or `expiresAt`
/// (0.4 app: RFC 3339). A missing or unparsable one is treated as
/// unknown-but-tryable, a past one as spent.
pub(crate) fn token_is_fresh(info: &Value, now_ms: i64) -> bool {
    let exp = ["expireTime", "expiresAt"]
        .iter()
        .find_map(|k| info.get(*k))
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(usage_core::parse_ts_ms)));
    exp.is_none_or(|ms| ms > now_ms)
}

pub(crate) fn samples_from_usage(usage: &Value) -> Vec<QuotaSample> {
    let resets = usage
        .get("expiresAt")
        .and_then(Value::as_i64)
        .filter(|ms| *ms > 0 && *ms < NO_EXPIRY_MS)
        .unwrap_or(0);
    let mut out = Vec::new();
    // The panel always draws plan credits first; the pack meters follow.
    if let Some(plan) = meter_sample(usage.get("userQuota"), "套餐内", resets) {
        out.push(plan);
    }
    if let Some(pack) = meter_sample(usage.get("addOnQuota"), "资源包", 0) {
        out.push(pack);
    }
    for pack in usage.get("dedicatedResourcePackages").and_then(Value::as_array).unwrap_or(&Vec::new()) {
        // The panel lists an unavailable pack with a badge ("已过期/已暂停/暂不
        // 可用"); a bar cannot say that, so it is simply not drawn.
        if pack.get("available") == Some(&Value::Bool(false)) {
            continue;
        }
        let name = pack
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or("专属资源包");
        if let Some(s) = meter_sample(Some(pack), name, 0) {
            out.push(s);
        }
    }
    if let Some(org) = usage.get("orgResourcePackage") {
        if let Some(s) = meter_sample(Some(org), "共享资源包", 0) {
            out.push(s);
        }
    }
    out
}

/// One meter as one bar. `percentage` is a fraction on the wire (measured 0.22
/// for 130/600); the vendor's own normalizer scales anything ≤ 1 to a percent,
/// and so does this. A meter with no allowance is not a 0 % bar: the free
/// tier's plan credits read as nothing rather than "spent".
fn meter_sample(meter: Option<&Value>, name: &str, resets_at_ms: i64) -> Option<QuotaSample> {
    let meter = meter?;
    let total = meter.get("total").and_then(Value::as_f64).filter(|n| n.is_finite())?;
    if total <= 0.0 {
        return None;
    }
    let used = meter.get("used").and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0);
    let percent = match meter.get("percentage").and_then(Value::as_f64).filter(|p| p.is_finite() && *p > 0.0) {
        Some(p) if p <= 1.0 => p * 100.0,
        Some(p) => p,
        None => used / total * 100.0,
    }
    .clamp(0.0, 100.0);
    let remaining = meter
        .get("remaining")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .unwrap_or((total - used).max(0.0));
    Some(QuotaSample {
        used_percent: percent,
        window_minutes: 0,
        resets_at_ms,
        label: Some(format!("{name} · 已用 {}/{}", trim(total - remaining), trim(total))),
        id: Some("credits".into()),
    })
}

// ------------------------------------------------------- the IDE's own snapshot

/// The offline fallback: the plan-credit snapshot the IDE keeps. It can only
/// ever describe a paid plan (`total > 0`) — the free tier's record is
/// `total: 0, isQuotaExceeded: true`, which drawn as a bar was exactly the
/// "已用完" this module no longer produces.
pub(crate) fn samples_from_snapshot(value: &Value) -> Vec<QuotaSample> {
    let quota = value.get("userQuota").cloned().unwrap_or(Value::Null);
    let num = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64).filter(|n| n.is_finite());
    let unit = quota
        .get("unit")
        .and_then(Value::as_str)
        .or_else(|| value.get("usageType").and_then(Value::as_str))
        .unwrap_or("credits");
    let total = num(&quota, "total").unwrap_or(0.0);
    let used = num(&quota, "used").unwrap_or(0.0);
    if total <= 0.0 {
        return Vec::new();
    }
    let percent = num(&quota, "percentage")
        .filter(|p| *p > 0.0)
        .map(|p| if p <= 1.0 { p * 100.0 } else { p })
        .unwrap_or_else(|| used / total * 100.0);
    vec![QuotaSample {
        used_percent: percent.clamp(0.0, 100.0),
        window_minutes: 0,
        resets_at_ms: value
            .get("expiresAt")
            .and_then(Value::as_i64)
            .filter(|ms| *ms > 0 && *ms < NO_EXPIRY_MS)
            .unwrap_or(0),
        label: Some(format!("{unit} {}/{}", trim(used), trim(total))),
        id: Some("credits".into()),
    }]
}

// ---- daily check-in ---------------------------------------------------------
//
// The credits campaign claim rides the same bearer token and base the usage
// meter already answers (`openapi.qoder.sh`; the CN edition's base is the
// fallback). Contract read out of the Qoder client's own main.log "[Campaign]"
// lines and replayed by hand: the list header is `Cosy-ClientType` — NO dash;
// the dashed `cosy-client-type` the community claimers pass is an unknown
// client to the server, which answers an EMPTY list for it. Only
// `CLAIM_BENEFIT` rows still in `CLAIMABLE` are worth a claim, POSTed to
// `.../{campaignId}/claim` (the UUID). The answer body is FLAT — `status` at
// the top level; the `data.status` an earlier reader trusted is the client's
// own postMessage RPC envelope and never appears on HTTP (measured
// 2026-10-09, when this machine claimed live: the first two attempts answered
// 200 `BLOCKED`/`RISK_BLOCKED` — the grant record is created on the first
// attempt and `claimedAt` sticks — and a 76 s retry answered `CLAIMED` with
// the row flipped, so BLOCKED is a retry-later risk gate, never a verdict).
// The claim route resolves the request through the
// vendor's device-identity service and refuses a bare request with 503
// SAME_PERSON_DEPENDENCY_UNAVAILABLE — forged machine headers fare no better
// (measured 2026-10-09) — so the claim goes out with the client's own
// identity: the persistent machine UUID from the app's `auth.machine-id` plus
// `Cosy-Machine{Token,Type,Code}` from the app's own UMID bridge, minted
// lazily (see [`QoderClaimIdentity`]). The same identity also gates what the
// list itself *shows*: two GETs a second apart, identical but for the
// Cosy-Machine* set (measured 2026-10-09 13:56), and the bare one hid the
// day's row entirely while the identity one listed it CLAIMABLE — so a bare
// answer is trusted for what it shows, never for what it omits, and a bare
// list with nothing claimable is re-asked with the minted identity before
// the day is read as closed or empty. Without the app on disk the claim
// falls back to the bare request, and that refusal reads as `claim_gated` —
// only it: a 200-BLOCKED row keeps retrying, because measured retries win.
// So tokenme reports the state honestly: CLAIMED rows mark the day 已签到,
// a claimable row the device gate refuses sets `claim_gated` (marker row +
// button label point at the client; recorded for the day so the auto pass
// stops re-POSTing the refused request), an empty list is `no_campaign`
// (the day stays open), and only a claim lost to anything else reads as
// 签到未成功.

const CAMPAIGNS_PATH: &str = "/sash/api/v1/me/campaigns";
const CHECKIN_BASES: [&str; 2] = ["https://openapi.qoder.sh", "https://openapi.qoder.com.cn"];
/// How long one minted machine identity stays fresh. The client itself keeps
/// its spawn result for an hour; 30 minutes matches what the community
/// bridges field-verified as "briefly stale is still accepted".
const IDENTITY_TTL_MS: i64 = 30 * 60 * 1000;

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct QoderCheckinState {
    /// Local day of the last attempt — successful or not. The once-a-day lock.
    last_attempt_day: String,
    /// Local day of the last success — the 已签到 marker the label reads.
    last_ok_day: String,
    /// Credits the last successful claim paid out.
    last_credits: f64,
    /// Local day a claim last hit the SAME_PERSON device gate. The auto pass
    /// skips the POST for the rest of that day — retrying the same refused
    /// request is what probes into a rate limit — and reports `claim_gated`
    /// off this record; a manual click still tries again.
    last_gated_day: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct QoderCheckinOutcome {
    /// The day is closed: claimed now, or had been claimed earlier today.
    done_today: bool,
    /// Already checked in (nothing claimable) rather than claimed now.
    was_already: bool,
    /// Claimable rows exist but the day's window has not opened (before
    /// 10:00 local): the button greys instead of claiming into the void.
    too_early: bool,
    /// The server answered with a campaigns envelope but had nothing claimable
    /// and nothing already claimed — no activity for this account today. Not
    /// an error: the day stays open in case a campaign opens later.
    no_campaign: bool,
    /// A CLAIMABLE row exists but the claim POST was refused by the vendor's
    /// device-identity gate (503 SAME_PERSON_DEPENDENCY_UNAVAILABLE) with
    /// whatever identity this machine could mint — the app's own bridge when
    /// the app is installed — so the UI points there. Recorded for the day:
    /// the auto pass stops re-POSTing the refused request until the record
    /// falls off, and a manual click forces past it.
    claim_gated: bool,
    /// A claim was attempted and lost to anything else (transport, other 5xx,
    /// business refusal): the plain retry-later failure.
    claim_failed: bool,
    credits: f64,
}

fn checkin_state_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("tokenme").join("quota").join("qoder_checkin.json"))
}

fn load_checkin_state(path: &std::path::Path) -> QoderCheckinState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_checkin_state(path: &std::path::Path, state: &QoderCheckinState) {
    if let Ok(text) = serde_json::to_string(state) {
        let _ = std::fs::write(path, text);
    }
}

fn checkin_day() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// The benefit campaigns a claim can still win: the client's own filter.
/// `campaignId` is a number on the wire today; both spellings parse.
fn claimable_campaigns(body: &Value) -> Vec<(String, f64)> {
    body.get("campaigns")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    if c.get("actionType").and_then(Value::as_str) != Some("CLAIM_BENEFIT") {
                        return None;
                    }
                    if c.get("claimStatus").and_then(Value::as_str) != Some("CLAIMABLE") {
                        return None;
                    }
                    let id = match c.get("campaignId") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => return None,
                    };
                    let amount = c.pointer("/benefit/amount").and_then(Value::as_f64).unwrap_or(0.0);
                    Some((id, amount))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether today's benefit is already paid: at least one `CLAIM_BENEFIT` row
/// whose window started today (the daily campaign refreshes at 10:00 local,
/// yesterday's row lingers claimed until 09:59), with every such row
/// `CLAIMED`. A row that is missing proves nothing — measured 2026-10-09 the
/// vendor drops today's row for minutes and answers CLAIMABLE again — and
/// `VIEW_DETAILS` placements sit CLAIMED all their life and prove nothing.
fn today_benefits_all_claimed(body: &Value) -> bool {
    let Some(rows) = body.get("campaigns").and_then(Value::as_array) else { return false };
    let today = chrono::Local::now().date_naive();
    let mut any_today = false;
    for r in rows {
        if r.get("actionType").and_then(Value::as_str) != Some("CLAIM_BENEFIT") {
            continue;
        }
        let started_today = r
            .get("startAt")
            .and_then(Value::as_i64)
            .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
            .map(|dt| dt.with_timezone(&chrono::Local).date_naive() == today)
            .unwrap_or(false);
        if !started_today {
            continue;
        }
        any_today = true;
        if r.get("claimStatus").and_then(Value::as_str) != Some("CLAIMED") {
            return false;
        }
    }
    any_today
}

/// The campaigns list. The Cosy-Machine* set is not just the claim's
/// prerequisite: the same GET answers a *different list* with and without it
/// (the identity-gated visibility measured 2026-10-09 13:56), so only an
/// identity-bearing read may be believed about an omission.
fn checkin_campaigns(base: &str, token: &str, ident: Option<&QoderClaimIdentity>) -> Option<Value> {
    let url = format!("{base}{CAMPAIGNS_PATH}");
    let mut req = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout_read(std::time::Duration::from_secs(10))
        .build()
        .get(&url)
        .set("authorization", &format!("Bearer {token}"))
        .set("Cosy-ClientType", "10")
        .set("Cosy-Version", "0.4.3")
        .set("user-agent", "Qoder")
        .set("accept", "application/json");
    if let Some(ident) = ident {
        req = with_machine_headers(req, ident);
    }
    let resp = req.call().ok()?;
    resp.into_string().ok().and_then(|t| serde_json::from_str(&t).ok())
}

/// The machine identity the claim route resolves the request through. The
/// desktop client mints it in-process before every campaigns call: a
/// persistent UUID it keeps in `auth.machine-id`, plus `machineToken/type/code`
/// from the UMID security SDK's native bridge the app ships
/// (`resources/umid/runtime-info`, the Alibaba device-identity half). The
/// server's `SAME_PERSON` check refuses everything without this set — forged
/// values included (measured 2026-10-09) — so tokenme runs the app's own
/// bridge instead of imitating it (cross-checked against the open-source
/// claimers: mmqz/cpa-multi-plugins, shuishuipingan/qoder2api-hub,
/// HUIdada1/AgentHub, yetone/magpie).
#[derive(Clone)]
struct QoderClaimIdentity {
    machine_id: String,
    machine_os: String,
    hostname: Option<String>,
    identity: QoderMachineIdentity,
}

/// The bridge's stdout shape. Extra keys (`vmInfo`, `accountOutcome`) are the
/// SDK's own and stay ignored.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct QoderMachineIdentity {
    #[serde(rename = "machineToken")]
    token: String,
    #[serde(rename = "machineType")]
    kind: String,
    #[serde(rename = "machineCode")]
    code: String,
}

/// One bridge answer parsed and complete, or nothing: the server filters a
/// malformed identity set the same as no set at all, so a partial one is
/// never worth sending.
fn parse_machine_identity(out: &str) -> Option<QoderMachineIdentity> {
    let id: QoderMachineIdentity = serde_json::from_str(out).ok()?;
    (!id.token.is_empty() && !id.kind.is_empty() && !id.code.is_empty()).then_some(id)
}

#[cfg(target_os = "macos")]
fn umid_candidates() -> Vec<std::path::PathBuf> {
    let rel = "Contents/Resources/umid/runtime-info";
    let mut out: Vec<_> = ["/Applications/Qoder.app"].iter().map(|r| std::path::Path::new(r).join(rel)).collect();
    if let Some(home) = dirs::home_dir() {
        out.push(home.join("Applications").join("Qoder.app").join(rel));
    }
    out
}

#[cfg(windows)]
fn umid_candidates() -> Vec<std::path::PathBuf> {
    let rel = "resources\\umid\\runtime-info.exe";
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("LOCALAPPDATA") {
        out.push(std::path::PathBuf::from(p).join("Programs").join("Qoder").join(rel));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(p) = std::env::var_os(var) {
            out.push(std::path::PathBuf::from(p).join("Qoder").join(rel));
        }
    }
    out
}

#[cfg(not(any(windows, target_os = "macos")))]
fn umid_candidates() -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// The app's own bridge binary; `TOKENME_QODER_UMID_BIN` overrides discovery
/// (tests), and a missing app just means no identity — never a mock one.
fn umid_binary() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("TOKENME_QODER_UMID_BIN").map(std::path::PathBuf::from) {
        return p.is_file().then_some(p);
    }
    umid_candidates().into_iter().find(|p| p.is_file())
}

/// One bridge run (3.1 s measured on this machine): `runtime-info prod
/// --account-stdin` with `{"account": uid}` on stdin answers the machine-level
/// identity — identical across account ids on one host. The child is killed
/// at 25 s; a killed or malformed run mints nothing and the claim falls back
/// to the gated path.
fn bridge_identity(uid: &str) -> Option<QoderMachineIdentity> {
    use std::io::{Read, Write};
    let exe = umid_binary()?;
    #[cfg(any(windows, target_os = "macos"))]
    const ACCOUNT_STDIN: bool = true;
    #[cfg(not(any(windows, target_os = "macos")))]
    const ACCOUNT_STDIN: bool = false;
    let args: &[&str] = if ACCOUNT_STDIN { &["prod", "--account-stdin"] } else { &["prod"] };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().ok()?;
    {
        let mut stdin = child.stdin.take()?;
        if ACCOUNT_STDIN {
            let _ = stdin.write_all(serde_json::json!({ "account": uid }).to_string().as_bytes());
        }
        // Dropping stdin closes the pipe; the bridge reads to EOF.
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    parse_machine_identity(&out)
}

/// Minted identities are machine-level and accepted briefly stale, so one
/// process keeps the last answer instead of paying a native run per poll.
/// Losing the cache to a restart just mints once more.
static MACHINE_IDENTITY: std::sync::Mutex<Option<(i64, QoderMachineIdentity)>> =
    std::sync::Mutex::new(None);

fn machine_identity(uid: &str) -> Option<QoderMachineIdentity> {
    let now = chrono::Local::now().timestamp_millis();
    let mut cache = MACHINE_IDENTITY.lock().ok()?;
    if let Some((at, id)) = cache.as_ref() {
        if now - at < IDENTITY_TTL_MS {
            return Some(id.clone());
        }
    }
    let id = bridge_identity(uid)?;
    *cache = Some((now, id.clone()));
    Some(id)
}

/// The persistent machine UUID the app writes once. Read only: creating one
/// here would mint a device identity the real client would then adopt — a
/// decision that belongs to the app, not this probe.
fn machine_id_file() -> Option<String> {
    app_dirs()
        .iter()
        .map(|d| d.join("auth.machine-id"))
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// The client's `Cosy-MachineOS` spelling (arch_os; the server checks the
/// header for presence, and the community cross-check spans "aarch64_darwin"
/// and "x86_64_win32").
fn machine_os() -> String {
    let arch = match std::env::consts::ARCH {
        "arm64" | "aarch64" => "aarch64",
        other => other,
    };
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    format!("{arch}_{os}")
}

#[cfg(windows)]
fn machine_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok().map(|h| h.trim().to_string()).filter(|h| !h.is_empty())
}

#[cfg(unix)]
fn machine_hostname() -> Option<String> {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

/// Assemble the full set the claim adds beyond the bearer, or nothing when a
/// half is missing (no app, no bridge answer, no machine id): the claim then
/// goes out bare and the vendor's answer decides the outcome as before.
fn claim_identity(uid: &str) -> Option<QoderClaimIdentity> {
    Some(QoderClaimIdentity {
        machine_id: machine_id_file()?,
        machine_os: machine_os(),
        hostname: machine_hostname(),
        identity: machine_identity(uid)?,
    })
}

/// One claim answer from wire shape: `(claimed, credits, gated)`. The body is
/// flat — `status` and `benefit` at the top level; the `{requestId,status,
/// data}` wrapper lives only in the client's own postMessage RPC and an
/// earlier reader built from that envelope read every real answer as
/// "unclaimed" (measured live 2026-10-09). A 503 with the SAME_PERSON error
/// is the vendor's device-identity gate — measured live it refuses every
/// identity set this machine can mint without the client app — so it reads as
/// "claim in the client", not "retry". A 200 whose status is BLOCKED
/// (failureCode RISK_BLOCKED) is the risk gate on the grant, not a verdict:
/// measured live the same request answered CLAIMED 76 s later, so it falls
/// through to the retry-later failure.
fn claim_answer(status: u16, body: &Value) -> (bool, f64, bool) {
    let claimed = body.get("status").and_then(Value::as_str) == Some("CLAIMED");
    let credits = body.pointer("/benefit/amount").and_then(Value::as_f64).unwrap_or(0.0);
    let gated = status == 503
        && body.get("errorCode").and_then(Value::as_str).is_some_and(|c| c.contains("SAME_PERSON"));
    (claimed, credits, gated)
}

/// The Cosy-Machine* set the client sends once it has minted an identity:
/// both the claim's SAME_PERSON resolution and the list's visibility read it.
fn with_machine_headers(req: ureq::Request, ident: &QoderClaimIdentity) -> ureq::Request {
    let req = req
        .set("Cosy-MachineId", &ident.machine_id)
        .set("Cosy-MachineOS", &ident.machine_os)
        .set("Cosy-MachineToken", &ident.identity.token)
        .set("Cosy-MachineType", &ident.identity.kind)
        .set("Cosy-MachineCode", &ident.identity.code);
    match &ident.hostname {
        Some(h) => req.set("Cosy-MachineHostname", h),
        None => req,
    }
}

fn checkin_claim(base: &str, token: &str, campaign: &str, ident: Option<&QoderClaimIdentity>) -> Option<(bool, f64, bool)> {
    let url = format!("{base}{CAMPAIGNS_PATH}/{campaign}/claim");
    let mut req = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout_read(std::time::Duration::from_secs(10))
        .build()
        .post(&url)
        .set("authorization", &format!("Bearer {token}"))
        .set("Cosy-ClientType", "10")
        .set("Cosy-Version", "0.4.3")
        .set("user-agent", "Qoder")
        .set("accept", "application/json");
    if let Some(ident) = ident {
        req = with_machine_headers(req, ident);
    }
    // ureq answers non-2xx as Err(Status(code, resp)); the gate is a 503, so
    // the response body must survive the error path to be recognized.
    let answer = match req.send_string("") {
        Ok(resp) => Some(resp),
        Err(ureq::Error::Status(_, resp)) => Some(resp),
        Err(_) => None,
    };
    let resp = answer?;
    let status = resp.status();
    let body: Value = resp.into_string().ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    Some(claim_answer(status, &body))
}

/// Fire today's claim (unless today is already spent) and report the outcome.
/// `None` = no usable token or no state file: the probe then never claims.
fn qoder_daily_checkin(info: &Value, force: bool) -> Option<QoderCheckinOutcome> {
    let path = checkin_state_path()?;
    let token = info.get("token").and_then(Value::as_str).filter(|t| !t.is_empty())?;
    let mut state = load_checkin_state(&path);
    let today = checkin_day();
    // A day that was already attempted keeps its recorded outcome, before any
    // other gate: the 已签到 marker must survive restarts, rebuilds and the
    // whole morning after a nine-o'clock claim, whatever the clock says now.
    if !force && state.last_attempt_day == today {
        return Some(QoderCheckinOutcome {
            done_today: state.last_ok_day == today,
            was_already: false,
            too_early: false,
            no_campaign: false,
            claim_gated: false,
            claim_failed: false,
            credits: state.last_credits,
        });
    }
    // The day's campaign opens at 10:00 local: before that a claimable list
    // must not be claimed early and an empty list must not close the day —
    // but the status read below still runs, so a benefit the IDE already paid
    // (today's row CLAIMED) marks the day at any hour. The next pass after
    // ten retries an open day on its own.
    let may_claim = force || chrono::Timelike::hour(&chrono::Local::now()) >= 10;
    let mut outcome = QoderCheckinOutcome {
        done_today: false,
        was_already: false,
        too_early: false,
        no_campaign: false,
        claim_gated: false,
        claim_failed: false,
        credits: 0.0,
    };
    let mut answered = false; // a base returned a parseable campaigns envelope
    let mut attempted = false; // a claim POST actually went out
    'bases: for base in CHECKIN_BASES {
        // Bare read first: it resolves the uid the mint needs without paying
        // the bridge run, and it is believed about what it *shows* — a
        // claimable row, today's row already CLAIMED.
        let Some(mut body) = checkin_campaigns(base, token, None) else { continue };
        answered = true;
        // But its omissions prove nothing: the bare list hides the day's row
        // (the identity-gated visibility above), so a bare answer with
        // nothing claimable and nothing closed is re-asked with the minted
        // identity before the day is read as closed or empty. The mint is
        // cached for 30 min, so this costs one bridge run per half hour.
        let mut ident = if claimable_campaigns(&body).is_empty() && !today_benefits_all_claimed(&body) {
            body.get("uid").and_then(Value::as_str).and_then(claim_identity)
        } else {
            None
        };
        if let Some(minted) = &ident {
            if let Some(retry) = checkin_campaigns(base, token, Some(minted)) {
                body = retry;
            }
        }
        for (id, amount) in claimable_campaigns(&body) {
            if !may_claim {
                outcome.too_early = true; // claimable but too early: day open, button greys
                break 'bases;
            }
            // A device-gate refusal recorded earlier today stands: the only
            // identity this machine can mint is already refused, and re-POSTing
            // it every pass is what probes into a rate limit. A manual click
            // forces past this.
            if !force && state.last_gated_day == today {
                outcome.claim_gated = true;
                break 'bases;
            }
            attempted = true;
            // The claim reuses the identity the re-list minted; a bare list
            // that already showed the row mints it here instead.
            let ident = ident.clone().or_else(|| body.get("uid").and_then(Value::as_str).and_then(claim_identity));
            match checkin_claim(base, token, &id, ident.as_ref()) {
                Some((true, credits, _)) => {
                    state.last_attempt_day = today.clone();
                    state.last_ok_day = today.clone();
                    state.last_credits = if credits > 0.0 { credits } else { amount };
                    outcome = QoderCheckinOutcome {
                        done_today: true,
                        was_already: false,
                        too_early: false,
                        no_campaign: false,
                        claim_gated: false,
                        claim_failed: false,
                        credits: state.last_credits,
                    };
                    break 'bases;
                }
                // The vendor's device-identity gate: the row is claimable but
                // only the client can claim it. Say so, record the day, and
                // stop — retrying a gated claim is what probes into a rate
                // limit.
                Some((false, _, true)) => {
                    state.last_gated_day = today.clone();
                    outcome.claim_gated = true;
                    break 'bases;
                }
                _ => {} // this row refused for another reason; try the next
            }
        }
        // Today's daily row already CLAIMED closes the day — and nothing
        // else does. A row that is *absent* from the list proves nothing:
        // measured 2026-10-09 the vendor drops the day's row for minutes at
        // a time (11:16:34 listed only the lingering VIEW_DETAILS row,
        // 11:16:36 the row was back CLAIMABLE), and the old all-rows-claimed
        // reading stamped the day paid off that flap — the badge said 已签到
        // while the client still showed the benefit unclaimed. Only rows
        // whose window started *today* may speak for today at all.
        if today_benefits_all_claimed(&body) {
            state.last_attempt_day = today.clone();
            state.last_ok_day = today.clone();
            outcome = QoderCheckinOutcome {
                done_today: true,
                was_already: true,
                too_early: false,
                no_campaign: false,
                claim_gated: false,
                claim_failed: false,
                credits: 0.0,
            };
            break 'bases;
        }
    }
    if answered && !outcome.done_today && !outcome.too_early && !outcome.claim_gated {
        if attempted {
            outcome.claim_failed = true; // we asked and were refused for other reasons
        } else {
            outcome.no_campaign = true; // the server spoke and had nothing to claim
        }
    }
    save_checkin_state(&path, &state);
    Some(outcome)
}

/// The panel's check-in button: one forced claim right now, past the 10:00
/// gate and the day lock (a manual click is the user asking again). The bool
/// says whether the day actually closed, so the button only turns into the
/// done badge for a real claim — "queued" or "too early" stay buttons.
pub fn qoder_manual_checkin() -> Result<(bool, String), String> {
    let info = app_dirs()
        .iter()
        .find(|d| d.join(AUTH_FILE).is_file())
        .and_then(|dir| discover_login_info(dir))
        .ok_or_else(|| "未找到 Qoder 登录".to_string())?;
    if chrono::Timelike::hour(&chrono::Local::now()) < 10 && !state_says_done_today() {
        return Ok((false, "未到签到时间，10:00 后可领".into()));
    }
    match qoder_daily_checkin(&info, true) {
        Some(o) if o.done_today && !o.was_already && o.credits > 0.0 => {
            Ok((true, format!("签到成功，+{:.0} 积分", o.credits)))
        }
        Some(o) if o.done_today => Ok((true, "今日已签到".to_string())),
        Some(o) if o.claim_gated => Ok((false, "请在 Qoder 客户端内领取".into())),
        Some(o) if o.no_campaign => Ok((false, "今日暂无可领的签到活动".into())),
        _ => Ok((false, "签到未成功，稍后再试".to_string())),
    }
}

/// Whether today is already recorded as claimed — a manual click before 10:00
/// on a morning the panel itself claimed at 09:59 still says 已签到.
fn state_says_done_today() -> bool {
    checkin_state_path()
        .map(|p| load_checkin_state(&p))
        .map(|s| s.last_ok_day == checkin_day())
        .unwrap_or(false)
}

/// The row that carries the 已签到 mark into the strip: it only exists for a
/// closed day, and the label is the exact wording the frontend's badge reads.
fn checkin_marker_row(outcome: QoderCheckinOutcome) -> QuotaSample {
    let label = if outcome.was_already {
        "今日已签到".to_string()
    } else if outcome.credits > 0.0 {
        format!("已自动签到 · +{:.0}", outcome.credits)
    } else {
        "已自动签到".to_string()
    };
    QuotaSample {
        used_percent: 0.0,
        window_minutes: 0,
        resets_at_ms: 0,
        label: Some(label),
        id: Some("checkin".into()),
    }
}

/// Append the checkin mark to a live sample set, once the day is closed.
fn with_checkin_mark(samples: Vec<QuotaSample>, outcome: &Option<QoderCheckinOutcome>) -> Vec<QuotaSample> {
    match outcome {
        Some(o) if o.done_today => {
            let mut out = samples;
            out.push(checkin_marker_row(*o));
            out
        }
        // The wait row is what greys the strip's button before the day's
        // window opens: same channel as the done mark, a different id.
        Some(o) if o.too_early => {
            let mut out = samples;
            out.push(QuotaSample {
                used_percent: 0.0,
                window_minutes: 0,
                resets_at_ms: 0,
                label: Some("未到签到时间 · 10:00 开领".into()),
                id: Some("checkin-wait".into()),
            });
            out
        }
        // The gate row names where the claim can actually happen: the client
        // mints the device fingerprint no third party carries.
        Some(o) if o.claim_gated => {
            let mut out = samples;
            out.push(QuotaSample {
                used_percent: 0.0,
                window_minutes: 0,
                resets_at_ms: 0,
                label: Some("签到需在 Qoder 内领取".into()),
                id: Some("checkin-gated".into()),
            });
            out
        }
        _ => samples,
    }
}

/// The panel's check-in button: one forced claim right now, whatever the
/// automatic pass already tried today. The message is user-facing.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The campaigns shape the client acts on: benefit actions only, and only
    /// while `CLAIMABLE`. `campaignId` is a number on the wire today; a string
    /// spelling must survive too.
    #[test]
    fn claimable_campaigns_filters_to_claimable_benefits() {
        let body = json!({"campaigns": [
            {"campaignId": 42, "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMABLE",
             "benefit": {"amount": 100}},
            {"campaignId": 7, "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMED",
             "benefit": {"amount": 100}},
            {"campaignId": 9, "actionType": "ATTEND_TASK", "claimStatus": "CLAIMABLE"},
            {"campaignId": "str-id", "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMABLE",
             "benefit": {"amount": 30}}
        ]});
        assert_eq!(claimable_campaigns(&body), vec![("42".to_string(), 100.0), ("str-id".to_string(), 30.0)]);
        // An empty or malformed list parses to nothing, never panics.
        assert!(claimable_campaigns(&json!({"campaigns": []})).is_empty());
        assert!(claimable_campaigns(&Value::Null).is_empty());
    }

    /// Only a today-dated `CLAIM_BENEFIT` row that is CLAIMED may close the
    /// day. The shape that actually fired the false 已签到 (2026-10-09):
    /// today's row dropped from the vendor's list for minutes, leaving only
    /// the VIEW_DETAILS placement that is CLAIMED all its life — the old
    /// all-rows-claimed reading stamped the day off exactly that flap while
    /// the client still showed the benefit unclaimed.
    #[test]
    fn only_a_today_benefit_row_all_claimed_closes_the_day() {
        let now = chrono::Local::now().timestamp();
        // The flap: today's row absent, a VIEW_DETAILS row CLAIMED.
        let flap = json!({"campaigns": [
            {"campaignId": 1, "actionType": "VIEW_DETAILS", "claimStatus": "CLAIMED"}]});
        assert!(!today_benefits_all_claimed(&flap));
        // Nothing listed at all: open, never paid.
        assert!(!today_benefits_all_claimed(&json!({"campaigns": []})));
        assert!(!today_benefits_all_claimed(&Value::Null));
        // Today's row CLAIMED = paid.
        let paid = json!({"campaigns": [
            {"campaignId": 2, "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMED", "startAt": now}]});
        assert!(today_benefits_all_claimed(&paid));
        // Claimable, mixed, or without a startAt: still open.
        let open = json!({"campaigns": [
            {"campaignId": 2, "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMABLE", "startAt": now}]});
        assert!(!today_benefits_all_claimed(&open));
        let mixed = json!({"campaigns": [
            {"campaignId": 2, "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMED", "startAt": now},
            {"campaignId": 3, "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMABLE", "startAt": now}]});
        assert!(!today_benefits_all_claimed(&mixed));
        let no_start = json!({"campaigns": [
            {"campaignId": 4, "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMED"}]});
        assert!(!today_benefits_all_claimed(&no_start));
        // Yesterday's lingering claimed row says nothing about today.
        let yesterday = json!({"campaigns": [
            {"campaignId": 5, "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMED", "startAt": now - 86_400}]});
        assert!(!today_benefits_all_claimed(&yesterday));
    }

    /// The label is the exact wording the strip's badge reads — a drift here
    /// silently demotes the row to decoration.
    #[test]
    fn the_checkin_marker_label_carries_the_badge_wording() {
        let already = checkin_marker_row(QoderCheckinOutcome {
            done_today: true,
            was_already: true,
            too_early: false,
            no_campaign: false,
            claim_gated: false,
            claim_failed: false,
            credits: 0.0,
        });
        assert_eq!(already.label.as_deref(), Some("今日已签到"));
        let claimed = checkin_marker_row(QoderCheckinOutcome {
            done_today: true,
            was_already: false,
            too_early: false,
            no_campaign: false,
            claim_gated: false,
            claim_failed: false,
            credits: 100.0,
        });
        assert_eq!(claimed.label.as_deref(), Some("已自动签到 · +100"));
        let plain = checkin_marker_row(QoderCheckinOutcome {
            done_today: true,
            was_already: false,
            too_early: false,
            no_campaign: false,
            claim_gated: false,
            claim_failed: false,
            credits: 0.0,
        });
        assert_eq!(plain.label.as_deref(), Some("已自动签到"));
        // An open day carries no row at all.
        let none: Vec<QuotaSample> = with_checkin_mark(
            vec![QuotaSample {
                used_percent: 1.0,
                window_minutes: 0,
                resets_at_ms: 0,
                label: None,
                id: Some("credits".into()),
            }],
            &None,
        );
        assert_eq!(none.len(), 1, "no claim, no marker row");
    }

    /// The claim answers measured live on this machine (2026-10-09): the body
    /// is FLAT, the first attempts answered 200 BLOCKED/RISK_BLOCKED off the
    /// same grant record, and a 76 s retry answered CLAIMED. An earlier reader
    /// read `/data/status` — the client's postMessage RPC envelope, never on
    /// the wire — and would have read the win itself as "unclaimed".
    #[test]
    fn the_claim_answer_is_read_from_the_flat_wire_shape() {
        let blocked = json!({"grantId":"01a11f4b-8ad7-7303-a3b2-356b6a81e336","status":"BLOCKED",
            "replayed":false,"benefit":{"kind":"CREDITS","amount":100,"validity":{"mode":"RELATIVE_DAYS","days":30}},
            "failureCode":"RISK_BLOCKED","campaignId":"01a0f1dc-5da3-7dea-bea3-7cf8f297d276",
            "campaignKey":"act-20260930-475","campaignVersion":1,"claimedAt":"2026-10-09T06:13:26.828541Z"});
        // The grant's benefit rides both answers — only `claimed` decides, and
        // callers read credits off a win alone.
        assert_eq!(claim_answer(200, &blocked), (false, 100.0, false), "BLOCKED is retry-later, not a gate");
        let claimed = json!({"grantId":"01a11f4b-8ad7-7303-a3b2-356b6a81e336","status":"CLAIMED",
            "replayed":false,"benefit":{"kind":"CREDITS","amount":100,"validity":{"mode":"RELATIVE_DAYS","days":30}},
            "campaignId":"01a0f1dc-5da3-7dea-bea3-7cf8f297d276","campaignKey":"act-20260930-475",
            "campaignVersion":1,"claimedAt":"2026-10-09T06:13:26.828541Z","grantedAt":"2026-10-09T06:21:39.798898Z",
            "expiresAt":"2026-11-08T06:13:26.828541Z"});
        assert_eq!(claim_answer(200, &claimed), (true, 100.0, false));
        // A replay of that same win is still the win.
        assert_eq!(claim_answer(200, &json!({"status":"CLAIMED","replayed":true,"benefit":{"amount":100}})), (true, 100.0, false));
        // The device gate (measured with and without forged machine headers):
        // 503 + SAME_PERSON means "claim in the client" — not a retry.
        assert_eq!(
            claim_answer(503, &json!({"errorCode":"SAME_PERSON_DEPENDENCY_UNAVAILABLE","errorMessage":"campaign service is temporarily unavailable"})),
            (false, 0.0, true)
        );
        assert_eq!(claim_answer(503, &json!({"errorCode":"OTHER"})), (false, 0.0, false));
        // A flat business refusal is neither.
        assert_eq!(claim_answer(200, &json!({"code":1,"msg":"queued"})), (false, 0.0, false));
    }

    /// The gate row's id must not read as a claim: the strip's done badge
    /// greps ids with an explicit wait/gated exclusion, and this test pins the
    /// id/label pair it greps against.
    #[test]
    fn the_gated_row_names_the_client_without_reading_as_done() {
        let gated_outcome = QoderCheckinOutcome {
            done_today: false,
            was_already: false,
            too_early: false,
            no_campaign: false,
            claim_gated: true,
            claim_failed: false,
            credits: 0.0,
        };
        let rows: Vec<QuotaSample> = with_checkin_mark(vec![], &Some(gated_outcome));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_deref(), Some("checkin-gated"));
        assert_eq!(rows[0].label.as_deref(), Some("签到需在 Qoder 内领取"));
    }

    /// The bridge's real stdout shape (measured 2026-10-09 on this machine:
    /// `machineToken` 88 chars "P1gA…", 18-hex type/code, plus SDK keys the
    /// parser must ignore). A partial answer mints nothing — the server
    /// filters a malformed identity set the same as no set at all.
    #[test]
    fn machine_identity_needs_all_three_bridge_values() {
        let real = r#"{"machineToken":"P1gAbDcEfGhIjKlMnOpQrStUvWxYz0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJK","machineType":"118d1e0191dead8b10","machineCode":"e773c10000123434bc","vmInfo":{"isVm":false,"brand":"None","percentage":0,"vmTypeCode":91},"accountOutcome":"success"}"#;
        let id = parse_machine_identity(real).expect("the real shape parses");
        assert_eq!(id.kind, "118d1e0191dead8b10");
        assert!(parse_machine_identity(r#"{"machineToken":"P1gA","machineType":"x"}"#).is_none());
        assert!(parse_machine_identity(r#"{"machineToken":"","machineType":"x","machineCode":"y"}"#).is_none());
        assert!(parse_machine_identity("").is_none());
        assert!(parse_machine_identity("not json").is_none());
    }

    /// `Cosy-MachineOS` rides the client's arch_os shape; the community
    /// cross-check spans "aarch64_darwin" and "x86_64_win32".
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn machine_os_is_the_clients_arch_underscore_os_shape() {
        let os = machine_os();
        #[cfg(target_os = "macos")]
        assert!(os.ends_with("_darwin"), "{os}");
        #[cfg(windows)]
        assert!(os.ends_with("_win32"), "{os}");
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(os, "aarch64_darwin");
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        assert_eq!(os, "x86_64_darwin");
        #[cfg(windows)]
        assert!(os.starts_with("x86_64_"), "{os}");
    }

    /// Discovery reaches the app bundle the client is actually installed as.
    #[cfg(target_os = "macos")]
    #[test]
    fn umid_candidates_name_the_apps_own_bridge() {
        let cands = umid_candidates();
        assert!(cands.iter().any(|p| p.to_string_lossy() == "/Applications/Qoder.app/Contents/Resources/umid/runtime-info"));
        assert!(cands.len() >= 2, "the per-user Applications folder is a candidate too");
    }

    /// A `TOKENME_QODER_UMID_BIN` override is a file check, not a fallback
    /// seed: a broken override must mint nothing instead of quietly running
    /// whatever the disk happens to hold.
    #[test]
    fn umid_binary_env_override_is_a_file_check_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("runtime-info");
        std::fs::write(&fake, b"#!/bin/sh\n").unwrap();
        std::env::set_var("TOKENME_QODER_UMID_BIN", &fake);
        assert_eq!(umid_binary().as_deref(), Some(fake.as_path()));
        std::env::set_var("TOKENME_QODER_UMID_BIN", dir.path().join("missing"));
        assert_eq!(umid_binary(), None, "a broken override must not fall through to discovery");
        std::env::remove_var("TOKENME_QODER_UMID_BIN");
    }

    /// The response measured live on this machine (2026-09-24, `personal_standard`):
    /// a free plan with **no** plan credits and a resource pack in use. Note what
    /// the IDE's own snapshot of this account says by contrast — `total: 0`,
    /// `isQuotaExceeded: true` — which is the stale half-truth this probe no
    /// longer draws a bar from.
    const FREE_WITH_PACK: &str = r#"{"displayMode":"qoder","qoderUsage":{"userId":"019f…",
        "userType":"personal_standard","usageType":"credits","totalUsagePercentage":0.22,
        "isQuotaExceeded":false,"expiresAt":253402214400000,
        "userQuota":{"total":0,"used":0,"remaining":0,"percentage":0,"unit":"credits"},
        "addOnQuota":{"total":600,"used":130,"remaining":470,"percentage":0.22,"unit":"credits",
          "detailUrl":"https://qoder.com/account/usage"},"isPlanQuotaProrated":false}}"#;

    #[test]
    fn a_free_plan_with_a_resource_pack_shows_the_pack_and_only_the_pack() {
        let body: Value = serde_json::from_str(FREE_WITH_PACK).unwrap();
        let usage = body.get("qoderUsage").unwrap();
        let s = samples_from_usage(usage);
        assert_eq!(s.len(), 1, "{s:?} — the 0/0 plan row is not a bar");
        assert!((s[0].used_percent - 22.0).abs() < 1e-9, "percentage arrives as a fraction");
        assert_eq!(s[0].label.as_deref(), Some("资源包 · 已用 130/600"));
        assert_eq!(s[0].resets_at_ms, 0, "the year-9999 sentinel is not a reset date");
        assert_eq!(s[0].window_minutes, 0);
    }

    #[test]
    fn a_paid_plan_shows_its_credits_with_the_renewal_date() {
        let usage = json!({
            "userType": "personal_pro", "expiresAt": 1_790_784_000_000i64,
            "userQuota": {"total": 2000, "used": 1500.5, "remaining": 499.5, "percentage": 75.03, "unit": "credits"},
            "addOnQuota": {"total": 1500, "used": 1500, "remaining": 0, "percentage": 1}
        });
        let s = samples_from_usage(&usage);
        assert_eq!(s.len(), 2, "{s:?}");
        let plan = &s[0];
        assert_eq!(plan.label.as_deref(), Some("套餐内 · 已用 1500.5/2000"));
        assert!((plan.used_percent - 75.03).abs() < 1e-9);
        assert_eq!(plan.resets_at_ms, 1_790_784_000_000, "the plan row carries the renewal");
        let pack = &s[1];
        assert_eq!(pack.label.as_deref(), Some("资源包 · 已用 1500/1500"));
        assert_eq!(pack.used_percent, 100.0, "percentage 1 means a whole, not 1 %");
        assert_eq!(pack.resets_at_ms, 0, "no own reset is advertised for a pack");
    }

    #[test]
    fn a_missing_percentage_is_recomputed_not_taken_as_zero() {
        let usage = json!({"userQuota": {"total": 400, "used": 100, "unit": "credits"}});
        let s = samples_from_usage(&usage);
        assert_eq!(s.len(), 1);
        assert!((s[0].used_percent - 25.0).abs() < 1e-9, "{:?}", s[0]);
    }

    #[test]
    fn dedicated_and_shared_packs_are_named_the_way_the_panel_names_them() {
        let usage = json!({
            "userQuota": {"total": 100, "used": 0},
            "dedicatedResourcePackages": [
                {"name": "内测礼包", "total": 50, "used": 10, "remaining": 40, "available": true},
                {"name": "", "total": 30, "used": 0, "available": true},
                {"name": "过期包", "total": 20, "used": 0, "available": false},
                {"total": 20, "used": 0, "available": true}
            ],
            "orgResourcePackage": {"available": true, "used": 25, "cap": 100, "total": 100}
        });
        let s = samples_from_usage(&usage);
        let labels: Vec<&str> = s.iter().map(|x| x.label.as_deref().unwrap_or_default()).collect();
        assert_eq!(s.len(), 5, "{labels:?}");
        assert_eq!(labels[0], "套餐内 · 已用 0/100");
        assert!(labels.contains(&"内测礼包 · 已用 10/50"), "{labels:?}");
        assert!(labels.contains(&"专属资源包 · 已用 0/30"), "an unnamed pack still gets a bar: {labels:?}");
        assert!(!labels.iter().any(|l| l.contains("过期包")), "{labels:?} — unavailable is not drawn");
        assert!(labels.contains(&"共享资源包 · 已用 25/100"), "{labels:?}");
    }

    #[test]
    fn an_enterprise_account_or_a_broken_answer_is_silence() {
        let body: Value = serde_json::from_str(
            r#"{"displayMode":"enterprise","enterpriseUsage":{"openMode":"externalBrowser","detailUrl":"https://qoder.com/x"}}"#,
        )
        .unwrap();
        assert!(body.get("qoderUsage").is_none(), "the enterprise panel lives in a browser, not in bars");
        assert!(samples_from_usage(&Value::Null).is_empty());
        assert!(samples_from_usage(&json!({"userQuota": {"total": 0, "used": 0}})).is_empty());
    }

    #[test]
    fn the_snapshot_fallback_never_invents_an_exhausted_free_plan() {
        // The stale record this machine actually had: 0/0 with `isQuotaExceeded`.
        // It is the half-truth the live call replaced — plan credits exhausted,
        // resource pack invisible — and as a fallback it must stay silent too.
        let stale: Value = serde_json::from_str(
            r#"{"userQuota":{"total":0,"used":0,"remaining":0,"percentage":0,"unit":"credits"},
                "isQuotaExceeded":true,"expiresAt":253402214400000}"#,
        )
        .unwrap();
        assert!(samples_from_snapshot(&stale).is_empty(), "no bar can be drawn from 0/0");
        assert!(samples_from_snapshot(&Value::Null).is_empty());

        let paid: Value = serde_json::from_str(
            r#"{"usageType":"credits","expiresAt":1790784000000,
                "userQuota":{"total":2000,"used":1500.5,"remaining":499.5,"percentage":75.03,"unit":"credits"}}"#,
        )
        .unwrap();
        let s = samples_from_snapshot(&paid);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].label.as_deref(), Some("credits 1500.5/2000"));
        assert_eq!(s[0].resets_at_ms, 1_790_784_000_000);
    }

    #[test]
    fn an_expired_token_is_not_refreshed_and_not_used() {
        let info = json!({"token": "t", "expireTime": "1791071360000"});
        assert!(token_is_fresh(&info, 1_791_071_359_999));
        assert!(!token_is_fresh(&info, 1_791_071_360_000), "the stored string is epoch milliseconds");
        // Missing or unparsable expiry: unknown, so the live call may proceed.
        assert!(token_is_fresh(&json!({"token": "t"}), 1_791_071_360_000));
        assert!(token_is_fresh(&json!({"token": "t", "expireTime": "not-a-number"}), 0));
        // The 0.4 app's record names its expiry `expiresAt` as RFC 3339.
        let new = json!({"token": "t", "expiresAt": "2026-10-20T00:29:25Z"});
        let ms = chrono::DateTime::parse_from_rfc3339("2026-10-20T00:29:25Z").unwrap().timestamp_millis();
        assert!(token_is_fresh(&new, ms - 1));
        assert!(!token_is_fresh(&new, ms), "at the instant of expiry the token is spent");
    }

    /// The 0.4 app's record lives in `auth.v1.dat` beside its own profile dir
    /// and is opened by its own keychain passphrase — round-trip that shape.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_desktop_app_record_round_trips_through_its_own_file() {
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        let pass = "tokenme-test-passphrase";
        let body = r#"{"schemaVersion":1,"token":"fixture","expiresAt":"2026-10-20T00:29:25Z"}"#;
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(pass.as_bytes(), b"saltysalt", 1003, &mut key);
        let iv = [0x20u8; 16];
        let ct = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(body.as_bytes());
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&ct);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(AUTH_FILE), &blob).unwrap();
        let info = app_user_info(dir.path(), pass).expect("the app record opens");
        assert_eq!(info.get("token").and_then(Value::as_str), Some("fixture"));
        assert!(token_is_fresh(&info, 0));
        assert!(
            app_user_info(dir.path(), "wrong-pass").is_none_or(|v| v.get("token").is_none()),
            "another generation's passphrase must not open this record"
        );
        assert!(app_user_info(&dir.path().join("missing"), pass).is_none());
    }

    /// Round-trips the envelope: encrypt with the same scheme, read it back.
    #[test]
    fn the_envelope_is_symmetric() {
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        let pass = "tokenme-test-passphrase";
        let body = r#"{"userQuota":{"total":10,"used":4,"unit":"credits"}}"#;
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(pass.as_bytes(), b"saltysalt", 1003, &mut key);
        let iv = [0x20u8; 16];
        let ct = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(body.as_bytes());
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&ct);
        assert_eq!(decrypt(pass, &blob).as_deref(), Some(body));
        assert!(decrypt("wrong", &blob).is_none_or(|p| !p.starts_with('{')));
        assert!(decrypt(pass, b"short").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_dpapi_and_v10_profile_fixture_round_trip() {
        use aes_gcm::{
            aead::{Aead, KeyInit},
            Aes256Gcm, Nonce,
        };
        use base64::Engine;
        use std::fs;
        use tempfile::tempdir;

        let key = [0x42u8; 32];
        let nonce = [0x24u8; 12];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let mut auth = b"v10".to_vec();
        auth.extend_from_slice(&nonce);
        auth.extend_from_slice(
            &cipher
                .encrypt(
                    Nonce::from_slice(&nonce),
                    br#"{"token":"fixture-token"}"# as &[u8],
                )
                .unwrap(),
        );

        let mut wrapped_key = b"DPAPI".to_vec();
        wrapped_key.extend_from_slice(&dpapi_protect_for_test(&key));
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Local State"),
            serde_json::to_vec(&serde_json::json!({
                "os_crypt": {"encrypted_key": base64::engine::general_purpose::STANDARD.encode(wrapped_key)}
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(dir.path().join("auth.v1.dat"), auth).unwrap();

        let info =
            read_windows_user_info(dir.path()).expect("the Windows profile fixture decrypts");
        assert_eq!(
            info.get("token").and_then(Value::as_str),
            Some("fixture-token")
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "reads the installed Qoder profile but never prints its token"]
    fn the_installed_windows_qoder_profile_is_readable() {
        let Some(dir) = app_dirs()
            .into_iter()
            .find(|dir| dir.join(AUTH_FILE).is_file())
        else {
            println!("[qoder] no Windows auth.v1.dat found");
            return;
        };
        let info =
            read_windows_user_info(&dir).expect("the installed Qoder profile decrypts");
        assert!(info
            .get("token")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.is_empty()));
        println!("[qoder] Windows encrypted login record decoded successfully");
    }

    #[cfg(windows)]
    fn dpapi_protect_for_test(data: &[u8]) -> Vec<u8> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};

        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len().try_into().unwrap(),
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                &mut output,
            )
        };
        assert_ne!(ok, 0, "CryptProtectData failed");
        assert!(!output.pbData.is_null());
        let protected =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        unsafe {
            LocalFree(output.pbData.cast());
        }
        protected
    }

    #[test]
    #[ignore = "reads the real Qoder install, may prompt for keychain access, and calls the vendor"]
    fn the_live_meters_this_account_actually_has() {
        #[cfg(target_os = "macos")]
        for dir in app_dirs() {
            if !dir.join(AUTH_FILE).is_file() {
                continue;
            }
            println!("[qoder] desktop app profile: {dir:?}");
            for (service, account) in KEYCHAIN_IDS {
                let Some(pass) = keychain_pass(service, account) else {
                    println!("[qoder] no keychain item {service}/{account}");
                    continue;
                };
                let Some(info) = app_user_info(&dir, &pass) else {
                    println!("[qoder] {service}/{account} did not open the app record");
                    continue;
                };
                println!(
                    "[qoder] token {} bytes, fresh: {}",
                    info.get("token").and_then(Value::as_str).map(str::len).unwrap_or(0),
                    token_is_fresh(&info, chrono::Utc::now().timestamp_millis())
                );
                for s in live_usage(&info).unwrap_or_default() {
                    println!("[qoder] {:>6.2}%  reset={:>13}  {}", s.used_percent, s.resets_at_ms, s.label.as_deref().unwrap_or_default());
                }
            }
        }

        let dbs = state_dbs();
        println!("[qoder] state.vscdb candidates: {dbs:?}");
        let Some(db) = dbs.first() else {
            println!("[qoder] no Qoder profile on this machine -> probe answers nothing");
            return;
        };
        let conn = open(db).expect("the state db opens read-only");
        for (service, account) in KEYCHAIN_IDS {
            let Some(pass) = keychain_pass(service, account) else {
                println!("[qoder] no keychain item {service}/{account}");
                continue;
            };
            let Some(info) = read_secret(&conn, USER_KEY)
                .and_then(|b| decrypt(&pass, &b))
                .and_then(|p| parse(&p))
            else {
                println!("[qoder] {service}/{account} did not open the login record");
                continue;
            };
            println!(
                "[qoder] token {} bytes, fresh: {}",
                info.get("token").and_then(Value::as_str).map(str::len).unwrap_or(0),
                token_is_fresh(&info, chrono::Utc::now().timestamp_millis())
            );
            let samples = live_usage(&info).unwrap_or_default();
            for s in &samples {
                println!("[qoder] {:>6.2}%  reset={:>13}  {}", s.used_percent, s.resets_at_ms, s.label.as_deref().unwrap_or_default());
            }
            if samples.is_empty() {
                println!("[qoder] live meters answered nothing; snapshot fallback:");
                if let Some(v) = read_secret(&conn, QUOTA_KEY).and_then(|b| decrypt(&pass, &b)).and_then(|p| parse(&p)) {
                    println!("[qoder] {:?}", samples_from_snapshot(&v));
                }
            }
        }
    }
}

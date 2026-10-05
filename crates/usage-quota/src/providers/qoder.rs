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
                if let Some(samples) = live_usage(&info).filter(|s| !s.is_empty()) {
                    return samples;
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
                    if let Some(samples) = live_usage(&info).filter(|s| !s.is_empty()) {
                        return samples;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

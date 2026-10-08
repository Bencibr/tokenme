//! Trae (ByteDance's AI IDE): the account's own entitlement-usage endpoint,
//! opened with the login record the IDE keeps in its global storage.
//!
//! ## The call
//!
//! The IDE's usage panel polls exactly one endpoint per refresh (renderer log,
//! `ICubeUsageService`):
//!
//! ```text
//! POST {host}/trae/api/v1/pay/ide_user_ent_usage
//! headers: Content-Type: application/json
//!          Authorization: Cloud-IDE-JWT <token>
//! body:    {"require_usage": true}
//! ```
//!
//! `host` comes from the login record itself (`https://growsg-normal.trae.ai`
//! here), the v2 path answers 404 on this account and the shipped mapper reads
//! v1 — measured live 2026-09-30: HTTP 200 with the real pack list.
//!
//! ## The credential envelope
//!
//! The login record is `globalStorage/storage.json` under a key shaped
//! `iCubeAuthInfo://<provider-id>` (`icube.cloudide` here; the sibling
//! `icube-dc:<uid>` record is a device key pair and carries no token). The
//! value is base64 of the IDE's own `byteCrypto` envelope — no keychain and no
//! OS secret store involved, the key travels *inside* the blob:
//!
//! ```text
//! bytes [0..6)   header, picks the salt pair:
//!                  116,99,5,16,0,0    → plain AES   (salt = SALT_PLAIN)
//!                  18,57,32,32,2,3    → private AES (salt = SALT_PRIVATE)
//! bytes [6..38)  32-byte random key, embedded
//! bytes [38..)   AES-128-CBC( SHA-512(plaintext) || plaintext || PKCS7 )
//! aesKey = SHA512( SHA512(key) || salt )[0..16]
//! iv     = SHA512( SHA512(key) || salt )[16..32]
//! verified: SHA-512(plaintext) == the leading 64-byte digest
//! ```
//!
//! An expired token is a quiet miss, never a refresh: the record carries a
//! `refreshToken` and consuming a rotation without writing it back would log
//! the user out of their own IDE (the same rule the Qoder probe keeps).
//!
//! ## How a pack list becomes bars
//!
//! The shipped mapper (`U(e)` in the workbench bundle) picks the plan pack as
//! the first of Ultra(6) > ProPlus(4) > Pro(1) > Lite(8) > Free(0) and meters
//! it as `quota.basic_usage_limit` vs `usage.basic_usage_amount` (dollars when
//! `is_dollar_usage_billing`); a bonus meter follows unless `no_bonus_quota`.
//! Fast requests are summed **across packs** (`gb(e)`), where a limit of −1
//! means unlimited — an infinite meter draws no bar. Pay-as-you-go is uncapped
//! by construction, so its usage number gets no bar either. Meters with a
//! zero limit are the 0/0 rows the other probes also skip.
//!
//! Token-level usage has its own surface now: the adapter crate
//! `usage-adapter-trae` decrypts the agent's SQLCipher database and reads each
//! completed turn. This probe stays the spend/limit side — the plan's dollar
//! meters and fast requests that no local log carries. The host-exit pause
//! gates it like every mapped tool, with one wrinkle recorded in `crate::host`:
//! Trae's main binary ships as the generic "Electron", so the process match is
//! on its `Trae Helper*` children.

use std::path::PathBuf;

use serde_json::{json, Value};
use usage_core::QuotaSample;

use crate::QuotaProbe;

/// One probe per edition fleet: the international build and the CN build log
/// into different fleets, so each gets its own tool and its own windows.
#[derive(Debug, Clone, Copy)]
pub struct TraeQuota {
    /// `true` → tool `trae_cn`, reading the `Trae CN` / `TRAE SOLO CN` stores.
    pub cn: bool,
}

/// `globalStorage/storage.json` of every installed edition — the same three
/// the usage adapter reads. The editions log into different fleets (the CN
/// build's auth record carries its own host), so each file contributes its own
/// logins and the probe reports them side by side. `TRAE_STORAGE_JSON`
/// overrides the list for tests and side-by-side installs.
fn storage_jsons(cn: bool) -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("TRAE_STORAGE_JSON") {
        return (!path.is_empty()).then(|| PathBuf::from(path)).into_iter().collect();
    }
    let Some(base) = dirs::data_dir() else { return Vec::new() };
    let editions: &[&str] = if cn { &["Trae CN", "TRAE SOLO CN"] } else { &["Trae"] };
    editions
        .iter()
        .map(|edition| base.join(edition).join("User").join("globalStorage").join("storage.json"))
        .filter(|p| p.is_file())
        .collect()
}

/// The endpoint path the IDE calls (v1; the v2 variant 404s on this account).
const USAGE_PATH: &str = "/trae/api/v1/pay/ide_user_ent_usage";
/// Fallback API host, used when the login record carries none.
const DEFAULT_HOST: &str = "https://growsg-normal.trae.ai";
/// The current-entitlement-base listing: where the CN credits packs live
/// (`credits_limit` sits under `product_extra.package_extra.quota`, a key the
/// `ide_user_ent_usage` payloads do not carry).
const ENT_BASE_PATH: &str = "/trae/api/v1/pay/user_cur_ent_base";

/// `entitlement_base_info.product_type` values (the bundle's `Vc` enum). The
/// plan picker reads exactly these five; packs outside them are add-ons.
const PRODUCT_ULTRA: f64 = 6.0;
const PRODUCT_PRO_PLUS: f64 = 4.0;
const PRODUCT_PRO: f64 = 1.0;
const PRODUCT_LITE: f64 = 8.0;
const PRODUCT_FREE: f64 = 0.0;
/// Promo packs duplicate the plan's numbers and are excluded by the mapper.
const PRODUCT_PROMO: f64 = 3.0;

struct Login {
    token: String,
    host: String,
    /// `expiredAt` on the record, RFC 3339; `None` when absent or unreadable.
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Every `iCubeAuthInfo://` record across the installed editions that
/// decrypts to a token-bearing login. Records for other provider ids hold
/// device key pairs and simply fail the shape test; a token seen twice (the
/// same account logged into two editions) is probed once.
fn logins(cn: bool) -> Vec<Login> {
    let mut out: Vec<Login> = Vec::new();
    for path in storage_jsons(cn) {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(all) = serde_json::from_str::<serde_json::Map<String, Value>>(&text) else {
            continue;
        };
        for (key, value) in &all {
            let Some(encoded) = value.as_str() else { continue };
            if !key.starts_with("iCubeAuthInfo://") {
                continue;
            }
            let Some(plain) = decrypt(encoded) else { continue };
            let Ok(record) = serde_json::from_str::<Value>(&plain) else { continue };
            let Some(token) = record.get("token").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
                continue;
            };
            if out.iter().any(|l| l.token == token) {
                continue;
            }
            let expires_at = record
                .get("expiredAt")
                .and_then(Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc));
            let host = record
                .get("host")
                .and_then(Value::as_str)
                .filter(|h| h.starts_with("http"))
                .unwrap_or(DEFAULT_HOST)
                .trim_end_matches('/')
                .to_string();
            out.push(Login { token: token.to_string(), host, expires_at });
        }
    }
    out
}

/// The bundle's obfuscation tables, byte-for-byte from `byteCrypto.js`. The
/// salt is their element-wise XOR — the tables exist so the salt is not a
/// plain constant in the shipped binary.
const TABLE_PLAIN_A: [u8; 64] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57, 130,
    155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166, 194, 35, 61,
    238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73, 109,
    139, 209, 37,
];
const TABLE_PLAIN_B: [u8; 64] = [
    31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95, 96, 81, 127, 169, 25,
    181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160, 224, 59, 77, 174, 42, 245, 176, 200,
    235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99, 85, 33,
    12, 125,
];
const TABLE_PRIVATE_A: [u8; 64] = [
    191, 192, 216, 250, 122, 246, 220, 97, 31, 254, 98, 27, 8, 72, 71, 176, 135, 99, 96, 18, 127,
    101, 203, 104, 211, 102, 191, 125, 37, 72, 150, 156, 51, 229, 121, 35, 17, 153, 141, 177,
    110, 131, 150, 128, 172, 255, 254, 6, 18, 140, 55, 62, 236, 249, 135, 64, 135, 12, 117, 4,
    89, 149, 168, 209,
];
const TABLE_PRIVATE_B: [u8; 64] = [
    246, 204, 26, 232, 232, 70, 129, 109, 223, 146, 169, 242, 23, 241, 105, 145, 50, 196, 165,
    42, 254, 120, 3, 54, 244, 207, 209, 85, 53, 6, 138, 106, 175, 148, 31, 204, 186, 186, 165,
    182, 87, 142, 49, 10, 39, 110, 26, 154, 86, 56, 173, 125, 18, 64, 198, 225, 99, 99, 83, 82,
    191, 134, 76, 170,
];

fn salt_for(header: &[u8]) -> Option<[u8; 64]> {
    let (a, b) = match header {
        [116, 99, 5, 16, 0, 0] => (&TABLE_PLAIN_A, &TABLE_PLAIN_B),
        [18, 57, 32, 32, 2, 3] => (&TABLE_PRIVATE_A, &TABLE_PRIVATE_B),
        _ => return None,
    };
    Some(core::array::from_fn(|i| a[i] ^ b[i]))
}

/// Decrypt one base64 `byteCrypto` envelope. Every step is exactly the bundle's
/// `RBe`; a mismatch anywhere is `None`, not a guess.
fn decrypt(encoded: &str) -> Option<String> {
    use base64::Engine;
    use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
    use sha2::{Digest, Sha512};

    let blob = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    let salt = salt_for(blob.get(..6)?)?;
    let key = blob.get(6..38)?;
    let ct = blob.get(38..)?;
    if ct.is_empty() || ct.len() % 16 != 0 {
        return None;
    }

    let mut stem = [0u8; 128];
    stem[..64].copy_from_slice(&Sha512::digest(key));
    stem[64..].copy_from_slice(&salt[..]);
    let derived: [u8; 64] = Sha512::digest(stem).into();
    let key: [u8; 16] = derived[..16].try_into().ok()?;
    let iv: [u8; 16] = derived[16..32].try_into().ok()?;

    // `NoPadding`: the app's own padding comes off by hand below, because the
    // digest it must be verified against covers the *unpadded* plaintext.
    let mut plain = cbc::Decryptor::<aes::Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded_vec_mut::<NoPadding>(ct)
        .ok()?;
    // The app pads before encrypting and hashes the *unpadded* plaintext, so
    // the padding comes off before the digest check, never after.
    let pad = *plain.last()? as usize;
    if !(1..=16).contains(&pad)
        || plain.len() <= pad
        || plain[plain.len() - pad..].iter().any(|b| *b != pad as u8)
    {
        return None;
    }
    plain.truncate(plain.len() - pad);
    if plain.len() <= 64 {
        return None;
    }
    let (digest, body) = plain.split_at(64);
    if Sha512::digest(body).as_slice() != digest {
        return None;
    }
    String::from_utf8(body.to_vec()).ok()
}

impl QuotaProbe for TraeQuota {
    fn tool(&self) -> &'static str {
        if self.cn {
            "trae_cn"
        } else {
            "trae"
        }
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let now = chrono::Utc::now();
        for login in logins(self.cn) {
            // A past expiry means "no live sample"; the probe never refreshes.
            if login.expires_at.is_some_and(|t| t <= now) {
                continue;
            }
            let url = format!("{}{USAGE_PATH}", login.host);
            let Some(body) = crate::http::post_json(
                &url,
                &[
                    ("content-type", "application/json"),
                    ("authorization", &format!("Cloud-IDE-JWT {}", login.token)),
                    ("accept", "application/json"),
                ],
                json!({"require_usage": true}),
            ) else {
                continue;
            };
            let samples = samples_from_entitlement(&body);
            if !samples.is_empty() {
                return samples;
            }
            if self.cn {
                if let Some(credits) = cn_credits_window(&login) {
                    return vec![credits];
                }
            }
            if let Some(free) = cn_free_window(&body) {
                return vec![free];
            }
        }
        Vec::new()
    }
}

/// The CN credits window. Credits billing (`is_credits_billing: true`) meters
/// in credits carried per pack: `user_cur_ent_base` lists each pack's
/// `credits_limit` under `product_extra.package_extra.quota`, and the IDE sums
/// the same fields (`total += c`, remaining `c - credits_amount`). The
/// entitlement payloads read today carry no `credits_amount` yet — the spent
/// side reads 0 until one appears, exactly as the IDE's own `?? 0` falls back.
fn cn_credits_window(login: &Login) -> Option<QuotaSample> {
    let url = format!("{}{ENT_BASE_PATH}", login.host);
    let body = crate::http::post_json(
        &url,
        &[
            ("content-type", "application/json"),
            ("authorization", &format!("Cloud-IDE-JWT {}", login.token)),
        ],
        json!({}),
    )?;
    let list = body.get("ent_base_list").and_then(Value::as_array)?;
    let mut total = 0.0;
    let mut unlimited = false;
    let mut seen = false;
    for pack in list {
        match pack
            .pointer("/product_extra/package_extra/quota/credits_limit")
            .and_then(Value::as_f64)
        {
            Some(v) if v < 0.0 => unlimited = true,
            Some(v) if v > 0.0 => {
                total += v;
                seen = true;
            }
            _ => {}
        }
    }
    let label = if unlimited {
        format!("积分 · 已用 0/{}+不限量", fmt(total))
    } else {
        format!("积分 · 已用 0/{}", fmt(total))
    };
    if !seen && !unlimited {
        return None;
    }
    Some(QuotaSample {
        used_percent: 0.0,
        window_minutes: 0,
        resets_at_ms: 0,
        label: Some(label),
        id: Some("credits".to_string()),
    })
}

/// The CN free tier has no pack with a limit: the IDE maps a packless account
/// to its own Free table (50 basic requests a month — `pD.FreeUser` in the
/// workbench bundle) and the entitlement payload's `basic_usage_*` fields stay
/// at zero. The window is drawn from that table, with the payload's own basic
/// usage as the spent side, so a CN Free account shows a bar instead of
/// nothing.
fn cn_free_window(body: &Value) -> Option<QuotaSample> {
    let packs = body.get("user_entitlement_pack_list").and_then(Value::as_array)?;
    let has_limited_pack = packs.iter().any(|p| {
        num(p.pointer("/entitlement_base_info/quota/basic_usage_limit")).unwrap_or(0.0) > 0.0
    });
    if has_limited_pack {
        return None;
    }
    let used: f64 = packs
        .iter()
        .map(|p| num(p.get("usage").unwrap_or(&Value::Null).get("basic_usage_amount")).unwrap_or(0.0))
        .sum();
    const FREE_BASIC_LIMIT: f64 = 50.0;
    Some(QuotaSample {
        used_percent: (used / FREE_BASIC_LIMIT * 100.0).clamp(0.0, 100.0),
        window_minutes: 0,
        resets_at_ms: 0,
        label: Some(format!("Free plan · 已用 {}/50", fmt(used))),
        id: Some("basic_usage".to_string()),
    })
}

/// The wire answer → bars, in the mapper's own order: plan, bonus, fast.
pub(crate) fn samples_from_entitlement(body: &Value) -> Vec<QuotaSample> {
    let Some(list) = body.get("user_entitlement_pack_list").and_then(Value::as_array) else {
        return Vec::new();
    };
    let packs: Vec<&Value> = list
        .iter()
        .filter(|p| num(p.pointer("/entitlement_base_info/product_type")) != Some(PRODUCT_PROMO))
        .collect();
    if packs.is_empty() {
        return Vec::new();
    }
    let dollar = body.get("is_dollar_usage_billing").and_then(Value::as_bool).unwrap_or(false);

    let mut out = Vec::new();
    if let Some(pack) = plan_pack(&packs) {
        let quota = pack.pointer("/entitlement_base_info/quota").unwrap_or(&Value::Null);
        let usage = pack.get("usage").unwrap_or(&Value::Null);
        let identity = pack
            .get("display_desc")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "Trae".into());
        if let Some(bar) = meter(quota, usage, "basic_usage", &identity, dollar, resets_of(pack)) {
            out.push(bar);
        }
        // `no_bonus_quota` hides the bonus meter entirely, exactly as the mapper does.
        if quota.get("no_bonus_quota").and_then(Value::as_bool) != Some(true) {
            if let Some(bar) = meter(quota, usage, "bonus_usage", "Bonus", dollar, 0) {
                out.push(bar);
            }
        }
    }
    if let Some(bar) = fast_request_bar(&packs) {
        out.push(bar);
    }
    out
}

/// First of Ultra > ProPlus > Pro > Lite > Free, the mapper's own priority.
fn plan_pack<'a>(packs: &[&'a Value]) -> Option<&'a Value> {
    for kind in [PRODUCT_ULTRA, PRODUCT_PRO_PLUS, PRODUCT_PRO, PRODUCT_LITE, PRODUCT_FREE] {
        if let Some(pack) = packs
            .iter()
            .copied()
            .find(|p| num(p.pointer("/entitlement_base_info/product_type")) == Some(kind))
        {
            return Some(pack);
        }
    }
    None
}

/// One `X_limit` / `X_amount` pair as one bar. A meter without a positive
/// allowance is not a 0 % bar — it is nothing to draw.
fn meter(
    quota: &Value,
    usage: &Value,
    stem: &str,
    name: &str,
    dollar: bool,
    resets_at_ms: i64,
) -> Option<QuotaSample> {
    let limit = num(quota.get(format!("{stem}_limit")))?;
    let used = num(usage.get(format!("{stem}_amount"))).unwrap_or(0.0);
    if limit <= 0.0 {
        return None;
    }
    let spent = if dollar { format!("${}", fmt(used)) } else { fmt(used) };
    let cap = if dollar { format!("${}", fmt(limit)) } else { fmt(limit) };
    Some(QuotaSample {
        used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
        window_minutes: 0,
        resets_at_ms,
        label: Some(format!("{name} · 已用 {spent}/{cap}")),
        id: Some(stem.to_string()),
    })
}

/// Fast requests sum across packs; a single −1 (unlimited) cancels the bar.
fn fast_request_bar(packs: &[&Value]) -> Option<QuotaSample> {
    let mut limit = 0.0;
    let mut used = 0.0;
    for pack in packs {
        match num(pack.pointer("/entitlement_base_info/quota/premium_model_fast_request_limit")) {
            Some(-1.0) => return None,
            Some(n) => limit += n,
            None => {}
        }
        used += num(pack.pointer("/usage/premium_model_fast_amount")).unwrap_or(0.0);
    }
    if limit <= 0.0 {
        return None;
    }
    Some(QuotaSample {
        used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
        window_minutes: 0,
        resets_at_ms: packs.iter().map(|p| resets_of(p)).max().unwrap_or(0),
        label: Some(format!("Fast · 已用 {}/{}", fmt(used), fmt(limit))),
        id: Some("fast".into()),
    })
}

/// The pack's own period end: `expire_time` first, then the entitlement's
/// `end_time` — both epoch *seconds* on the wire.
fn resets_of(pack: &Value) -> i64 {
    let seconds = num(pack.get("expire_time"))
        .filter(|s| *s > 0.0)
        .or_else(|| num(pack.pointer("/entitlement_base_info/end_time")).filter(|s| *s > 0.0));
    seconds.map(|s| (s * 1000.0) as i64).unwrap_or(0)
}

fn num(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// Whole when whole, enough decimals to keep small dollars honest.
fn fmt(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else if n.abs() < 0.1 {
        format!("{n:.4}")
    } else {
        format!("{n:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// Round-trips the envelope: encrypt with the derived scheme, read back —
    /// and refuses a flipped byte, a wrong header.
    #[test]
    fn the_envelope_is_symmetric_and_resists_tampering() {
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        use sha2::{Digest, Sha512};

        let body = br#"{"token":"t","host":"https://x"}"#;
        // The wire plaintext: SHA-512 digest of the body, then the body.
        let mut prefixed = Sha512::digest(body).to_vec();
        prefixed.extend_from_slice(body);

        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let salt = salt_for(&[116, 99, 5, 16, 0, 0]).unwrap();
        let mut stem = [0u8; 128];
        stem[..64].copy_from_slice(&Sha512::digest(key));
        stem[64..].copy_from_slice(&salt);
        let derived: [u8; 64] = Sha512::digest(stem).into();
        let aes: [u8; 16] = derived[..16].try_into().unwrap();
        let iv: [u8; 16] = derived[16..32].try_into().unwrap();
        let ct = cbc::Encryptor::<aes::Aes128>::new(&aes.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(&prefixed);

        let mut blob = vec![116u8, 99, 5, 16, 0, 0];
        blob.extend_from_slice(&key);
        blob.extend_from_slice(&ct);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&blob);
        assert_eq!(decrypt(&encoded).as_deref(), Some(std::str::from_utf8(body).unwrap()));

        // A tampered ciphertext fails the digest check, a wrong header fails earlier.
        let mut flipped = blob.clone();
        flipped[50] ^= 0xff;
        let flipped = base64::engine::general_purpose::STANDARD.encode(&flipped);
        assert_eq!(decrypt(&flipped), None);
        let mut bad_header = blob;
        bad_header[0] = 1;
        let bad_header = base64::engine::general_purpose::STANDARD.encode(&bad_header);
        assert_eq!(decrypt(&bad_header), None);
    }

    /// The answer this machine's free account actually gave (2026-09-30),
    /// trimmed to the fields the probe reads.
    const MEASURED: &str = r#"{"billing_version":3,"is_dollar_usage_billing":true,
        "user_entitlement_pack_list":[{"display_desc":"Free plan","expire_time":0,
        "entitlement_base_info":{"end_time":1790812799,"product_type":0,
          "quota":{"advanced_model_request_limit":1000,"auto_completion_limit":5000,
            "basic_usage_limit":1,"bonus_usage_limit":0,"credits_limit":0,
            "no_bonus_quota":true,"premium_model_fast_request_limit":10,
            "premium_model_slow_request_limit":50},
          "product_extra":{"subscription_extra":{"period_type":0}}},
        "status":1,
        "usage":{"basic_usage_amount":0.00839,"bonus_usage_amount":0,"credits_amount":0,
          "is_flash_consuming":true}}]}"#;

    #[test]
    fn the_measured_free_plan_becomes_one_dollar_bar() {
        let body: Value = serde_json::from_str(MEASURED).unwrap();
        let s = samples_from_entitlement(&body);
        let labels: Vec<&str> = s.iter().map(|b| b.label.as_deref().unwrap_or_default()).collect();
        assert_eq!(
            labels,
            vec!["Free plan · 已用 $0.0084/$1", "Fast · 已用 0/10"],
            "{labels:?} — the plan's 10 fast requests are a real, empty meter"
        );
        let bar = &s[0];
        assert!((bar.used_percent - 0.839).abs() < 1e-9, "{}", bar.used_percent);
        assert_eq!(bar.id.as_deref(), Some("basic_usage"));
        // end_time 1790812799 s → ms; `expire_time: 0` must not win over it.
        assert_eq!(bar.resets_at_ms, 1_790_812_799_000);
    }

    #[test]
    fn a_paid_plan_draws_plan_bonus_and_fast() {
        let body: Value = serde_json::from_str(
            r#"{"is_dollar_usage_billing":false,"user_entitlement_pack_list":[
            {"display_desc":"Pro","expire_time":0,"entitlement_base_info":{"product_type":1,"end_time":1790812799,
              "quota":{"basic_usage_limit":600,"bonus_usage_limit":100}},
             "usage":{"basic_usage_amount":60.5,"bonus_usage_amount":10}},
            {"display_desc":"","expire_time":0,"entitlement_base_info":{"product_type":2,"end_time":1793404799,
              "quota":{"premium_model_fast_request_limit":80}},
             "usage":{"premium_model_fast_amount":8}}]}"#,
        )
        .unwrap();
        let s = samples_from_entitlement(&body);
        let labels: Vec<&str> = s.iter().map(|b| b.label.as_deref().unwrap_or_default()).collect();
        assert_eq!(labels, vec!["Pro · 已用 60.50/600", "Bonus · 已用 10/100", "Fast · 已用 8/80"], "{labels:?}");
        // The fast bar takes the latest pack end as its reset.
        assert_eq!(s[2].resets_at_ms, 1_793_404_799_000);
    }

    #[test]
    fn an_unlimited_or_absent_fast_meter_draws_nothing() {
        let unlimited: Value = serde_json::from_str(
            r#"{"user_entitlement_pack_list":[
            {"entitlement_base_info":{"product_type":1,"quota":{"basic_usage_limit":10,
               "premium_model_fast_request_limit":-1}},"usage":{"basic_usage_amount":1,
               "premium_model_fast_amount":999}}]}"#,
        )
        .unwrap();
        assert_eq!(samples_from_entitlement(&unlimited).len(), 1, "only the plan bar");
        let zero: Value = serde_json::from_str(
            r#"{"user_entitlement_pack_list":[
            {"entitlement_base_info":{"product_type":0,"quota":{"basic_usage_limit":0}},
             "usage":{"basic_usage_amount":0}}]}"#,
        )
        .unwrap();
        assert!(samples_from_entitlement(&zero).is_empty(), "0/0 is not a bar");
        assert!(samples_from_entitlement(&json!({"billing_version": 3})).is_empty());
    }

    /// A promo pack would double-count the plan's numbers; the mapper drops it.
    #[test]
    fn a_promo_pack_is_not_a_plan_and_not_a_fast_meter() {
        let body: Value = serde_json::from_str(
            r#"{"user_entitlement_pack_list":[
            {"entitlement_base_info":{"product_type":3,"quota":{"basic_usage_limit":600,
               "premium_model_fast_request_limit":500}},"usage":{"basic_usage_amount":1}}]}"#,
        )
        .unwrap();
        assert!(samples_from_entitlement(&body).is_empty());
    }

    /// Live proof on the machine that has the IDE; run with
    /// `cargo test -p usage-quota providers::trae -- --ignored --nocapture`.
    /// Prints shapes and counts, never the token itself.
    #[test]
    #[ignore = "reads the real Trae install and calls the vendor's live quota API"]
    fn the_live_meters_this_account_actually_has() {
        let paths = storage_jsons(false).into_iter().chain(storage_jsons(true)).collect::<Vec<_>>();
        if paths.is_empty() {
            println!("[trae] no platform data dir");
            return;
        }
        for path in &paths {
            println!("[trae] storage: {}", path.display());
        }
        let all: serde_json::Map<String, Value> = paths
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .filter_map(|t| serde_json::from_str(&t).ok())
            .fold(serde_json::Map::new(), |mut acc, m: serde_json::Map<String, Value>| {
                acc.extend(m);
                acc
            });
        let auth_keys: Vec<&String> = all.keys().filter(|k| k.starts_with("iCubeAuthInfo://")).collect();
        println!("[trae] iCubeAuthInfo records: {auth_keys:?}");
        for (key, value) in all.iter().filter(|(k, _)| k.starts_with("iCubeAuthInfo://")) {
            let Some(encoded) = value.as_str() else { continue };
            let Some(plain) = decrypt(encoded) else {
                println!("[trae] {key}: envelope did not decrypt");
                continue;
            };
            let record: Value = serde_json::from_str(&plain).unwrap();
            let token = record.get("token").and_then(Value::as_str).unwrap_or_default();
            println!(
                "[trae] {key}: token {} bytes, expiredAt {:?}, host {:?}",
                token.len(),
                record.get("expiredAt").and_then(Value::as_str),
                record.get("host").and_then(Value::as_str)
            );
            if token.is_empty() {
                continue;
            }
            let url = format!(
                "{}{USAGE_PATH}",
                record.get("host").and_then(Value::as_str).unwrap_or(DEFAULT_HOST)
            );
            let resp = ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(3))
                .timeout_read(std::time::Duration::from_secs(6))
                .build()
                .post(&url)
                .set("content-type", "application/json")
                .set("authorization", &format!("Cloud-IDE-JWT {token}"))
                .send_json(json!({"require_usage": true}));
            match resp {
                Ok(r) => {
                    let body: Value = r.into_json().unwrap();
                    println!("[trae] HTTP 200, keys: {:?}", body.as_object().map(|o| o.keys().collect::<Vec<_>>()));
                    let s = samples_from_entitlement(&body);
                    for bar in &s {
                        println!("[trae] {:>6.2}%  reset={:>13}  {}", bar.used_percent, bar.resets_at_ms, bar.label.as_deref().unwrap_or_default());
                    }
                    if s.is_empty() {
                        println!("[trae] mapper drew nothing from: {}", serde_json::to_string(&body).unwrap_or_default().chars().take(400).collect::<String>());
                    }
                }
                Err(ureq::Error::Status(code, r)) => {
                    println!("[trae] HTTP {code}: {}", r.into_string().unwrap_or_default().chars().take(200).collect::<String>());
                }
                Err(e) => println!("[trae] transport error: {e}"),
            }
        }
        // The exact production path, closing the loop past the hand-rolled call above.
        let fetched = TraeQuota { cn: false }.fetch();
        println!("[trae] fetch() → {} bars", fetched.len());
        for bar in &fetched {
            println!("[trae] fetch: {:>6.2}%  {}", bar.used_percent, bar.label.as_deref().unwrap_or_default());
        }
    }
}

/// `#[ignore]`d live diagnostic: which editions log in on this machine, and
/// with what host/expiry — the shape a CN-only install must produce for its
/// quota windows to appear. Never prints the token.
#[test]
#[ignore]
fn live_logins_report() {
    for path in storage_jsons(false).into_iter().chain(storage_jsons(true)) {
        println!("storage: {}", path.display());
    }
    for login in logins(false).into_iter().chain(logins(true)) {
        println!(
            "[trae] login: host {}  expiredAt {:?}",
            login.host, login.expires_at
        );
    }
    assert!(!logins(false).is_empty(), "no logins decrypt from the live stores");
}

/// `#[ignore]`d: dump the CN account's full entitlement payload (no token) so
/// the Free-plan window's mapping is written against the real shapes.
#[test]
#[ignore]
fn cn_entitlement_dump() {
    for login in logins(true).into_iter().filter(|l| l.host.contains("trae.cn")) {
        let url = format!("{}{USAGE_PATH}", login.host);
        let Some(body) = crate::http::post_json(
            &url,
            &[
                ("content-type", "application/json"),
                ("authorization", &format!("Cloud-IDE-JWT {}", login.token)),
            ],
            json!({}),
        ) else {
            println!("[trae-cn] {url}: request failed");
            continue;
        };
        println!("[trae-cn] {url}:\n{}", serde_json::to_string_pretty(&body).unwrap());
    }
}

/// `#[ignore]`d: the v2 entitlement endpoint takes `require_usage` and is what
/// the CN IDE itself calls for the free tier's usage numbers — probe it and
/// dump the shape.
#[test]
#[ignore]
fn cn_entitlement_v2_dump() {
    for login in logins(true).into_iter().filter(|l| l.host.contains("trae.cn")) {
        for path in ["/trae/api/v2/pay/ide_user_ent_usage", USAGE_PATH] {
            let url = format!("{}{path}", login.host);
            let Some(body) = crate::http::post_json(
                &url,
                &[
                    ("content-type", "application/json"),
                    ("authorization", &format!("Cloud-IDE-JWT {}", login.token)),
                ],
                json!({ "require_usage": true, "req_source": "IDE" }),
            ) else {
                println!("[trae-cn] {url}: request failed");
                continue;
            };
            println!("[trae-cn] {url}:\n{}", serde_json::to_string_pretty(&body).unwrap());
        }
    }
}

#[cfg(test)]
mod cn_free_tests {
    use super::*;

    /// The exact shape the live CN account returned (2026-10-08): a freshman
    /// account whose only pack carries no limits at all.
    #[test]
    fn a_packless_freshman_account_draws_the_free_table() {
        let body: Value = serde_json::json!({
            "is_dollar_usage_billing": false,
            "is_pay_freshman": true,
            "trial_status": {"is_eligible_for_trial": true, "is_in_trial": false, "trial_end_time": 1791627941},
            "user_entitlement_pack_list": [{
                "display_desc": "原速通次数兑换",
                "entitlement_base_info": {
                    "product_type": 2,
                    "quota": {"basic_usage_limit": 0, "premium_model_fast_request_limit": 0, "no_bonus_quota": true},
                    "product_id": 208
                },
                "usage": {"basic_usage_amount": 0, "premium_model_fast_amount": 0}
            }]
        });
        assert!(samples_from_entitlement(&body).is_empty(), "the paid-plan mapper has nothing for a 0-limit pack");
        let bar = cn_free_window(&body).expect("the free table draws instead");
        assert_eq!(bar.used_percent, 0.0);
        assert_eq!(bar.label.as_deref(), Some("Free plan · 已用 0/50"));
    }

    /// An account whose pack carries a real limit belongs to the paid mapper,
    /// not the free table.
    #[test]
    fn an_account_with_a_limited_pack_keeps_the_paid_mapper() {
        let body: Value = serde_json::json!({
            "is_dollar_usage_billing": false,
            "user_entitlement_pack_list": [{
                "entitlement_base_info": {"product_type": 1, "quota": {"basic_usage_limit": 500}},
                "usage": {"basic_usage_amount": 150}
            }]
        });
        assert!(cn_free_window(&body).is_none(), "the free table must not shadow a real pack");
    }
}

/// `#[ignore]`d live check: what the CN probe actually fetches right now.
#[test]
#[ignore]
fn cn_fetch_live() {
    let bars = TraeQuota { cn: true }.fetch();
    println!("[trae-cn] fetch() → {} bars", bars.len());
    for bar in &bars {
        println!("[trae-cn] fetch: {:>6.2}%  {}", bar.used_percent, bar.label.as_deref().unwrap_or_default());
    }
    assert!(!bars.is_empty(), "the CN probe returned no bars for a running host");
}

/// `#[ignore]`d: the CN dashboard's credits endpoints against the local token
/// — status, billing status and the web entitlement (which the dashboard page
/// itself calls). Dumps shapes, never the token.
#[test]
#[ignore]
fn cn_credits_dump() {
    for login in logins(true).into_iter().filter(|l| l.host.contains("trae.cn")) {
        for (path, payload) in [
            ("/trae/api/v1/pay/query_user_usage_group_by_session",
             json!({ "start_time": 1791187200000i64, "end_time": 1791878399000i64, "page_size": 100, "page_num": 1, "usage_type": 0 })),
            ("/trae/api/v1/pay/query_user_usage_group_by_session",
             json!({ "start_time": 1791187200000i64, "end_time": 1791878399000i64, "page_size": 100, "page_num": 1, "usage_type": 1 })),
        ] {
            let url = format!("{}{path}", login.host);
            let resp = ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(5))
                .timeout_read(std::time::Duration::from_secs(10))
                .build()
                .post(&url)
                .set("content-type", "application/json")
                .set("authorization", &format!("Cloud-IDE-JWT {}", login.token))
                .send_string(&payload.to_string());
            match resp {
                Ok(r) => {
                    let text = r.into_string().unwrap_or_default();
                    let pretty: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    println!("[trae-cn] {url} ({}):\n{}", "2xx", serde_json::to_string_pretty(&pretty).unwrap());
                }
                Err(ureq::Error::Status(code, r)) => {
                    let text = r.into_string().unwrap_or_default();
                    println!("[trae-cn] {url}: HTTP {code}: {text}");
                }
                Err(e) => println!("[trae-cn] {url}: {e}"),
            }
        }
    }
}

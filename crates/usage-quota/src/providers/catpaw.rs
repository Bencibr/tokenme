//! 妙手 (the IDE formerly branded CatPaw) 的积分余额,走积分门户自己的接口。
//!
//! ## Credential
//!
//! 设置页的"妙手积分"只是一个跳转链接(`credit.catpaw.meituan.com` 的 SSO
//! 网页),IDE 从不缓存余额。门户认的是 passport 会话 token,两代凭据都读:
//!
//! - **新版(妙手)**:`%APPDATA%/catpaw-moon/catx-credential.json` 的
//!   `ssoTokenEnc` —— `base64(iv[12] ‖ tag[16] ‖ ciphertext)`,AES-256-GCM,
//!   key = `sha256(machineId + ":catpaw-desk-token-v2")`(与 IDE 自己的解密
//!   逐字节一致,machineId 是 node-machine-id 的原始 id:Windows 注册表
//!   `MachineGuid`、macOS `IOPlatformUUID`)。实测该 token 配
//!   `Cookie: mt_c_token=<token>` 仍被门户接受(200 / code 0)。
//! - **旧版(CatPaw)**:`state.vscdb`(ItemTable 键 `catpaw.mt-authentication`,
//!   双重编码的 JSON)里 `sessions[0].accessToken` —— 作后备,装着未升级
//!   IDE 的机器从这里拿 token。
//!
//! `code != 0`(登录失效)是"无样本",不是错误——与 Qoder 探针同一规则。

use std::path::PathBuf;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use usage_core::QuotaSample;

use crate::http::get_json;
use crate::QuotaProbe;

pub struct CatpawQuota;

const BALANCE_URL: &str = "https://credit.catpaw.meituan.com/api/credit/balance";
/// 美团积分门户认可的会话 cookie 名;值就是 IDE 的 passport accessToken。
const TOKEN_COOKIE: &str = "mt_c_token";
/// 积分按月发放:一个自然月,配额条据此显示"月窗口"。
const WINDOW_MINUTES: i64 = 43_200;

impl QuotaProbe for CatpawQuota {
    fn tool(&self) -> &'static str {
        "catpaw"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(token) = read_token() else { return Vec::new() };
        let Some(body) = get_json(
            BALANCE_URL,
            &[
                ("Cookie", &format!("{TOKEN_COOKIE}={token}")),
                ("Content-Type", "application/json"),
            ],
        ) else {
            return Vec::new();
        };
        samples_from_balance(&body)
    }
}

// ---------------------------------------------------------------- credential

/// The renamed IDE's credential file; `CATPAW_CREDENTIAL_FILE` overrides it
/// for tests and side-by-side installs.
fn credential_file() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CATPAW_CREDENTIAL_FILE") {
        return Some(PathBuf::from(path));
    }
    dirs::data_dir().map(|d| d.join("catpaw-moon").join("catx-credential.json"))
}

/// The machine id that keys the IDE's token encryption — the raw id
/// node-machine-id feeds its own hash (Windows registry `MachineGuid`, macOS
/// `IOPlatformUUID`, Linux dbus/system machine id). `CATPAW_MACHINE_ID`
/// overrides it for tests.
fn machine_id() -> Option<String> {
    if let Some(id) = std::env::var_os("CATPAW_MACHINE_ID") {
        return Some(id.to_string_lossy().into_owned());
    }
    machine_id_of_platform()
}

#[cfg(windows)]
fn machine_id_of_platform() -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let subkey: Vec<u16> = r"SOFTWARE\Microsoft\Cryptography"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name: Vec<u16> = "MachineGuid".encode_utf16().chain(std::iter::once(0)).collect();
    let mut value = [0u16; 128];
    let mut bytes = std::mem::size_of_val(&value) as u32;
    let mut kind = 0;
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            &mut kind,
            value.as_mut_ptr() as _,
            &mut bytes,
        )
    };
    if result != 0 {
        return None;
    }
    let chars = (bytes as usize / std::mem::size_of::<u16>()).min(value.len());
    let value = String::from_utf16_lossy(&value[..chars]).trim_end_matches('\0').trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(target_os = "macos")]
fn machine_id_of_platform() -> Option<String> {
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.contains("IOPlatformUUID"))?;
    let uuid = line.split('"').nth(3)?.trim().to_string();
    (!uuid.is_empty()).then_some(uuid)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn machine_id_of_platform() -> Option<String> {
    for path in ["/var/lib/dbus/machine-id", "/etc/machine-id"] {
        if let Ok(id) = std::fs::read_to_string(path) {
            let id = id.trim().to_string();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

/// The token file's AES key, verbatim from the IDE's own decrypt: the machine
/// id (empty when unavailable, exactly as the IDE falls back) salted and
/// hashed.
fn token_key(machine: Option<&str>) -> [u8; 32] {
    Sha256::digest(format!("{}:catpaw-desk-token-v2", machine.unwrap_or_default())).into()
}

/// `ssoTokenEnc` → `{"access_token": …}` → the passport token. Any failure
/// reads as "no credential": a rotated key, a changed layout, or a truncated
/// file all end the same way, with the probe silent rather than the panel
/// wrong.
fn token_from_credential_file() -> Option<String> {
    let raw = std::fs::read_to_string(credential_file()?).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let sealed = parsed.get("ssoTokenEnc")?.as_str()?;
    let blob = base64::engine::general_purpose::STANDARD.decode(sealed.trim()).ok()?;
    if blob.len() < 12 + 16 + 1 {
        return None;
    }
    let (iv, rest) = blob.split_at(12);
    let (tag, body) = rest.split_at(16);
    // `aead` wants the tag appended to the ciphertext; the envelope keeps
    // them apart (same layout as the cola one, different key source).
    let mut payload = body.to_vec();
    payload.extend_from_slice(tag);
    let plain = Aes256Gcm::new_from_slice(&token_key(machine_id().as_deref()))
        .ok()?
        .decrypt(Nonce::from_slice(iv), payload.as_ref())
        .ok()?;
    let opened: Value = serde_json::from_slice(&plain).ok()?;
    let token = opened
        .get("access_token")
        .or_else(|| opened.get("ssoToken"))
        .and_then(Value::as_str)?;
    (!token.is_empty()).then(|| token.to_string())
}

/// The IDE's pre-rename global state db; `CATPAW_STATE_DB` overrides it for
/// tests and side-by-side installs.
fn state_db() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CATPAW_STATE_DB") {
        return Some(PathBuf::from(path));
    }
    dirs::data_dir()
        .map(|d| d.join("CatPawAI/User/globalStorage/state.vscdb"))
        .filter(|p| p.is_file())
}

/// The renamed IDE first, the pre-rename one second — an upgraded machine
/// keeps both, and only the new file carries a live token.
fn read_token() -> Option<String> {
    token_from_credential_file().or_else(|| {
        let conn = Connection::open_with_flags(state_db()?, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
        read_token_from_state_db(&conn)
    })
}

/// `state.vscdb` → `catpaw.mt-authentication` → `{"mt.auth": "<json string>"}`,
/// where the inner JSON is `{"sessions": [{"accessToken": …}]}`. A missing db,
/// a missing key or a second login row all degrade to "no token".
fn read_token_from_state_db(conn: &Connection) -> Option<String> {
    let raw: Option<String> = conn
        .prepare("SELECT value FROM ItemTable WHERE key = 'catpaw.mt-authentication'")
        .ok()?
        .query_row([], |row| row.get(0))
        .optional()
        .ok()
        .flatten();
    let outer: Value = serde_json::from_str(&raw?).ok()?;
    let inner: Value = serde_json::from_str(outer.get("mt.auth")?.as_str()?).ok()?;
    let token = inner
        .get("sessions")?
        .get(0)?
        .get("accessToken")?
        .as_str()?
        .to_string();
    (!token.is_empty()).then_some(token)
}

// ------------------------------------------------------------------- mapping

/// The balance answer is one plan pot: used = total − available.
fn samples_from_balance(body: &Value) -> Vec<QuotaSample> {
    let mut out = Vec::new();
    if body.get("code").and_then(Value::as_i64) != Some(0) {
        return out;
    }
    let Some(data) = body.get("data") else { return out };
    let num = |k: &str| data.get(k).and_then(Value::as_str).and_then(|s| s.parse::<f64>().ok());
    let (Some(total), Some(available)) = (num("totalCredits"), num("availableCredits")) else {
        return out;
    };
    if total <= 0.0 {
        return out;
    }
    let used = ((total - available) / total * 100.0).clamp(0.0, 100.0);
    out.push(QuotaSample {
        used_percent: used,
        window_minutes: WINDOW_MINUTES,
        resets_at_ms: 0,
        label: Some(format!("套餐积分 · 已用 {}/{}", trim(total - available), trim(total))),
        id: Some("credits".into()),
    });
    out
}

/// 1200.00 → 1200, 58.5 → 58.5 — the label reads like a count, not a ledger.
fn trim(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The token tests pin process-global env overrides; serialize them.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn maps_the_real_balance_answer() {
        let body = json!({"code": 0, "message": "success", "data": {
            "userId": 61041752, "totalCredits": "1200.00", "availableCredits": "1200.00",
            "frozenCredits": "0.00", "expiredCredits": "0.00", "version": 0}});
        let s = samples_from_balance(&body);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].used_percent, 0.0);
        assert_eq!(s[0].label.as_deref(), Some("套餐积分 · 已用 0/1200"));
        assert_eq!(s[0].id.as_deref(), Some("credits"));
        assert_eq!(s[0].window_minutes, 43_200, "月窗口");
        assert_eq!(s[0].resets_at_ms, 0, "无本地可见的重置时间");
    }

    #[test]
    fn partial_usage_is_the_spent_share() {
        let body = json!({"code": 0, "data": {"totalCredits": "1200.00", "availableCredits": "300.00"}});
        let s = samples_from_balance(&body);
        assert_eq!(s[0].used_percent, 75.0);
        assert_eq!(s[0].label.as_deref(), Some("套餐积分 · 已用 900/1200"));
    }

    #[test]
    fn logout_shape_and_garbage_are_silence() {
        assert!(samples_from_balance(&json!({"data": {"message": "未登录或登录失效，需要先登录"}, "code": 401})).is_empty());
        assert!(samples_from_balance(&json!({"code": 0})).is_empty());
        assert!(samples_from_balance(&json!({"code": 0, "data": {"totalCredits": "0.00", "availableCredits": "0.00"}})).is_empty(),
            "0/0 不是一条 100% 的条(与 Qoder 同规则)");
    }

    #[test]
    fn token_comes_out_of_a_fixture_db() {
        let _env = env_lock();
        let dir = tempfile::tempdir().unwrap();
        // The renamed IDE's credential file would win over this fixture if it
        // were reachable; every test pins both overrides.
        let no_credential = dir.path().join("absent-credential.json");
        std::env::set_var("CATPAW_CREDENTIAL_FILE", &no_credential);
        let db = dir.path().join("state.vscdb");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO ItemTable VALUES ('catpaw.mt-authentication', '{\"mt.auth\": \"{\\\"sessions\\\":[{\\\"accessToken\\\":\\\"tok-1\\\"}]}\"}');
             INSERT INTO ItemTable VALUES ('unrelated', 'x');",
        )
        .unwrap();
        drop(conn);
        std::env::set_var("CATPAW_STATE_DB", &db);
        assert_eq!(read_token().as_deref(), Some("tok-1"));
        // A db without the key is silence, not an error.
        let empty = dir.path().join("empty.vscdb");
        let conn = Connection::open(&empty).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);").unwrap();
        drop(conn);
        std::env::set_var("CATPAW_STATE_DB", &empty);
        assert_eq!(read_token(), None);
        std::env::remove_var("CATPAW_STATE_DB");
        std::env::remove_var("CATPAW_CREDENTIAL_FILE");
    }

    /// The renamed IDE's envelope: `base64(iv ‖ tag ‖ ciphertext)`, key =
    /// sha256(machineId + ":catpaw-desk-token-v2") — opened exactly the way
    /// the IDE opens it (the golden bytes here are sealed by the same aes-gcm
    /// construction the reader uses).
    #[test]
    fn token_comes_out_of_the_renamed_credential_file() {
        use aes_gcm::aead::Aead;

        let _env = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let credential = dir.path().join("catx-credential.json");
        let key = token_key(Some("test-machine"));
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let iv = [7u8; 12];
        let sealed = cipher
            .encrypt(Nonce::from_slice(&iv), br#"{"access_token":"tok-new","modified_at":1}"#.as_ref())
            .unwrap();
        // The envelope keeps the tag in front; the cipher appends it.
        let (body, tag) = sealed.split_at(sealed.len() - 16);
        let mut blob = iv.to_vec();
        blob.extend_from_slice(tag);
        blob.extend_from_slice(body);
        std::fs::write(
            &credential,
            format!("{{\n\t\"ssoTokenEnc\": \"{}\"\n}}", base64::engine::general_purpose::STANDARD.encode(&blob)),
        )
        .unwrap();
        std::env::set_var("CATPAW_CREDENTIAL_FILE", &credential);
        std::env::set_var("CATPAW_MACHINE_ID", "test-machine");
        assert_eq!(read_token().as_deref(), Some("tok-new"));
        // A wrong machine id (or a truncated blob) is silence, not an error.
        std::env::set_var("CATPAW_MACHINE_ID", "other-machine");
        assert_eq!(read_token(), None);
        std::env::remove_var("CATPAW_CREDENTIAL_FILE");
        std::env::remove_var("CATPAW_MACHINE_ID");
    }

    /// The renamed file wins over the legacy db when both exist — an upgraded
    /// machine keeps both, and only the new token is live.
    #[test]
    fn the_renamed_credential_wins_over_the_legacy_db() {
        use aes_gcm::aead::Aead;

        let _env = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let credential = dir.path().join("catx-credential.json");
        let cipher = Aes256Gcm::new_from_slice(&token_key(Some("test-machine"))).unwrap();
        let sealed = cipher
            .encrypt(Nonce::from_slice(&[7u8; 12]), br#"{"access_token":"tok-new"}"#.as_ref())
            .unwrap();
        let (body, tag) = sealed.split_at(sealed.len() - 16);
        let mut blob = [7u8; 12].to_vec();
        blob.extend_from_slice(tag);
        blob.extend_from_slice(body);
        std::fs::write(
            &credential,
            format!("{{\"ssoTokenEnc\": \"{}\"}}", base64::engine::general_purpose::STANDARD.encode(&blob)),
        )
        .unwrap();
        let db = dir.path().join("state.vscdb");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO ItemTable VALUES ('catpaw.mt-authentication', '{\"mt.auth\": \"{\\\"sessions\\\":[{\\\"accessToken\\\":\\\"tok-old\\\"}]}\"}');",
        )
        .unwrap();
        drop(conn);
        std::env::set_var("CATPAW_CREDENTIAL_FILE", &credential);
        std::env::set_var("CATPAW_MACHINE_ID", "test-machine");
        std::env::set_var("CATPAW_STATE_DB", &db);
        assert_eq!(read_token().as_deref(), Some("tok-new"));
        // With the renamed file unreadable, the legacy db answers.
        std::env::set_var("CATPAW_CREDENTIAL_FILE", dir.path().join("absent.json"));
        assert_eq!(read_token().as_deref(), Some("tok-old"));
        std::env::remove_var("CATPAW_CREDENTIAL_FILE");
        std::env::remove_var("CATPAW_MACHINE_ID");
        std::env::remove_var("CATPAW_STATE_DB");
    }
}

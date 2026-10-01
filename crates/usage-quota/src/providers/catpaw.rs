//! CatPaw (美团内部 IDE) 的积分余额,走积分门户自己的接口。
//!
//! ## Credential
//!
//! 设置页的"CatPaw 积分"只是一个跳转链接(`credit.catpaw.meituan.com` 的 SSO
//! 网页),IDE 从不缓存余额。但门户认的正是 IDE 已有的 passport 会话
//! token——`state.vscdb`(ItemTable 键 `catpaw.mt-authentication`,双重编码的
//! JSON)里 `sessions[0].accessToken`,以 `Cookie: mt_c_token=<token>` 的身份
//! 发送即可(Chrome 里该 cookie 的解密值与它逐字节一致,实测 200)。
//!
//! ## The call
//!
//! `GET https://credit.catpaw.meituan.com/api/credit/balance` →
//! `{"code":0,"data":{"totalCredits":"1200.00","availableCredits":"1200.00",
//! "frozenCredits":"0.00","expiredCredits":"0.00"}}`。积分按月发放、无本地
//! 可见的重置时间,所以 `resets_at_ms` 为 0、窗口按自然月计(43200 分钟)。
//! `code != 0`(登录失效)是"无样本",不是错误——与 Qoder 探针同一规则。

use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
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

/// The IDE's global state db; `CATPAW_STATE_DB` overrides it for tests and
/// side-by-side installs.
fn state_db() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CATPAW_STATE_DB") {
        return Some(PathBuf::from(path));
    }
    dirs::data_dir()
        .map(|d| d.join("CatPawAI/User/globalStorage/state.vscdb"))
        .filter(|p| p.is_file())
}

/// `state.vscdb` → `catpaw.mt-authentication` → `{"mt.auth": "<json string>"}`,
/// where the inner JSON is `{"sessions": [{"accessToken": …}]}`. A missing db,
/// a missing key or a second login row all degrade to "no token".
fn read_token() -> Option<String> {
    let conn = Connection::open_with_flags(state_db()?, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
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
        let dir = tempfile::tempdir().unwrap();
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
    }
}

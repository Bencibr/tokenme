//! FunIDE: the cloud points balance behind the IDE's GLM-plan models.
//!
//! ## Where the numbers live
//!
//! The FunIDE cloud (`https://fun.quqimeng.com/ide/api/v1`) bills its plan
//! models in points, and the account state is reachable with the IDE's own
//! access token — stored *in plain text* (a JWT) in the VS Code secret table:
//! `<data>/FunIDE/User/globalStorage/state.vscdb`, ItemTable key
//! `funide.cloud.accessToken`. Measured live 2026-09-29:
//!
//! ```text
//! GET /ide/api/v1/billing/balance   (Bearer <jwt>)
//! → {"ok":true,"data":{"points":10.04,"total_consumed":0.056,…}}
//! ```
//!
//! The token is short-lived (2 h, refreshed by the IDE); an expired or absent
//! token answers `Vec::new()` — opening FunIDE refreshes it and the probe
//! resumes on a later pass.
//!
//! ## How a points balance becomes a bar
//!
//! Points are pay-as-you-go with check-in grants on top of recharges, so the
//! bar reads against the account's own turnover: used = consumed /
//! (remaining + consumed). The label carries the remaining figure.

use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use usage_core::QuotaSample;

use crate::{http::get_json, QuotaProbe};

const BALANCE_URL: &str = "https://fun.quqimeng.com/ide/api/v1/billing/balance";
const TOKEN_KEY: &str = "funide.cloud.accessToken";

pub struct FunIdeQuota;

impl QuotaProbe for FunIdeQuota {
    fn tool(&self) -> &'static str {
        "funide"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(token) = access_token() else { return Vec::new() };
        let authorization = format!("Bearer {token}");
        let Some(body) = get_json(
            BALANCE_URL,
            &[("authorization", authorization.as_str()), ("accept", "application/json")],
        ) else {
            return Vec::new();
        };
        sample_from(&body).into_iter().collect()
    }
}

/// `<data>/FunIDE/User/globalStorage/state.vscdb`, then the plaintext token.
/// `$FUNIDE_DATA_DIR` is tokenme's override; the IDE documents none.
fn access_token() -> Option<String> {
    let path = state_db()?;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let value: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            rusqlite::params![TOKEN_KEY],
            |r| r.get(0),
        )
        .ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn state_db() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FUNIDE_DATA_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        let p = dir.join("User").join("globalStorage").join("state.vscdb");
        return p.is_file().then_some(p);
    }
    let base = dirs::data_dir()?;
    let p = base
        .join("FunIDE")
        .join("User")
        .join("globalStorage")
        .join("state.vscdb");
    p.is_file().then_some(p)
}

/// `ok:true` gates the answer; the bar reads against the account's turnover
/// (points + consumed), since check-in grants sit on top of recharges.
fn sample_from(body: &Value) -> Option<QuotaSample> {
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let data = body.get("data")?;
    let num = |k: &str| {
        data.get(k)
            .and_then(Value::as_f64)
            .or_else(|| data.get(k)?.as_str()?.trim().parse::<f64>().ok())
            .unwrap_or(0.0)
            .max(0.0)
    };
    let points = num("points");
    let consumed = num("total_consumed");
    if points + consumed <= 0.0 {
        return None;
    }
    let used_percent = (consumed / (points + consumed) * 100.0).clamp(0.0, 100.0);
    Some(QuotaSample {
        used_percent,
        window_minutes: 0,
        resets_at_ms: 0,
        label: Some(format!("积分 {points:.2}")),
        id: Some("funide-points".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r#"{"ok":true,"data":{"points":10.043741,"frozen_points":0,
        "total_recharged":0.1,"total_consumed":0.056259,"points_per_yuan":1,
        "billing_enabled":true}}"#;

    #[test]
    fn the_points_balance_becomes_a_bar_against_turnover() {
        let body: Value = serde_json::from_str(LIVE).unwrap();
        let s = sample_from(&body).expect("a funded account has a bar");
        assert_eq!(s.label.as_deref(), Some("积分 10.04"));
        assert!((s.used_percent - 0.56).abs() < 0.01, "0.056 of 10.10 turnover: {s:?}");
        assert_eq!(s.window_minutes, 0, "points are pay-as-you-go, no window");
        assert_eq!(s.id.as_deref(), Some("funide-points"));
    }

    #[test]
    fn failures_answer_nothing() {
        for raw in [
            r#"{"ok":false,"error":{"code":"AUTH"}}"#,
            r#"{"ok":true,"data":{"points":0,"total_consumed":0}}"#,
            "not json",
        ] {
            let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            assert!(sample_from(&v).is_none(), "{raw}");
        }
    }
}

//! CC Switch: the budgets the gateway itself enforces.
//!
//! No network and no vendor endpoint: the proxy stores spend limits per provider
//! in its own database, in the two columns the app writes when a user sets a
//! 每日/每月 上限 (`providers.limit_daily_usd`, `providers.limit_monthly_usd`).
//!
//! The spent side of the ratio comes from `proxy_request_logs`, summed per
//! `provider_id` since the local midnight / first of the month. That is
//! deliberately the *gateway's* money rather than tokenme's own price table:
//! the limit is enforced against `total_cost_usd`, so any other number would
//! answer a different question. `created_at` is in seconds.
//!
//! Answering nothing is the normal case — this machine has 64 providers and not
//! one cap set. An absent bar beats a 0 % bar that implies an unlimited plan.

use std::path::Path;

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use usage_core::QuotaSample;

use crate::QuotaProbe;

pub struct CcSwitchQuota;

const WINDOW_DAILY: i64 = 1_440;
const WINDOW_MONTHLY: i64 = 43_200;

impl QuotaProbe for CcSwitchQuota {
    fn tool(&self) -> &'static str {
        "ccswitch"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        let Some(db) = db_path() else { return Vec::new() };
        // Never `immutable=1`: the gateway keeps this database in WAL mode while
        // serving traffic, and skipping the wal-index loses uncheckpointed rows.
        let Ok(conn) = Connection::open_with_flags(
            read_only_uri(&db),
            OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) else {
            return Vec::new();
        };
        samples(&conn, &Utc::now())
    }
}

/// Both windows, in display order. A database missing either table yields nothing.
pub(crate) fn samples(conn: &Connection, now: &DateTime<Utc>) -> Vec<QuotaSample> {
    let mut out = window(conn, now, "limit_daily_usd", WINDOW_DAILY, "日", day_start(now));
    out.extend(window(conn, now, "limit_monthly_usd", WINDOW_MONTHLY, "月", month_start(now)));
    out.sort_by(|a, b| a.window_minutes.cmp(&b.window_minutes).then_with(|| {
        b.used_percent.total_cmp(&a.used_percent)
    }));
    out
}

fn window(
    conn: &Connection,
    now: &DateTime<Utc>,
    column: &str,
    minutes: i64,
    what: &str,
    since_s: i64,
) -> Vec<QuotaSample> {
    // `CAST` because a settings column a user edited by hand can hold anything,
    // and one unparseable row must not cost us every provider's bar.
    let sql = format!(
        "SELECT id, name, CAST({column} AS REAL) FROM providers WHERE CAST({column} AS REAL) > 0"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else { return Vec::new() };
    let Ok(rows) = stmt.query_map([], |r| {
        let id: String = r.get(0)?;
        let name: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
        Ok((id.clone(), if name.trim().is_empty() { id } else { name }, r.get::<_, f64>(2)?))
    }) else {
        return Vec::new();
    };

    let reset_ms = if minutes == WINDOW_DAILY {
        (day_start_dt(now) + chrono::Duration::days(1)).timestamp_millis()
    } else {
        next_month_start_dt(now).timestamp_millis()
    };
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (id, name, limit) = row;
        let Some(spent) = spent(conn, &id, since_s) else { continue };
        out.push(QuotaSample {
            used_percent: (spent / limit * 100.0).clamp(0.0, 100.0),
            window_minutes: minutes,
            resets_at_ms: reset_ms,
            label: Some(format!("{name} · {what} ${spent:.2}/${limit:.0}")),
            id: Some(format!("{id}-{what}")),
        });
    }
    out
}

/// What the gateway charged one provider since `since_s`, in its own USD.
fn spent(conn: &Connection, provider: &str, since_s: i64) -> Option<f64> {
    conn.query_row(
        "SELECT COALESCE(SUM(CAST(total_cost_usd AS REAL)), 0) FROM proxy_request_logs \
         WHERE provider_id = ?1 AND created_at >= ?2",
        rusqlite::params![provider, since_s],
        |r| r.get::<_, f64>(0),
    )
    .ok()
}

fn day_start(now: &DateTime<Utc>) -> i64 {
    day_start_dt(now).timestamp()
}

fn month_start(now: &DateTime<Utc>) -> i64 {
    month_start_dt(now).timestamp()
}

/// The cap lifts at the first of the *next* month, not when this one opened.
fn next_month_start_dt(now: &DateTime<Utc>) -> DateTime<Local> {
    let local = now.with_timezone(&Local);
    let (y, m) = if local.month() == 12 { (local.year() + 1, 1) } else { (local.year(), local.month() + 1) };
    NaiveDate::from_ymd_opt(y, m, 1)
        .and_then(midnight)
        .unwrap_or(local)
}

fn day_start_dt(now: &DateTime<Utc>) -> DateTime<Local> {
    let local = now.with_timezone(&Local);
    midnight(local.date_naive()).unwrap_or(local)
}

fn month_start_dt(now: &DateTime<Utc>) -> DateTime<Local> {
    let local = now.with_timezone(&Local);
    let first = local.date_naive().with_day(1).unwrap_or_else(|| NaiveDate::from_ymd_opt(local.year(), local.month(), 1).expect("day 1 exists"));
    midnight(first).unwrap_or(local)
}

/// Local midnight. `single()` only fails inside a DST gap that lands exactly on
/// midnight, which no tz with these tools uses; the caller keeps the timestamp it
/// had rather than guessing an offset.
fn midnight(date: NaiveDate) -> Option<DateTime<Local>> {
    date.and_hms_opt(0, 0, 0).and_then(|n| Local.from_local_datetime(&n).single())
}

fn db_path() -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let test_home = std::env::var("CC_SWITCH_TEST_HOME").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let base = match test_home {
        Some(h) => std::path::PathBuf::from(h),
        None => home,
    };
    let db = base.join(".cc-switch").join("cc-switch.db");
    db.is_file().then_some(db)
}

fn read_only_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 16);
    for b in raw.as_bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'/' | b'-' | b'_' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    format!("file:{out}?mode=ro")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE providers (id TEXT PRIMARY KEY, name TEXT, limit_daily_usd REAL, limit_monthly_usd REAL);
             CREATE TABLE proxy_request_logs (provider_id TEXT, total_cost_usd TEXT, created_at INTEGER);
             INSERT INTO providers VALUES
               ('p-a','Anthropic',10.0,200.0), ('p-b','  ',5.0,NULL),
               ('p-c','Free',NULL,NULL), ('p-d','Zero',0,NULL);
             INSERT INTO proxy_request_logs VALUES
               ('p-a','4.25',1790165200), ('p-a','junk',1790165201), ('p-a','90.0',100),
               ('p-b','1.5',1790165202), ('p-d','3.0',1790165203), ('p-c','9.9',1790165204);",
        )
        .unwrap();
        c
    }

    #[test]
    fn a_capped_provider_becomes_one_bar_per_window() {
        let c = conn();
        let s = samples(&c, &Utc.timestamp_millis_opt(1_790_165_270_000).unwrap());
        // Rows written before this machine's local midnight are outside the day.
        let labels: Vec<&str> = s.iter().map(|x| x.label.as_deref().unwrap_or_default()).collect();
        assert!(s.iter().all(|x| x.used_percent > 0.0 && x.used_percent <= 100.0), "{labels:?}");
        assert!(s.iter().any(|x| x.window_minutes == WINDOW_DAILY), "{labels:?}");
        assert!(s.iter().any(|x| x.window_minutes == WINDOW_MONTHLY), "{labels:?}");
        for x in &s {
            assert!(x.resets_at_ms > 1_790_165_270_000, "every window opens forward: {x:?}");
            assert!(x.label.as_deref().unwrap_or_default().contains('$'), "{x:?}");
        }
    }

    #[test]
    fn uncapped_and_zero_capped_providers_are_never_shown() {
        let c = conn();
        let s = samples(&c, &Utc.timestamp_millis_opt(1_790_165_270_000).unwrap());
        let ids: Vec<String> = s.iter().map(|x| x.label.clone().unwrap_or_default()).collect();
        assert!(ids.iter().all(|l| !l.starts_with("Free") && !l.starts_with("Zero")), "{ids:?}");
    }

    #[test]
    fn an_unnamed_provider_falls_back_to_its_id() {
        let c = conn();
        let s = samples(&c, &Utc.timestamp_millis_opt(1_790_165_270_000).unwrap());
        assert!(s.iter().any(|x| x.label.as_deref().unwrap_or_default().starts_with("p-b")), "{s:?}");
    }

    #[test]
    fn a_ledger_without_our_tables_answers_nothing() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE unrelated (x INTEGER);").unwrap();
        assert!(samples(&c, &Utc::now()).is_empty());
    }

    #[test]
    fn month_bounds_and_resets_are_consistent() {
        let now = Utc.timestamp_millis_opt(1_767_225_600_000).unwrap(); // 2026-01-01T00:00:00Z
        assert!(month_start(&now) <= now.timestamp(), "the month opened at or before now");
        assert!(month_start_dt(&now).timestamp_millis() <= now.timestamp_millis() + 1);
        assert!(day_start(&now) <= now.timestamp());
        assert!(month_start_dt(&now).timestamp_millis() > 0);
    }

    #[test]
    #[ignore = "reads ~/.cc-switch/cc-switch.db"]
    fn the_live_gateway_reports_its_caps_or_the_absence_of_them() {
        let Some(db) = db_path() else { println!("[ccswitch] no gateway database here"); return };
        let c = Connection::open_with_flags(read_only_uri(&db), OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let counts: (i64, i64, i64) = c
            .query_row(
                "SELECT count(*), count(limit_daily_usd), count(limit_monthly_usd) FROM providers",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        println!("[ccswitch] {db:?} providers={counts:?}");
        println!("[ccswitch] spend today = {:?}", c.query_row("SELECT COUNT(*), COALESCE(SUM(CAST(total_cost_usd AS REAL)),0.0) FROM proxy_request_logs WHERE created_at >= ?1", [day_start(&Utc::now())], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,f64>(1)?))).unwrap());
        println!("[ccswitch] samples = {:?}", samples(&c, &Utc::now()));
    }
}

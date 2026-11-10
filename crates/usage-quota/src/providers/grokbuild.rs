//! Grok Build / SuperGrok weekly pool from the CLI billing proxy.
//!
//! Grok Build's TUI `/usage` panel loads the shared SuperGrok weekly allowance
//! through the same upstream the CLI uses after `grok login`. Captured live on
//! this machine (and independently by claudia / GrokUsageBar):
//!
//! ```text
//! GET https://cli-chat-proxy.grok.com/v1/billing?format=credits
//! Authorization: Bearer <~/.grok/auth.json OIDC key>
//! X-XAI-Token-Auth: xai-grok-cli
//! ```
//!
//! `config.creditUsagePercent` is **used** (0–100). `config.currentPeriod.end`
//! is the reset. When `isUnifiedBillingUser` is true, Cursor / Grok Bot /
//! GrokChat draw from this same weekly window — `productUsage` is a share
//! breakdown, not a separate meter, so this probe emits one weekly sample.
//!
//! Cursor does not get its own bar. Its spend is already inside that percent,
//! but it does not write `~/.grok`, so the five-minute probe cache would keep
//! showing the pre-spend number. A newer `~/.cursor/ai-tracking` database, or
//! the Cursor app simply being open, makes the next pass ask the proxy again.
//! A short floor keeps a streaming turn from refetching on every write.
//!
//! A machine with only Cursor installed has no `auth.json`. The same bar is
//! then filled from Cursor's own sign-in (`state.vscdb`'s
//! `cursorAuth/accessToken`, read-only) via `GetCurrentPeriodUsage`. That
//! answer is still one `grokbuild` sample — `planUsage.totalPercentUsed`, the
//! included-usage percent the editor shows — not a second Cursor row. When
//! both logins exist, Grok's billing proxy wins: it is the pool that also
//! sees Grok chat and Grok Bot.
//!
//! Nothing here logs in or refreshes either token. Grok owns its own refresh
//! (`auth.json` stays read-only), and Cursor owns its session. A Grok login
//! that is present but expired, or whose request fails, does **not** fall
//! through to Cursor: that would rename the window (different account id,
//! different cycle length) and re-fire the used-up banner. Cursor is the
//! source only when there is no Grok login at all.

use serde_json::Value;
use usage_core::QuotaSample;

use crate::http::get_json;
use crate::QuotaProbe;

const ENDPOINT: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const CURSOR_PERIOD: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const USER_AGENT: &str = concat!("tokenme/", env!("CARGO_PKG_VERSION"));
const TOKEN_AUTH: &str = "xai-grok-cli";
const WEEK_MINUTES: i64 = 10_080;

pub struct GrokBuildQuota;

impl QuotaProbe for GrokBuildQuota {
    fn tool(&self) -> &'static str {
        "grokbuild"
    }

    fn fetch(&self) -> Vec<QuotaSample> {
        // Count the attempt before the request returns. A failure keeps the
        // previous sample's timestamp (the grace window), and the refresh
        // decision keys off that timestamp — without this stamp a tracking-db
        // write would retry on every ingest pass for as long as the outage lasts.
        note_pool_attempt();
        live().unwrap_or_default()
    }

    fn reuse_cache(&self, captured_at_ms: i64) -> bool {
        !pool_refresh_due_now(captured_at_ms)
    }
}

enum GrokRead {
    Samples(Vec<QuotaSample>),
    /// No `auth.json`, or nothing in it that was ever a key.
    Absent,
    /// A Grok login exists and did not answer. Do not replace it with Cursor.
    Failed(&'static str),
}

fn live() -> Option<Vec<QuotaSample>> {
    // Grok's proxy is the shared pool, including chat and bot. Cursor's
    // editor session fills the same bar only when that login is absent.
    match grok_read() {
        GrokRead::Samples(samples) => Some(samples),
        GrokRead::Absent => cursor_live(),
        GrokRead::Failed(why) => {
            crate::notes::note("grokbuild", why);
            None
        }
    }
}

fn grok_read() -> GrokRead {
    let Some(text) = auth_text() else {
        return GrokRead::Absent;
    };
    let (token, identity) = match auth_state(&text) {
        AuthState::Live(token, identity) => (token, identity),
        AuthState::Expired => return GrokRead::Failed("grok login expired"),
        AuthState::Absent => return GrokRead::Absent,
    };
    let headers = [
        ("authorization", format!("Bearer {token}")),
        ("x-xai-token-auth", TOKEN_AUTH.to_string()),
        ("accept", "application/json".into()),
        ("user-agent", USER_AGENT.into()),
    ];
    let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let Some(body) = get_json(ENDPOINT, &borrowed) else {
        return GrokRead::Failed("grok billing request failed");
    };
    match stamp(samples_from(&body), &identity) {
        Some(samples) => GrokRead::Samples(samples),
        None => GrokRead::Failed("grok billing had no percent"),
    }
}

fn cursor_live() -> Option<Vec<QuotaSample>> {
    let Some((token, identity)) = cursor_credential() else {
        crate::notes::note("grokbuild", "no grok login and no cursor session");
        return None;
    };
    let headers = [
        ("authorization", format!("Bearer {token}")),
        ("content-type", "application/json".into()),
        ("connect-protocol-version", "1".into()),
        ("accept", "application/json".into()),
    ];
    let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let Some(body) = crate::http::post_json(CURSOR_PERIOD, &borrowed, serde_json::json!({})) else {
        crate::notes::note("grokbuild", "cursor period request failed");
        return None;
    };
    let Some(samples) = stamp(cursor_samples_from(&body), &identity) else {
        crate::notes::note("grokbuild", "cursor period had no included percent");
        return None;
    };
    Some(samples)
}

fn stamp(mut samples: Vec<QuotaSample>, identity: &str) -> Option<Vec<QuotaSample>> {
    if samples.is_empty() {
        return None;
    }
    for sample in &mut samples {
        sample.id = Some(super::account_window_id(identity, sample.window_minutes));
    }
    Some(samples)
}

/// `creditUsagePercent` is already used 0–100; the period type is weekly on
/// the measured SuperGrok accounts. Missing / out-of-range percent → no row
/// (never invent a number).
pub(crate) fn samples_from(body: &Value) -> Vec<QuotaSample> {
    let Some(config) = body.get("config").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(used) = config.get("creditUsagePercent").and_then(Value::as_f64) else {
        return Vec::new();
    };
    // Negatives / NaN are refused; mild overshoot is clamped to 100.
    if !used.is_finite() || used < 0.0 {
        return Vec::new();
    }
    let resets_at_ms = period_end_ms(config).unwrap_or(0);
    let window_minutes = period_minutes(config).unwrap_or(WEEK_MINUTES);
    vec![QuotaSample {
        used_percent: used.clamp(0.0, 100.0),
        window_minutes,
        resets_at_ms,
        label: Some(label_for(window_minutes)),
        id: None,
    }]
}

fn period_end_ms(config: &serde_json::Map<String, Value>) -> Option<i64> {
    let period = config.get("currentPeriod").or_else(|| config.get("billingPeriodEnd"));
    // Prefer nested currentPeriod.end; fall back to billingPeriodEnd string.
    if let Some(end) = config
        .get("currentPeriod")
        .and_then(|p| p.get("end"))
        .and_then(Value::as_str)
    {
        return usage_core::parse_ts_ms(end);
    }
    if let Some(end) = config.get("billingPeriodEnd").and_then(Value::as_str) {
        return usage_core::parse_ts_ms(end);
    }
    let _ = period;
    None
}

fn period_minutes(config: &serde_json::Map<String, Value>) -> Option<i64> {
    let kind = config
        .get("currentPeriod")
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "USAGE_PERIOD_TYPE_WEEKLY" | "" => Some(WEEK_MINUTES),
        "USAGE_PERIOD_TYPE_DAILY" => Some(1_440),
        "USAGE_PERIOD_TYPE_MONTHLY" => Some(43_200),
        _ => {
            // Derive from start/end when the enum is unfamiliar.
            let start = config
                .get("currentPeriod")
                .and_then(|p| p.get("start"))
                .and_then(Value::as_str)
                .and_then(usage_core::parse_ts_ms)?;
            let end = period_end_ms(config)?;
            let minutes = (end - start) / 60_000;
            if minutes > 0 {
                Some(minutes)
            } else {
                None
            }
        }
    }
}

fn label_for(minutes: i64) -> String {
    match minutes {
        300 => "5 小时".into(),
        1_440 => "天".into(),
        10_080 => "周".into(),
        43_200 => "月".into(),
        other => format!("{other} 分钟"),
    }
}

/// Cursor's current-period payload, either the Connect shape (`planUsage` at
/// the root) or the dashboard shape (`individualUsage.plan`). One included
/// percent — `totalPercentUsed`, the figure the editor calls "included usage".
/// `autoPercentUsed` is only the fallback when the total is absent. No percent
/// means no row; a missing cycle length is treated as the weekly shared pool.
pub(crate) fn cursor_samples_from(body: &Value) -> Vec<QuotaSample> {
    let plan = body
        .get("planUsage")
        .or_else(|| body.pointer("/individualUsage/plan"));
    let Some(plan) = plan else {
        return Vec::new();
    };
    if plan.get("enabled").and_then(Value::as_bool) == Some(false) {
        return Vec::new();
    }
    let Some(used) = plan
        .get("totalPercentUsed")
        .or_else(|| plan.get("autoPercentUsed"))
        .and_then(Value::as_f64)
    else {
        return Vec::new();
    };
    if !used.is_finite() || used < 0.0 {
        return Vec::new();
    }
    let start = body.get("billingCycleStart").and_then(value_ts_ms);
    let end = body.get("billingCycleEnd").and_then(value_ts_ms);
    let (window_minutes, resets_at_ms) = window_from_cycle(start, end);
    vec![QuotaSample {
        used_percent: used.clamp(0.0, 100.0),
        window_minutes,
        resets_at_ms,
        label: Some(label_for(window_minutes)),
        id: None,
    }]
}

/// Snap a cycle length onto the labels the strip already knows. A week that
/// is 10,079 minutes must still read as 周, not as a raw minute count.
fn window_from_cycle(start: Option<i64>, end: Option<i64>) -> (i64, i64) {
    let resets = end.unwrap_or(0);
    let Some(span) = start.zip(end).map(|(s, e)| e.saturating_sub(s) / 60_000).filter(|m| *m > 0) else {
        return (WEEK_MINUTES, resets);
    };
    let minutes = if (6_000..=12_000).contains(&span) {
        WEEK_MINUTES
    } else if (20_000..=50_000).contains(&span) {
        43_200
    } else if (1_000..=2_000).contains(&span) {
        1_440
    } else {
        span
    };
    (minutes, resets)
}

fn value_ts_ms(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return usage_core::parse_ts_ms(text);
    }
    let n = value.as_i64().or_else(|| value.as_f64().map(|f| f as i64))?;
    if n <= 0 {
        return None;
    }
    // Bare unix seconds are 10 digits through the 2280s; milliseconds are 13.
    if n < 10_000_000_000 { Some(n * 1000) } else { Some(n) }
}

/// Cursor's sign-in, read-only. `dirs::config_dir()` is Application Support
/// on macOS, `%APPDATA%` on Windows, and `~/.config` on Linux — the same
/// place every VS Code-family app keeps `state.vscdb`.
fn cursor_credential() -> Option<(String, String)> {
    let path = cursor_state_db()?;
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    credential_in(&conn)
}

fn cursor_state_db() -> Option<std::path::PathBuf> {
    let path = dirs::config_dir()?.join("Cursor").join("User").join("globalStorage").join("state.vscdb");
    path.is_file().then_some(path)
}

pub(crate) fn credential_in(conn: &rusqlite::Connection) -> Option<(String, String)> {
    let token = state_value(conn, "cursorAuth/accessToken")?;
    if token.is_empty() {
        return None;
    }
    let identity = state_value(conn, "cursorAuth/cachedEmail").unwrap_or_else(|| "cursor".into());
    Some((token, identity))
}

fn state_value(conn: &rusqlite::Connection, key: &str) -> Option<String> {
    let raw: String = conn
        .query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| row.get(0))
        .ok()?;
    let text = raw.trim();
    let plain = match serde_json::from_str::<Value>(text) {
        Ok(Value::String(inner)) => inner,
        _ => text.trim_matches('"').to_string(),
    };
    let plain = plain.trim().to_string();
    (!plain.is_empty()).then_some(plain)
}

/// `$GROK_HOME/auth.json` else `~/.grok/auth.json`. The file maps
/// `https://auth.x.ai::<client_id>` → `{ key, expires_at, … }`.
fn auth_text() -> Option<String> {
    // Match the usage adapter: trim `GROK_HOME`, treat blank as unset.
    let root = match std::env::var("GROK_HOME") {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                dirs::home_dir().map(|h| h.join(".grok"))?
            } else {
                std::path::PathBuf::from(trimmed)
            }
        }
        Err(_) => dirs::home_dir().map(|h| h.join(".grok"))?,
    };
    std::fs::read_to_string(root.join("auth.json")).ok()
}

enum AuthState {
    Live(String, String),
    /// A key was present and its `expires_at` is already past.
    Expired,
    Absent,
}

fn auth_state(text: &str) -> AuthState {
    let Ok(body) = serde_json::from_str::<Value>(text) else {
        return AuthState::Absent;
    };
    let Some(obj) = body.as_object() else {
        return AuthState::Absent;
    };
    let mut expired = false;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    for (account_key, entry) in obj {
        let Some(entry) = entry.as_object() else { continue };
        let Some(token) = entry.get("key").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        if let Some(expires) = entry.get("expires_at").and_then(Value::as_str) {
            if let Some(ms) = usage_core::parse_ts_ms(expires) {
                if ms <= now {
                    expired = true;
                    continue;
                }
            }
        }
        let identity = entry
            .get("user_id")
            .or_else(|| entry.get("email"))
            .or_else(|| entry.get("principal_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(account_key)
            .to_string();
        return AuthState::Live(token.to_string(), identity);
    }
    if expired { AuthState::Expired } else { AuthState::Absent }
}

/// A streaming Cursor turn rewrites the tracking db continuously. Under this
/// age the cached percent stands, even if the file just moved.
const POOL_REFRESH_FLOOR_MS: i64 = 20_000;
/// While the IDE is open the shared pool can move with no Grok log write.
/// This is the longest that open app may keep a stale percent.
const POOL_REFRESH_OPEN_MS: i64 = 45_000;

/// Whether the cached shared-pool sample is stale because Cursor has spent
/// since it was taken. `cached_at_ms <= 0` means there is no sample yet — the
/// ordinary fetch owns that, and an empty negative cache must not be retried
/// on every Cursor write.
pub(crate) fn pool_refresh_due(
    cached_at_ms: i64,
    now_ms: i64,
    activity_ms: Option<i64>,
    cursor_open: bool,
    last_attempt_ms: i64,
) -> bool {
    if cached_at_ms <= 0 {
        return false;
    }
    // A failed fetch does not move the sample timestamp. The attempt does.
    if last_attempt_ms > 0 && now_ms.saturating_sub(last_attempt_ms) < POOL_REFRESH_FLOOR_MS {
        return false;
    }
    let age = now_ms.saturating_sub(cached_at_ms);
    if age < POOL_REFRESH_FLOOR_MS {
        return false;
    }
    if activity_ms.is_some_and(|at| at > cached_at_ms) {
        return true;
    }
    cursor_open && age >= POOL_REFRESH_OPEN_MS
}

pub(crate) fn pool_refresh_due_now(cached_at_ms: i64) -> bool {
    pool_refresh_due(
        cached_at_ms,
        crate::cache::now_ms(),
        cursor_activity_ms(),
        crate::host::cursor_app_running(),
        last_pool_attempt_ms(),
    )
}

static LAST_POOL_ATTEMPT_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn note_pool_attempt() {
    LAST_POOL_ATTEMPT_MS.store(crate::cache::now_ms(), std::sync::atomic::Ordering::Relaxed);
}

fn last_pool_attempt_ms() -> i64 {
    LAST_POOL_ATTEMPT_MS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `~/.cursor/ai-tracking` when that directory exists. The engine watches it
/// so a Cursor spend wakes a pass instead of waiting out the idle cadence.
/// The database itself is never opened: its mtime is the whole signal, and
/// the billed percent still comes from the Grok proxy.
pub(crate) fn cursor_activity_roots() -> Vec<std::path::PathBuf> {
    cursor_tracking_dir().filter(|dir| dir.is_dir()).into_iter().collect()
}

pub(crate) fn is_cursor_activity_root(path: &std::path::Path) -> bool {
    cursor_activity_roots().iter().any(|root| root == path)
}

fn cursor_tracking_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|home| home.join(".cursor").join("ai-tracking"))
}

/// Newest mtime of the tracking database or its `-wal`, in epoch ms.
pub(crate) fn cursor_activity_ms() -> Option<i64> {
    let dir = cursor_tracking_dir()?;
    activity_ms_in(&dir)
}

pub(crate) fn activity_ms_in(dir: &std::path::Path) -> Option<i64> {
    let db = dir.join("ai-code-tracking.db");
    let wal = {
        let mut name = db.file_name()?.to_os_string();
        name.push("-wal");
        db.with_file_name(name)
    };
    match (mtime_ms(&db), mtime_ms(&wal)) {
        (Some(db_ms), Some(wal_ms)) => Some(db_ms.max(wal_ms)),
        (db_ms, wal_ms) => db_ms.or(wal_ms),
    }
}

fn mtime_ms(path: &std::path::Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_millis() as i64)
}

pub(crate) fn token_in(text: &str) -> Option<(String, String)> {
    match auth_state(text) {
        AuthState::Live(token, identity) => Some((token, identity)),
        AuthState::Expired | AuthState::Absent => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured 200 body from `GET …/billing?format=credits` (account fields
    /// faked; every field name and unit is the real thing).
    const OK_SAMPLE: &str = r#"{
      "config": {
        "currentPeriod": {
          "type": "USAGE_PERIOD_TYPE_WEEKLY",
          "start": "2026-11-03T15:49:56.585+00:00",
          "end": "2026-11-10T15:49:56.585+00:00"
        },
        "creditUsagePercent": 5.0,
        "productUsage": [
          {"product": "GrokBot", "usagePercent": 2.0},
          {"product": "GrokChat", "usagePercent": 2.0},
          {"product": "Cursor", "usagePercent": 1.0}
        ],
        "isUnifiedBillingUser": true,
        "prepaidBalance": {"val": 0},
        "billingPeriodStart": "2026-11-03T15:49:56.585+00:00",
        "billingPeriodEnd": "2026-11-10T15:49:56.585+00:00"
      }
    }"#;

    #[test]
    fn weekly_window_from_credit_usage_percent() {
        let body: Value = serde_json::from_str(OK_SAMPLE).unwrap();
        let s = samples_from(&body);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].used_percent, 5.0);
        assert_eq!(s[0].window_minutes, WEEK_MINUTES);
        assert_eq!(s[0].label.as_deref(), Some("周"));
        assert!(s[0].resets_at_ms > 0, "period end must parse");
        // productUsage is a share breakdown of the same pool — not extra rows.
    }

    #[test]
    fn used_percent_is_clamped_and_junk_yields_nothing() {
        let over: Value = serde_json::from_str(
            r#"{"config":{"creditUsagePercent":140.5,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-11-10T15:49:56.585+00:00"}}}"#,
        )
        .unwrap();
        assert_eq!(samples_from(&over)[0].used_percent, 100.0);
        assert!(samples_from(&Value::Null).is_empty());
        assert!(samples_from(&serde_json::from_str(r#"{"config":{}}"#).unwrap()).is_empty());
        assert!(samples_from(
            &serde_json::from_str(r#"{"config":{"creditUsagePercent":-1}}"#).unwrap()
        )
        .is_empty());
    }

    #[test]
    fn auth_json_needs_a_live_key() {
        let future = "2099-01-01T00:00:00Z";
        let past = "2020-01-01T00:00:00Z";
        let (token, id) = token_in(&format!(
            r#"{{"https://auth.x.ai::client":{{"key":"  eyJfake  ","expires_at":"{future}","user_id":"u-1"}}}}"#
        ))
        .unwrap();
        assert_eq!(token, "eyJfake");
        assert_eq!(id, "u-1");
        assert!(
            token_in(&format!(
                r#"{{"https://auth.x.ai::client":{{"key":"dead","expires_at":"{past}"}}}}"#
            ))
            .is_none(),
            "expired keys are refused"
        );
        assert!(token_in(r#"{"https://auth.x.ai::client":{"key":""}}"#).is_none());
        assert!(token_in("not json").is_none());
        assert!(matches!(auth_state(&format!(
            r#"{{"https://auth.x.ai::client":{{"key":"dead","expires_at":"{past}"}}}}"#
        )), AuthState::Expired), "an expired key is a failed login, not an absent one");
        assert!(matches!(auth_state("{}"), AuthState::Absent));
    }

    #[test]
    fn cursor_spend_refreshes_the_same_pool_and_a_burst_does_not() {
        let cached = 1_000_000;
        assert!(
            !pool_refresh_due(0, cached + 50_000, Some(cached + 40_000), true, 0),
            "no cached sample yet — the ordinary fetch owns the first read"
        );
        assert!(
            !pool_refresh_due(cached, cached + 5_000, Some(cached + 4_000), true, 0),
            "a streaming turn must not refetch inside the floor"
        );
        assert!(
            pool_refresh_due(cached, cached + 21_000, Some(cached + 20_000), false, 0),
            "a tracking-db write after the floor is a Cursor spend"
        );
        assert!(
            !pool_refresh_due(cached, cached + 21_000, Some(cached - 1), false, 0),
            "activity older than the sample is not a new spend"
        );
        assert!(!pool_refresh_due(cached, cached + 30_000, None, true, 0));
        assert!(
            !pool_refresh_due(cached, cached + 21_000, Some(cached + 20_000), false, cached + 16_000),
            "a failed attempt inside the floor does not retry, even though the sample timestamp did not move"
        );
        assert!(
            pool_refresh_due(cached, cached + 21_000, Some(cached + 20_000), false, cached),
            "an attempt older than the floor may try again"
        );
        assert!(
            pool_refresh_due(cached, cached + POOL_REFRESH_OPEN_MS, None, true, 0),
            "an open IDE with no new file still moves the shared pool"
        );
        assert!(
            !pool_refresh_due(cached, cached + 120_000, None, false, 0),
            "a closed IDE and a quiet tracking db keep the five-minute cache"
        );
    }

    #[test]
    fn the_tracking_wal_is_cursor_activity_even_when_the_main_db_is_older() {
        let dir = std::env::temp_dir().join(format!("tokenme-cursor-activity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("ai-code-tracking.db");
        std::fs::write(&db, b"db").unwrap();
        let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let file = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(past)).unwrap();
        let wal = dir.join("ai-code-tracking.db-wal");
        std::fs::write(&wal, b"wal").unwrap();
        let seen = activity_ms_in(&dir).unwrap();
        assert!(seen > 1_700_000_000_000, "the wal mtime wins over a stale main db, got {seen}");
        assert!(activity_ms_in(&dir.join("missing")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Cursor-only install has no `~/.grok/auth.json`. The editor's current
    /// period is the same included pool, and it must become the one bar.
    #[test]
    fn a_cursor_only_period_is_the_same_weekly_bar() {
        let body: Value = serde_json::from_str(
            r#"{
              "billingCycleStart": "2026-11-03T15:49:56.585Z",
              "billingCycleEnd": "2026-11-10T15:49:56.585Z",
              "planUsage": {
                "totalPercentUsed": 12.5,
                "autoPercentUsed": 4.0,
                "apiPercentUsed": 80.0
              }
            }"#,
        )
        .unwrap();
        let s = cursor_samples_from(&body);
        assert_eq!(s.len(), 1, "one included bar, not one per pool");
        assert_eq!(s[0].used_percent, 12.5, "totalPercentUsed is the included figure");
        assert_eq!(s[0].window_minutes, WEEK_MINUTES);
        assert_eq!(s[0].label.as_deref(), Some("周"));
        assert!(s[0].resets_at_ms > 0);

        let nested: Value = serde_json::from_str(
            r#"{"billingCycleEnd": 1790000000000, "individualUsage": {"plan": {"autoPercentUsed": 9.0}}}"#,
        )
        .unwrap();
        let n = cursor_samples_from(&nested);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].used_percent, 9.0, "autoPercentUsed only when the total is absent");
        assert_eq!(n[0].resets_at_ms, 1_790_000_000_000);

        assert!(cursor_samples_from(&serde_json::json!({"planUsage": {"enabled": false, "totalPercentUsed": 10.0}})).is_empty());
        assert!(cursor_samples_from(&serde_json::json!({"planUsage": {}})).is_empty());
        assert!(cursor_samples_from(&Value::Null).is_empty());
    }

    #[test]
    fn cursor_sign_in_is_read_from_item_table_without_writing() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO ItemTable (key, value) VALUES
               ('cursorAuth/accessToken', '\"curs_live\"'),
               ('cursorAuth/cachedEmail', 'person@example.com');",
        )
        .unwrap();
        let (token, identity) = credential_in(&conn).unwrap();
        assert_eq!(token, "curs_live");
        assert_eq!(identity, "person@example.com");

        let bare = rusqlite::Connection::open_in_memory().unwrap();
        bare.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO ItemTable (key, value) VALUES ('cursorAuth/accessToken', 'plain-token');",
        )
        .unwrap();
        let (token, identity) = credential_in(&bare).unwrap();
        assert_eq!(token, "plain-token");
        assert_eq!(identity, "cursor");

        let empty = rusqlite::Connection::open_in_memory().unwrap();
        empty.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);").unwrap();
        assert!(credential_in(&empty).is_none());
    }

    #[test]
    #[ignore]
    fn live_billing_endpoint_answers_for_this_account() {
        let samples = GrokBuildQuota.fetch();
        assert!(
            !samples.is_empty(),
            "no ~/.grok/auth.json key, or it expired (401)?"
        );
        for s in &samples {
            println!(
                "grokbuild quota: {:.2}% window={} label={:?} resets_at_ms={}",
                s.used_percent, s.window_minutes, s.label, s.resets_at_ms
            );
        }
    }
}

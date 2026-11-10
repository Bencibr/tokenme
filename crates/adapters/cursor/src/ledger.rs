//! Cursor has no local per-request ledger. The dashboard does.
//!
//! `discover` asks `get-filtered-usage-events` with the sign-in already in
//! `state.vscdb` and appends reduced lines to `cursor-usage.jsonl` under
//! tokenme's data dir. Ingest then reads that file like any other JSONL.
//! The database is opened read-only; the cookie never touches the log.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use usage_core::{FileKind, ReadCursor, ReadOutcome, SourceFile, TokenCounts};

use crate::parser::{self, dedupe_key, event_from};

const EVENTS_URL: &str = "https://cursor.com/api/dashboard/get-filtered-usage-events";
const PAGE_SIZE: usize = 100;
const PAGES_PER_PULL: u32 = 4;
const BACKFILL_MS: i64 = 30 * 86_400_000;
const QUIET_MS: i64 = 60_000;
const ACTIVITY_MS: i64 = 20_000;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawEvent {
    pub ts_ms: i64,
    pub model: String,
    pub counts: TokenCounts,
}

impl RawEvent {
    fn key(&self) -> String {
        dedupe_key(self.ts_ms, &self.model, &self.counts)
    }

    fn line(&self) -> String {
        let key = self.key();
        serde_json::json!({
            "ts": self.ts_ms,
            "model": self.model,
            "input": self.counts.input,
            "output": self.counts.output,
            "cache_read": self.counts.cache_read,
            "cache_write": self.counts.cache_creation,
            "dedupe": key,
        })
        .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullState {
    #[serde(default)]
    pub watermark_ms: i64,
    #[serde(default)]
    pub seen: Vec<String>,
    #[serde(default)]
    pub next_page: u32,
    #[serde(default)]
    pub range_start_ms: i64,
    #[serde(default)]
    pub range_end_ms: i64,
    #[serde(default)]
    pub pulled_at_ms: i64,
}

impl Default for PullState {
    fn default() -> Self {
        Self {
            watermark_ms: 0,
            seen: Vec::new(),
            next_page: 1,
            range_start_ms: 0,
            range_end_ms: 0,
            pulled_at_ms: 0,
        }
    }
}

pub(crate) fn should_pull(state: &PullState, now_ms: i64, tracking_ms: Option<i64>) -> bool {
    if state.next_page > 1 {
        return true;
    }
    let age = now_ms.saturating_sub(state.pulled_at_ms);
    if tracking_ms.is_some_and(|t| t > state.pulled_at_ms) && age >= ACTIVITY_MS {
        return true;
    }
    age >= QUIET_MS
}

/// Fold one dashboard page into ledger lines.
///
/// Pages are newest-first. A row older than the watermark means this page
/// has crossed into history we already stored, so paging stops. A full page
/// that is still entirely new asks for the next page inside the same window.
pub(crate) fn fold_page(state: &PullState, events: &[RawEvent], page: u32, page_len: usize) -> (Vec<String>, PullState, bool) {
    let mut lines = Vec::new();
    let mut watermark = state.watermark_ms;
    let mut seen = state.seen.clone();
    let mut crossed = false;
    for event in events {
        if event.ts_ms <= 0 || event.model.is_empty() || event.counts.is_zero() {
            continue;
        }
        if event.ts_ms < state.watermark_ms {
            crossed = true;
            continue;
        }
        let key = event.key();
        if event.ts_ms == state.watermark_ms && state.seen.iter().any(|have| have == &key) {
            continue;
        }
        lines.push(event.line());
        if event.ts_ms > watermark {
            watermark = event.ts_ms;
            seen.clear();
            seen.push(key);
        } else if event.ts_ms == watermark && !seen.iter().any(|have| have == &key) {
            seen.push(key);
        }
    }
    let more = page_len >= PAGE_SIZE && !crossed;
    let mut next = state.clone();
    next.watermark_ms = watermark;
    next.seen = seen;
    next.next_page = if more { page.saturating_add(1) } else { 1 };
    (lines, next, more)
}

pub(crate) fn events_in(body: &Value) -> Vec<RawEvent> {
    let Some(rows) = body.get("usageEventsDisplay").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter().filter_map(raw_event).collect()
}

fn raw_event(value: &Value) -> Option<RawEvent> {
    let ts_ms = timestamp_ms(value.get("timestamp")?)?;
    let model = value.get("model").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty())?;
    let usage = value.get("tokenUsage").unwrap_or(value);
    let counts = TokenCounts {
        input: token_num(usage.get("inputTokens")),
        output: token_num(usage.get("outputTokens")),
        cache_read: token_num(usage.get("cacheReadTokens")),
        cache_creation: token_num(usage.get("cacheWriteTokens")),
        ..TokenCounts::default()
    };
    Some(RawEvent { ts_ms, model: model.to_string(), counts })
}

fn timestamp_ms(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return usage_core::parse_ts_ms(text);
    }
    let n = value.as_i64().or_else(|| value.as_f64().map(|n| n as i64))?;
    if n <= 0 {
        return None;
    }
    if n < 10_000_000_000 { Some(n * 1000) } else { Some(n) }
}

fn token_num(value: Option<&Value>) -> f64 {
    let Some(value) = value else { return 0.0 };
    let n = if let Some(n) = value.as_f64() {
        n
    } else {
        value.as_str().and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(0.0)
    };
    if n.is_finite() && n > 0.0 { n } else { 0.0 }
}

pub(crate) fn data_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("tokenme"))
}

pub(crate) fn ledger_path() -> Option<PathBuf> {
    Some(data_dir()?.join("cursor-usage.jsonl"))
}

fn state_path() -> Option<PathBuf> {
    Some(data_dir()?.join("cursor-usage.state.json"))
}

pub(crate) fn tracking_mtime_ms() -> Option<i64> {
    let home = dirs::home_dir()?;
    let db = home.join(".cursor").join("ai-tracking").join("ai-code-tracking.db");
    let wal = home.join(".cursor").join("ai-tracking").join("ai-code-tracking.db-wal");
    mtime_ms(&db).max(mtime_ms(&wal))
}

fn mtime_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_millis() as i64)
}

/// Append newly seen account events. Failures leave the previous ledger in place.
pub(crate) fn pull() {
    let Some(dir) = data_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let Some(ledger) = ledger_path() else { return };
    let Some(state_file) = state_path() else { return };
    let now = now_ms();
    let mut state = read_state(&state_file);
    if !should_pull(&state, now, tracking_mtime_ms()) {
        return;
    }
    let Some((token, user)) = sign_in() else { return };
    if state.next_page <= 1 {
        state.range_end_ms = now;
        state.range_start_ms = if state.watermark_ms > 0 {
            state.watermark_ms
        } else {
            now.saturating_sub(BACKFILL_MS)
        };
        state.next_page = 1;
    }
    let mut page = state.next_page.max(1);
    for _ in 0..PAGES_PER_PULL {
        let Some(body) = fetch_page(&token, &user, state.range_start_ms, state.range_end_ms, page) else {
            break;
        };
        let events = events_in(&body);
        let page_len = body.get("usageEventsDisplay").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
        let (lines, next, more) = fold_page(&state, &events, page, page_len);
        if !lines.is_empty() && append_lines(&ledger, &lines).is_err() {
            break;
        }
        state = next;
        if !more {
            state.pulled_at_ms = now;
            state.next_page = 1;
            break;
        }
        page = state.next_page;
    }
    if state.next_page > 1 {
        // Interrupted mid-window: the next discover continues this range.
        state.pulled_at_ms = state.pulled_at_ms.min(now);
    }
    let _ = write_state(&state_file, &state);
}

fn fetch_page(token: &str, user: &str, start_ms: i64, end_ms: i64, page: u32) -> Option<Value> {
    let cookie = format!(
        "WorkosCursorSessionToken={}%3A%3A{token}",
        user.replace('|', "%7C")
    );
    let body = serde_json::json!({
        "startDate": start_ms.to_string(),
        "endDate": end_ms.to_string(),
        "page": page,
        "pageSize": PAGE_SIZE,
    });
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout_read(Duration::from_secs(8))
        .build();
    let response = agent
        .post(EVENTS_URL)
        .set("cookie", &cookie)
        .set("authorization", &format!("Bearer {token}"))
        .set("content-type", "application/json")
        .set("origin", "https://cursor.com")
        .set("accept", "application/json")
        .send_json(body)
        .ok()?;
    response.into_json().ok()
}

fn append_lines(path: &Path, lines: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    for line in lines {
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

fn read_state(path: &Path) -> PullState {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn write_state(path: &Path, state: &PullState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string(state).unwrap_or_else(|_| "{}".into()))
}

pub(crate) fn source_file(path: &Path) -> Option<SourceFile> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some(SourceFile { path: path.to_path_buf(), kind: FileKind::Jsonl, size: meta.len(), mtime_ms })
}

pub(crate) fn read_ledger(file: &SourceFile, cursor: ReadCursor) -> ReadOutcome {
    let Ok(bytes) = std::fs::read(&file.path) else {
        return ReadOutcome { events: Vec::new(), cursor };
    };
    let start = (cursor.0 as usize).min(bytes.len());
    let slice = &bytes[start..];
    let end_rel = slice.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    let mut events = Vec::new();
    for raw in slice[..end_rel].split(|b| *b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(raw) else { continue };
        let Some(row) = parser::parse_line(text) else { continue };
        events.push(event_from(row, &file.key()));
    }
    ReadOutcome { events, cursor: ReadCursor(start as u64 + end_rel as u64) }
}

pub(crate) fn roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(path) = ledger_path() {
        if let Some(parent) = path.parent() {
            if parent.is_dir() {
                // The ledger file itself, not the tokenme data dir: that dir
                // also holds index.db, and watching it would wake on our own writes.
                if path.is_file() {
                    out.push(path);
                }
            }
        }
    }
    if let Some(home) = dirs::home_dir() {
        let tracking = home.join(".cursor").join("ai-tracking");
        if tracking.is_dir() {
            out.push(tracking);
        }
    }
    out
}

fn sign_in() -> Option<(String, String)> {
    let path = cursor_state_db()?;
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let token = state_value(&conn, "cursorAuth/accessToken")?;
    let user = jwt_sub(&token)
        .or_else(|| profile_auth_id(&conn))
        .or_else(|| state_value(&conn, "cursorAuth/cachedEmail"))?;
    Some((token, user))
}

pub(crate) fn signed_in() -> bool {
    sign_in().is_some()
}

fn cursor_state_db() -> Option<PathBuf> {
    let path = dirs::config_dir()?.join("Cursor").join("User").join("globalStorage").join("state.vscdb");
    path.is_file().then_some(path)
}

fn state_value(conn: &rusqlite::Connection, key: &str) -> Option<String> {
    let raw: String = conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| row.get(0)).ok()?;
    let text = raw.trim();
    let plain = match serde_json::from_str::<Value>(text) {
        Ok(Value::String(inner)) => inner,
        _ => text.trim_matches('"').to_string(),
    };
    let plain = plain.trim().to_string();
    (!plain.is_empty()).then_some(plain)
}

fn profile_auth_id(conn: &rusqlite::Connection) -> Option<String> {
    let raw = state_value(conn, "cursorAuth/cachedScopedProfile")?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value.get("authId").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn jwt_sub(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    if payload.is_empty() {
        return None;
    }
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("sub").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(ts: i64, model: &str, input: f64) -> RawEvent {
        RawEvent {
            ts_ms: ts,
            model: model.into(),
            counts: TokenCounts { input, output: 1.0, ..TokenCounts::default() },
        }
    }

    #[test]
    fn a_page_keeps_new_turns_and_stops_once_history_reappears() {
        let body = serde_json::json!({
            "usageEventsDisplay": [
                {"timestamp": "1700000002000", "model": "composer-2.5", "tokenUsage": {"inputTokens": 10, "outputTokens": 4, "cacheReadTokens": "8", "cacheWriteTokens": 2}},
                {"timestamp": "1700000001000", "model": "grok-4", "tokenUsage": {"inputTokens": 0, "outputTokens": 0}}
            ]
        });
        let events = events_in(&body);
        assert_eq!(events.len(), 2);
        let (lines, next, more) = fold_page(&PullState::default(), &events, 1, 2);
        assert!(!more);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("composer-2.5"));
        assert_eq!(next.watermark_ms, 1_700_000_002_000);

        let newer = vec![
            event(1_700_000_003_000, "composer-2.5", 3.0),
            RawEvent {
                ts_ms: 1_700_000_002_000,
                model: "composer-2.5".into(),
                counts: TokenCounts { input: 10.0, output: 4.0, cache_read: 8.0, cache_creation: 2.0, ..TokenCounts::default() },
            },
        ];
        let (more_lines, later, more) = fold_page(&next, &newer, 1, PAGE_SIZE);
        assert!(more, "a full page that is still newer than the watermark continues");
        assert_eq!(more_lines.len(), 1);
        assert!(more_lines[0].contains("1700000003000"));
        assert_eq!(later.watermark_ms, 1_700_000_003_000);
        assert_eq!(later.next_page, 2);
    }

    #[test]
    fn quiet_accounts_wait_and_a_half_finished_page_does_not() {
        let fresh = PullState { pulled_at_ms: 1_000_000, next_page: 1, ..PullState::default() };
        assert!(!should_pull(&fresh, 1_000_000 + 30_000, None));
        assert!(should_pull(&fresh, 1_000_000 + QUIET_MS, None));
        assert!(should_pull(&fresh, 1_000_000 + ACTIVITY_MS, Some(1_000_000 + 1)));
        let mid = PullState { next_page: 3, pulled_at_ms: 1_000_000, ..PullState::default() };
        assert!(should_pull(&mid, 1_000_000, None));
    }
}

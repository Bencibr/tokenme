//! File logging + panic capture. Dependency-free on purpose: the logger must
//! survive whatever killed the thing it is reporting on.
//!
//! The active file is `panel.log` under the OS log directory. Two guards keep
//! it a diagnostic, not an archive: past 2 MiB it rotates to a single scratch
//! generation (`panel.log.1`), and at local midnight it rotates to a dated
//! file (`panel-2026-09-30.log`). Dated files older than [`KEEP_DAYS`] are
//! deleted on the next write after startup — so the directory holds roughly
//! this week, nothing more. Lines carry a local wall-clock stamp; the panic
//! hook writes every panic — thread, message, location — before the default
//! stderr hook runs, so a menu-bar app that dies without a console still
//! leaves a note behind.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde::Serialize;
use serde_json::Value;

const MAX_BYTES: u64 = 2 << 20;
const MAX_DIAGNOSTIC_READ_BYTES: u64 = 256 << 10;
const MAX_DIAGNOSTIC_LINES: usize = 200;
const MAX_DIAGNOSTIC_SEARCH: usize = 100;
/// Dated log files older than this are deleted (days).
const KEEP_DAYS: i64 = 7;

static PATH: OnceLock<PathBuf> = OnceLock::new();
static LOCK: Mutex<()> = Mutex::new(());
/// The local day the active `panel.log` belongs to; rotation happens when the
/// wall clock moves to another day.
static LAST_DAY: Mutex<Option<String>> = Mutex::new(None);

pub fn init() {
    let dir = log_dir();
    let _ = fs::create_dir_all(&dir);
    let _ = PATH.set(dir.join("panel.log"));
    install_panic_hook();
    info(&format!(
        "TokenMe v{} build {} starting",
        env!("CARGO_PKG_VERSION"),
        env!("TOKENME_BUILD_ID")
    ));
}

pub fn log_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = dirs::home_dir().map(|h| h.join("Library").join("Logs").join("tokenme"));
    #[cfg(not(target_os = "macos"))]
    let base = dirs::data_local_dir().map(|d| d.join("tokenme").join("logs"));
    base.unwrap_or_else(|| PathBuf::from("."))
}

fn diagnostics_path() -> PathBuf {
    PATH.get()
        .and_then(|path| path.parent().map(|dir| dir.join("diagnostics.log")))
        .unwrap_or_else(|| log_dir().join("diagnostics.log"))
}

pub fn info(msg: &str) {
    write_line("info", msg);
}

pub fn error(msg: &str) {
    write_line("error", msg);
}

#[derive(Debug, Serialize)]
struct DiagnosticEvent<'a> {
    schema: u8,
    at_ms: i64,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
}

const DIAGNOSTIC_EVENTS: &[&str] = &[
    "sync.primary.start",
    "sync.primary.stop",
    "sync.pairing.requested",
    "sync.pairing.approved",
    "sync.pairing.rejected",
    "sync.transport.connected",
    "sync.transport.failed",
    "diagnostics.session.created",
    "diagnostics.request.denied",
    "diagnostics.operation.completed",
];

fn safe_event_field(value: Option<&str>) -> Option<String> {
    value.map(|value| {
        let redacted = redact_text(value);
        redacted.chars().take(128).collect()
    })
}

/// Persist only the fixed audit fields. The caller never supplies a log path,
/// body, credential, or internal handler error to this boundary.
pub fn diagnostic_event(
    event: &str,
    outcome: Option<&str>,
    scope: Option<&str>,
    origin: Option<&str>,
    request_id: Option<&str>,
    error_code: Option<&str>,
    operation_id: Option<&str>,
    action: Option<&str>,
) {
    if !DIAGNOSTIC_EVENTS.contains(&event) {
        return;
    }
    let entry = DiagnosticEvent {
        schema: 1,
        at_ms: usage_core::report::now_ms(),
        event,
        outcome: outcome.map(|value| match value {
            "ok" | "error" | "denied" | "expired" | "revoked" => value,
            _ => "error",
        }),
        scope: safe_event_field(scope),
        origin: safe_event_field(origin),
        request_id: safe_event_field(request_id),
        error_code: safe_event_field(error_code),
        operation_id: safe_event_field(operation_id),
        action: safe_event_field(action),
    };
    let Ok(line) = serde_json::to_string(&entry) else { return };
    let path = diagnostics_path();
    let _guard = LOCK.lock();
    let now = Local::now();
    let _ = fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")));
    rotate_named(&path, &now, "diagnostics");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
        let _ = file.write_all(b"\n");
        let _ = file.sync_data();
    }
}

/// Map existing sync command names onto the design's fixed event vocabulary.
/// Status snapshots are intentionally not audit events.
pub fn sync_event(action: &str, ok: bool, error_code: Option<&str>) {
    let event = match action {
        "start_primary" => "sync.primary.start",
        "stop" => "sync.primary.stop",
        "create_invite" | "pair" | "pair_discovered" => "sync.pairing.requested",
        "approve" => "sync.pairing.approved",
        "reject" | "clear_expired_request" => "sync.pairing.rejected",
        "start_secondary" | "discover" => {
            if ok { "sync.transport.connected" } else { "sync.transport.failed" }
        }
        "status" | "snapshot" => return,
        _ => {
            if ok { "sync.transport.connected" } else { "sync.transport.failed" }
        }
    };
    diagnostic_event(
        event,
        Some(if ok { "ok" } else { "error" }),
        None,
        None,
        None,
        error_code,
        None,
        Some(action),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticLog {
    Panel,
    Scan,
    Diagnostics,
}

impl DiagnosticLog {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "panel" => Ok(Self::Panel),
            "scan" => Ok(Self::Scan),
            // The wire name "sync" predates the diagnostics ledger; both
            // names read the same file, so one variant carries them both.
            "sync" | "diagnostics" => Ok(Self::Diagnostics),
            _ => Err("log must be one of panel, scan, sync, diagnostics".into()),
        }
    }

    fn path(self) -> PathBuf {
        match self {
            Self::Panel => PATH.get().cloned().unwrap_or_else(|| log_dir().join("panel.log")),
            Self::Scan => PATH
                .get()
                .and_then(|path| path.parent().map(|dir| dir.join("scan.log")))
                .unwrap_or_else(|| log_dir().join("scan.log")),
            Self::Diagnostics => diagnostics_path(),
        }
    }
}

fn read_bounded(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => "log is unavailable".to_string(),
        _ => "log could not be read".to_string(),
    })?;
    let length = file.metadata().map_err(|_| "log could not be read")?.len();
    let start = length.saturating_sub(MAX_DIAGNOSTIC_READ_BYTES);
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(start)).map_err(|_| "log could not be read")?;
    let mut bytes = Vec::with_capacity((length - start) as usize);
    file.take(MAX_DIAGNOSTIC_READ_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|_| "log could not be read")?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn line_timestamp_ms(line: &str) -> Option<i64> {
    if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(line) {
        if let Some(timestamp) = object.get("at_ms").and_then(Value::as_i64) {
            return Some(timestamp);
        }
    }
    let timestamp = line.get(..19)?;
    let fractional_ms = if line.as_bytes().get(19) == Some(&b'.') {
        line.get(20..23)?.parse::<i64>().ok()?
    } else {
        0
    };
    let naive = NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S").ok()?;
    Local.from_local_datetime(&naive).single().map(|date| date.timestamp_millis() + fractional_ms)
}

fn bounded_lines(log: &str, since_ms: Option<i64>) -> Result<Vec<String>, String> {
    let kind = DiagnosticLog::parse(log)?;
    let raw = read_bounded(&kind.path())?;
    Ok(raw
        .lines()
        .filter(|line| since_ms.is_none_or(|since| line_timestamp_ms(line).is_some_and(|at| at >= since)))
        .map(redact_text)
        .collect())
}

pub fn diagnostic_log_tail(log: &str, lines: usize, since_ms: Option<i64>) -> Result<Vec<String>, String> {
    if lines == 0 || lines > MAX_DIAGNOSTIC_LINES {
        return Err(format!("lines must be between 1 and {MAX_DIAGNOSTIC_LINES}"));
    }
    let all = bounded_lines(log, since_ms)?;
    Ok(all.into_iter().rev().take(lines).collect::<Vec<_>>().into_iter().rev().collect())
}

pub fn diagnostic_log_search(
    log: &str,
    query: &str,
    since_ms: Option<i64>,
    limit: usize,
) -> Result<Vec<String>, String> {
    if query.is_empty() || query.len() > 128 || query.chars().any(char::is_control) {
        return Err("query must be 1-128 printable characters".into());
    }
    if limit == 0 || limit > MAX_DIAGNOSTIC_SEARCH {
        return Err(format!("limit must be between 1 and {MAX_DIAGNOSTIC_SEARCH}"));
    }
    let needle = query.to_ascii_lowercase();
    Ok(bounded_lines(log, since_ms)?
        .into_iter()
        .filter(|line| line.to_ascii_lowercase().contains(&needle))
        .take(limit)
        .collect())
}

/// Redaction is also applied when old panel/scan logs are read, not only when
/// the structured diagnostics log is written.
pub fn redact_text(input: &str) -> String {
    let mut output = input.to_string();
    for root in [dirs::home_dir(), std::env::current_dir().ok()] {
        if let Some(root) = root {
            let root = root.to_string_lossy();
            if !root.is_empty() {
                output = output.replace(root.as_ref(), "<path>");
            }
        }
    }
    for marker in [
        "authorization",
        "cookie",
        "api_key",
        "api-key",
        "password",
        "private_key",
        "private-key",
        "secret",
        "token",
    ] {
        redact_marker(&mut output, marker);
    }
    redact_absolute_paths(&output)
}

fn redact_marker(value: &mut String, marker: &str) {
    loop {
        let lower = value.to_ascii_lowercase();
        let Some(position) = lower.find(marker) else { return };
        let before = position.checked_sub(1).and_then(|index| value.as_bytes().get(index)).copied();
        let after = value.as_bytes().get(position + marker.len()).copied();
        if before.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || after.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            let next = position + marker.len();
            if next >= value.len() {
                return;
            }
            let tail = value[next..].to_string();
            if tail.is_empty() {
                return;
            }
            // The marker was part of an ordinary word (for example
            // `tokenme`); continue searching after it.
            let mut remainder = tail;
            redact_marker(&mut remainder, marker);
            value.truncate(position + marker.len());
            value.push_str(&remainder);
            return;
        }
        let mut start = position + marker.len();
        while value.as_bytes().get(start).is_some_and(u8::is_ascii_whitespace) {
            start += 1;
        }
        if value.as_bytes().get(start).is_some_and(|byte| *byte == b':' || *byte == b'=') {
            start += 1;
            while value.as_bytes().get(start).is_some_and(u8::is_ascii_whitespace) {
                start += 1;
            }
        } else if marker != "authorization" && marker != "cookie" {
            return;
        }
        let end = if marker == "authorization" || marker == "cookie" {
            value[start..]
                .find([',', ';', '}'])
                .map(|offset| start + offset)
                .unwrap_or(value.len())
        } else if value.as_bytes().get(start) == Some(&b'"') {
            value[start + 1..]
                .find('"')
                .map(|offset| start + 2 + offset)
                .unwrap_or(value.len())
        } else {
            value[start..]
                .find(|character: char| character.is_whitespace() || matches!(character, ',' | ';' | '}'))
                .map(|offset| start + offset)
                .unwrap_or(value.len())
        };
        value.replace_range(position..end, "<redacted>");
    }
}

fn redact_absolute_paths(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let unix_path = bytes[index] == b'/' && bytes.get(index + 1).is_some_and(|byte| *byte != b'/');
        let windows_path = index + 2 < bytes.len()
            && bytes[index].is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && (bytes[index + 2] == b'\\' || bytes[index + 2] == b'/');
        if unix_path || windows_path {
            let end = value[index..]
                .find(|character: char| character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ';'))
                .map(|offset| index + offset)
                .unwrap_or(bytes.len());
            output.push_str("<path>");
            index = end;
        } else {
            let character = value[index..].chars().next().expect("index is on a character boundary");
            output.push(character);
            index += character.len_utf8();
        }
    }
    output
}

/// The check-in ledger, one line per attempt in its own file (`checkin.log`
/// beside `panel.log`, same rotation rules). Panel log lines drown between
/// ingest and poll chatter; a check-in history is per-day bookkeeping and
/// wants to be findable in one place.
pub fn checkin(msg: &str) {
    let Some(base) = PATH.get() else { return };
    let path = base.with_file_name("checkin.log");
    let _guard = LOCK.lock();
    let now = Local::now();
    // Its own prefix: the shared "panel" prefix made a day-rotated checkin
    // ledger collide with panel.log's own rotation of the same day, and the
    // later rename silently destroyed the earlier generation.
    rotate_named(&path, &now, "checkin");
    let line = format!("{} {msg}\n", now.format("%Y-%m-%d %H:%M:%S"));
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn write_line(level: &str, msg: &str) {
    let Some(path) = PATH.get() else { return };
    let _guard = LOCK.lock();
    let now = Local::now();
    rotate(path, &now);
    let thread = std::thread::current();
    let line = format!(
        "{} [{level}] [pid={}] [thread={}] {msg}\n",
        now.format("%Y-%m-%d %H:%M:%S%.3f"),
        std::process::id(),
        thread.name().unwrap_or("unnamed"),
    );
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Day-crossing and flood guards, in that order. Both rename, neither errors
/// loudly: losing a rotation beats killing the thread that logs.
fn rotate(path: &Path, now: &DateTime<Local>) {
    rotate_named(path, now, "panel");
}

fn rotate_named(path: &Path, now: &DateTime<Local>, prefix: &str) {
    let today = now.format("%Y-%m-%d").to_string();
    let mtime_day = |p: &Path| {
        fs::metadata(p).ok().and_then(|m| m.modified().ok()).map(|t| {
            let t: DateTime<Local> = t.into();
            t.format("%Y-%m-%d").to_string()
        })
    };
    let mut last = LAST_DAY.lock().unwrap_or_else(|e| e.into_inner());
    let day_changed = match &*last {
        Some(d) => d != &today,
        // First write of this process: only rotate when the file on disk was
        // last written on an earlier day — a relaunch today keeps appending.
        None => match mtime_day(path) {
            Some(d) if d != today => true,
            None => {
                // No file yet: record the day so this branch runs once.
                *last = Some(today);
                return;
            }
            Some(_) => false,
        },
    };
    if day_changed {
        let old_day = last.clone().or_else(|| mtime_day(path)).unwrap_or_else(|| {
            now.date_naive()
                .checked_sub_days(chrono::Days::new(1))
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| today.clone())
        });
        let dated = path.with_file_name(format!("{prefix}-{old_day}.log"));
        let _ = fs::rename(path, &dated);
        *last = Some(today);
        drop(last);
        cleanup(path, now);
    }
    // Flood guard: a runaway writer (a broken adapter spamming one line per
    // event) must not own the disk however short the day.
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > MAX_BYTES {
            let _ = fs::rename(path, path.with_extension("log.1"));
        }
    }
}

/// Delete our dated generations past [`KEEP_DAYS`]. `scan.log`'s own rotation
/// is size-based inside scan_log.rs; its single scratch generation ages out
/// here too, so the whole directory stays bounded.
fn cleanup(path: &Path, now: &DateTime<Local>) {
    let Some(dir) = path.parent() else { return };
    let cutoff = now.timestamp_millis() - KEEP_DAYS * 86_400_000;
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = (name.starts_with("panel-") && name.ends_with(".log"))
            || name == "panel.log.1"
            || name == "scan.log.1"
            || (name.starts_with("diagnostics-") && name.ends_with(".log"))
            || name == "diagnostics.log.1"
            || (name.starts_with("checkin-") && name.ends_with(".log"))
            || name == "checkin.log.1";
        if !ours {
            continue;
        }
        let old = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|age| (age.as_millis() as i64) < cutoff)
            .unwrap_or(false);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        let at = match info.location() {
            Some(loc) => format!(" at {}:{}:{}", loc.file(), loc.line(), loc.column()),
            None => String::new(),
        };
        write_line("panic", &format!("thread={thread}{at} {info}"));
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::{diagnostics_path, DiagnosticLog};

    #[test]
    fn sync_log_reads_the_structured_sync_audit_file() {
        assert_eq!(DiagnosticLog::parse("sync"), Ok(DiagnosticLog::Sync));
        assert_eq!(DiagnosticLog::Sync.path(), diagnostics_path());
    }
}

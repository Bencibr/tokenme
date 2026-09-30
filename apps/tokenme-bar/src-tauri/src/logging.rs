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

use chrono::{DateTime, Local};

const MAX_BYTES: u64 = 2 << 20;
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

pub fn info(msg: &str) {
    write_line("info", msg);
}

pub fn error(msg: &str) {
    write_line("error", msg);
}

fn write_line(level: &str, msg: &str) {
    let Some(path) = PATH.get() else { return };
    let _guard = LOCK.lock();
    let now = Local::now();
    rotate(path, &now);
    let line = format!("{} [{level}] {msg}\n", now.format("%Y-%m-%d %H:%M:%S"));
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Day-crossing and flood guards, in that order. Both rename, neither errors
/// loudly: losing a rotation beats killing the thread that logs.
fn rotate(path: &Path, now: &DateTime<Local>) {
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
        let dated = path.with_file_name(format!("panel-{old_day}.log"));
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
            || name == "scan.log.1";
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

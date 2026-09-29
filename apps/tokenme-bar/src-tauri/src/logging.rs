//! File logging + panic capture. Dependency-free on purpose: the logger must
//! survive whatever killed the thing it is reporting on.
//!
//! One file (`panel.log`, 1 MiB, one rotated generation) under the OS log
//! directory. The panic hook writes every panic — thread, message, location —
//! before the default stderr hook runs, so a menu-bar app that dies without a
//! console still leaves a note behind.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 1 << 20;

static PATH: OnceLock<PathBuf> = OnceLock::new();
static LOCK: Mutex<()> = Mutex::new(());

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
    // Rotate once past the cap: keep exactly one generation, the log is a
    // diagnostic, not an archive.
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > MAX_BYTES {
            let _ = fs::rename(path, path.with_extension("log.1"));
        }
    }
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let line = format!("{} [{level}] {msg}\n", ts);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
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

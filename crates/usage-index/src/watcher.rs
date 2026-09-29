//! Debounced filesystem watcher.
//!
//! JSONL logs are appended token-by-token, so a raw notify stream would fire
//! hundreds of times per turn and every signal costs an ingest pass. Events are
//! coalesced into one `()` per quiet period and duplicate pending notifications
//! are dropped, so a busy machine still refreshes about once per settle-down.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use notify::{RecursiveMode, RecommendedWatcher};
// The notify trait that provides `watch`; the crate's own `Watcher` is the public one.
use notify::Watcher as _;
use usage_core::{Error, Result};

/// How long the tree must stay quiet before an ingest pass is signalled.
pub const QUIET_MS: i64 = 500;

/// Owns the notify watcher plus the coalescing thread; dropping it stops both.
pub struct Watcher {
    _watcher: RecommendedWatcher,
    stop: Arc<AtomicBool>,
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

/// `pending` is set by the notify callback and cleared by the debounce thread
/// just before it signals — that is what drops duplicate pending notifications.
struct State {
    pending: Mutex<bool>,
    wake: Condvar,
    last_event_ms: AtomicI64,
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Watcher {
    /// Watches every root recursively and pings `tx` once the tree goes quiet.
    /// A root that does not exist yet is not an error: its nearest existing
    /// ancestor is watched instead, so the source shows up as soon as the tool
    /// creates its directory.
    pub fn spawn(roots: &[PathBuf], tx: Sender<()>) -> Result<Watcher> {
        // (expected path with symlinks resolved, deepest existing ancestor to watch)
        let plans: Vec<(PathBuf, PathBuf)> = roots.iter().filter_map(|r| resolve(r)).collect();
        let expected: Vec<PathBuf> = plans.iter().map(|(e, _)| e.clone()).collect();

        let state = Arc::new(State {
            pending: Mutex::new(false),
            wake: Condvar::new(),
            last_event_ms: AtomicI64::new(0),
        });
        let callback_state = Arc::clone(&state);
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let ev = match res {
                Ok(ev) => ev,
                Err(_) => return,
            };
            // Ancestor watching is deliberately coarse — `$HOME` is a
            // legitimate stand-in for a not-yet-created `~/.claude/projects` —
            // so everything reported is filtered against the declared roots.
            if !ev.paths.iter().any(|p| relevant(&expected, p)) {
                return;
            }
            callback_state.last_event_ms.store(epoch_ms(), Ordering::SeqCst);
            let mut pending = lock(&callback_state.pending);
            if !*pending {
                *pending = true;
                callback_state.wake.notify_all();
            }
        })
        .map_err(|e| {
            Error::io(
                roots.first().cloned().unwrap_or_default(),
                std::io::Error::other(e),
            )
        })?;

        let mut watched: Vec<&PathBuf> = Vec::new();
        for (_, target) in &plans {
            if target.parent().is_none() {
                // Never install a machine-wide watch on the filesystem root; a
                // later `spawn` after the tool creates its dir is the fix.
                continue;
            }
            if watched.contains(&target) {
                continue;
            }
            let mode = if std::fs::metadata(target).map(|m| m.is_dir()).unwrap_or(true) {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            // A quota-exhausted inotify set or a racing delete must not make the
            // whole watcher unusable; ingest still runs on the app's timer.
            let _ = watcher.watch(target, mode);
            watched.push(target);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("tokenme-watch".into())
            .spawn({
                let state = Arc::clone(&state);
                let stop = Arc::clone(&stop);
                move || debounce(state, stop, tx)
            })
            .map_err(|e| Error::io(PathBuf::new(), std::io::Error::other(e)))?;
        Ok(Watcher { _watcher: watcher, stop, state, thread: Some(thread) })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.state.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn debounce(state: Arc<State>, stop: Arc<AtomicBool>, tx: Sender<()>) {
    let mut guard = lock(&state.pending);
    loop {
        // The timeout is only a safety net against a lost notification.
        while !*guard && !stop.load(Ordering::SeqCst) {
            let (g, _) = state
                .wake
                .wait_timeout(guard, Duration::from_millis(QUIET_MS as u64))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = g;
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        loop {
            let idle = epoch_ms().saturating_sub(state.last_event_ms.load(Ordering::SeqCst));
            if idle >= QUIET_MS {
                break;
            }
            let (g, _) = state
                .wake
                .wait_timeout(guard, Duration::from_millis((QUIET_MS - idle).max(1) as u64))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = g;
            if stop.load(Ordering::SeqCst) {
                return;
            }
        }
        // Cleared before sending, so an event racing the send re-arms the
        // pending flag rather than being swallowed.
        *guard = false;
        drop(guard);
        if tx.send(()).is_err() {
            return;
        }
        guard = lock(&state.pending);
    }
}

/// Resolves a declared root to `(expected, watch_target)`: the root itself with
/// as much of its prefix symlink-resolved as exists today, plus the deepest
/// ancestor that can actually be watched right now.
fn resolve(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = root;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(cur) {
            let mut expected = canonical;
            for part in suffix.iter().rev() {
                expected.push(part);
            }
            return Some((expected, cur.to_path_buf()));
        }
        suffix.push(cur.file_name()?.to_os_string());
        cur = cur.parent()?;
    }
}

/// Deepest existing ancestor with symlinks resolved (the path itself when it
/// exists). Both sides of every comparison go through here because backends
/// report canonical paths — `/private/var/…` for a `/var/…` temp dir.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut cur = path;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(cur) {
            return Some(canonical);
        }
        cur = cur.parent()?;
    }
}

fn relevant(expected: &[PathBuf], path: &Path) -> bool {
    let Some(resolved) = nearest_existing(path) else {
        return false;
    };
    expected.iter().any(|e| resolved.starts_with(e) || e.starts_with(&resolved))
}

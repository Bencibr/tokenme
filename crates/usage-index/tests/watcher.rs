//! End-to-end checks for the debounced watcher. These exercise real filesystem
//! events, so they are the slowest tests here; run with `--nocapture` when a
//! platform backend misbehaves. They share one serial lock: on CI's busy
//! runner the backend's event delivery bleeds across simultaneous watchers on
//! sibling tempdirs, and a "stays silent" assertion then fails on someone
//! else's traffic.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use usage_index::Watcher;

fn signal_within(rx: &mpsc::Receiver<()>, budget: Duration) -> bool {
    matches!(rx.recv_timeout(budget), Ok(()))
}

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn touching_a_watched_file_signals_once() {
    let _serial = serial();
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("rollout.jsonl");
    std::fs::write(&file, b"{}\n").unwrap();

    let (tx, rx) = mpsc::channel();
    let _watcher = Watcher::spawn(&[dir.path().to_path_buf()], tx).unwrap();

    for i in 0..6 {
        std::fs::write(&file, format!("{{}}\n{i}\n").into_bytes()).unwrap();
    }
    assert!(signal_within(&rx, Duration::from_secs(3)), "no signal within 3s");
    // The burst must collapse: a quiet period of 500ms plus a little slack is
    // long enough to see whether a second signal was queued behind the first.
    std::thread::sleep(Duration::from_millis(900));
    assert!(rx.try_recv().is_err(), "the burst produced more than one coalesced signal");
}

#[test]
fn a_root_that_does_not_exist_yet_is_watched_through_its_ancestor() {
    let _serial = serial();
    let dir = tempfile::TempDir::new().unwrap();
    let future = dir.path().join(".claude-state/projects");

    let (tx, rx) = mpsc::channel();
    let roots = [future.clone()];
    let _watcher = Watcher::spawn(&roots, tx).unwrap();

    std::fs::create_dir_all(&future).unwrap();
    std::fs::write(future.join("a.jsonl"), b"{}\n").unwrap();
    assert!(signal_within(&rx, Duration::from_secs(3)), "late-created root produced no signal");
}

#[test]
fn activity_outside_the_roots_stays_silent() {
    let _serial = serial();
    let watched = tempfile::TempDir::new().unwrap();
    let other = tempfile::TempDir::new().unwrap();
    let (tx, rx) = mpsc::channel();
    let _watcher = Watcher::spawn(&[watched.path().to_path_buf()], tx).unwrap();
    // FSEvents timestamps are coarse: the watched dir's own creation (seconds
    // before the stream started) can be delivered as a "since now" event on a
    // busy machine, and the debounce turns it into a queued signal. Drain the
    // queue until it goes quiet BEFORE writing, or the phantom sits in the
    // channel and the assertion reads it as "unrelated writes signalled".
    // Bounded: a watcher that genuinely cannot stay quiet is a real bug the
    // assertion below must catch, not an infinite drain. The budget covers a
    // cold CI VM, where FSEvents' first delivery has been measured past 1.5s.
    for _ in 0..12 {
        if !signal_within(&rx, Duration::from_millis(300)) {
            break;
        }
    }

    std::fs::write(other.path().join("else.jsonl"), b"{}\n").unwrap();
    std::fs::write(other.path().join("more.jsonl"), b"{}\n").unwrap();
    assert!(!signal_within(&rx, Duration::from_millis(1200)), "unrelated writes signalled");
}

#[test]
fn dropping_the_watcher_stops_the_thread() {
    let _serial = serial();
    let dir = tempfile::TempDir::new().unwrap();
    let (tx, rx) = mpsc::channel::<()>();
    let roots: Vec<PathBuf> = vec![dir.path().to_path_buf()];
    {
        let watcher = Watcher::spawn(&roots, tx).unwrap();
        drop(watcher);
    }
    std::fs::write(dir.path().join("late.jsonl"), b"{}\n").unwrap();
    assert!(!signal_within(&rx, Duration::from_millis(900)), "a dropped watcher still signals");
}

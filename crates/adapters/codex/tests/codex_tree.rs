//! `discover` / `probe` against a synthetic `$CODEX_HOME`, including the
//! date-directory pruning that bounds the first index pass.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use usage_adapter_codex::CodexAdapter;
use usage_core::{DateFilter, SourceAdapter};

/// `set_var` is process-global, so the tests in this binary run one at a time.
static HOME_LOCK: Mutex<()> = Mutex::new(());

const ROLLING: &str = concat!(
    r#"{"timestamp":"2026-09-23T11:32:13.873Z","ordinal":0,"type":"session_meta","payload":{"session_id":"s","cwd":"/w","cli_version":"0.155.1"}}"#,
    "\n",
    r#"{"timestamp":"2026-09-23T11:32:31.944Z","ordinal":1,"type":"turn_context","payload":{"turn_id":"t","cwd":"/w","model":"gpt-5.6-luna"}}"#,
    "\n",
    r#"{"timestamp":"2026-09-23T11:32:32.000Z","ordinal":2,"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":5,"total_tokens":105}}}}"#,
    "\n"
);

fn touch(dir: &Path, rel: &str) -> PathBuf {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, ROLLING).unwrap();
    p
}

fn home_with_tree(base: &Path) -> PathBuf {
    let home = base.join(".codex");
    // Both real spellings of the date partition.
    touch(&home, "sessions/2026-09-23/rollout-2026-09-23T11-32-11-00000000-0000-4000-8000-0000000000a1.jsonl");
    touch(&home, "sessions/2026/09/23/rollout-2026-09-23T12-00-00-00000000-0000-4000-8000-0000000000a2.jsonl");
    touch(&home, "sessions/2026/08/01/rollout-2026-08-01T09-00-00-00000000-0000-4000-8000-0000000000a3.jsonl");
    touch(&home, "sessions/2025/12/31/rollout-2025-12-31T23-00-00-00000000-0000-4000-8000-0000000000a4.jsonl");
    // Archived copies are duplicates of live sessions (14 of 17 on this machine):
    // reading both would double-count, so `sessions/` is the only root.
    touch(&home, "archived_sessions/rollout-2026-09-23T11-32-11-00000000-0000-4000-8000-0000000000a1.jsonl");
    // Not a log at all.
    std::fs::write(home.join("sessions/2026/09/23/notes.md"), b"x").unwrap();
    home
}

#[test]
fn discover_prunes_whole_date_directories() {
    let _g = HOME_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let home = home_with_tree(tmp.path());
    std::env::set_var("CODEX_HOME", &home);
    let a = CodexAdapter;

    let all = a.discover(&DateFilter::default());
    assert_eq!(all.len(), 4, "everything under sessions/, nothing from archived_sessions/");
    assert!(all.iter().all(|f| f.path.starts_with(home.join("sessions"))));
    assert!(all.iter().all(|f| f.size > 0 && f.mtime_ms > 0));
    let paths: Vec<PathBuf> = all.iter().map(|f| f.path.clone()).collect();
    assert!(paths.windows(2).all(|w| w[0] <= w[1]), "sorted by path for a stable manifest");
    assert_eq!(paths.len(), 4);

    let since = DateFilter::new(Some(ms(2026, 9, 20)), None);
    let recent = a.discover(&since);
    assert_eq!(recent.len(), 2, "the 2025 and 2026/08 day directories are skipped whole");
    assert!(recent.iter().all(|f| f.path.to_string_lossy().contains("09-23") || f.path.to_string_lossy().contains("09/23")));

    let narrow = DateFilter::new(Some(ms(2026, 8, 1) + 3_600_000), Some(ms(2026, 8, 30)));
    let one: Vec<PathBuf> = a.discover(&narrow).into_iter().map(|f| f.path).collect();
    assert_eq!(one.len(), 1, "August alone, and the year/month chains prune around it");
    let august_day = Path::new("2026").join("08").join("01");
    assert!(one[0].parent().is_some_and(|parent| parent.ends_with(&august_day)));

    std::env::remove_var("CODEX_HOME");
}

#[test]
fn probe_short_circuits_and_reports_the_root() {
    let _g = HOME_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let home = home_with_tree(tmp.path());
    std::env::set_var("CODEX_HOME", &home);
    let a = CodexAdapter;

    let d = a.probe().expect("codex detected");
    assert_eq!(d.id, "codex");
    assert_eq!(d.display, "Codex");
    assert_eq!(d.roots, vec![home.join("sessions")]);
    assert_eq!(d.hint.as_deref(), Some("cli 0.155.1"));

    // No sessions directory at all -> not detected.
    std::env::set_var("CODEX_HOME", tmp.path().join("nowhere"));
    assert!(a.probe().is_none());
    assert!(a.discover(&DateFilter::default()).is_empty());
    std::env::remove_var("CODEX_HOME");
}

fn ms(y: i32, mo: u32, d: u32) -> i64 {
    chrono::Local
        .with_ymd_and_hms(y, mo, d, 0, 0, 0)
        .single()
        .expect("local day exists")
        .timestamp_millis()
}

use chrono::TimeZone as _;

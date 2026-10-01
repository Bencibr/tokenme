//! Public-surface tests for the cline adapter: everything `usage-adapter-all`
//! and the CLI touch, driven through the [`SourceAdapter`] trait only.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use usage_adapter_cline::{ClineAdapter, TOOL_ID};
use usage_core::{
    DateFilter, Error, FileKind, Meter, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile, UsageForm,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn tree_file(path: PathBuf) -> SourceFile {
    let meta = std::fs::metadata(&path).unwrap();
    let mtime_ms = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    SourceFile { path, kind: FileKind::Tree, size: meta.len(), mtime_ms }
}

/// Env vars are process-global.
fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn public_surface_is_frozen() {
    let adapter = ClineAdapter;
    assert_eq!(TOOL_ID, "cline");
    assert_eq!(adapter.id(), "cline");
    assert_eq!(adapter.display_name(), "Cline");
    // Mirrors `ClineAdapter::semantics()` in src/lib.rs: per-call token usage,
    // model on the same record, ids usable for dedupe, no quota window.
    assert_eq!(
        adapter.semantics(),
        Semantics {
            usage_form: UsageForm::PerCall,
            meter: Meter::Tokens,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    );
    let cloned: ClineAdapter = Default::default();
    assert_eq!(cloned.id(), adapter.id(), "Copy + Default derive");
    assert!(format!("{cloned:?}").contains("ClineAdapter"));
}

#[test]
fn reading_a_rewritten_transcript_twice_yields_the_same_events() {
    let adapter = ClineAdapter;
    let file = tree_file(fixture("mixed-rows.messages.json"));
    let first = adapter.read(&file, ReadCursor(0)).unwrap();
    let second = adapter.read(&file, first.cursor).unwrap();
    assert_eq!(first.events.len(), 5);
    assert_eq!(first.events, second.events, "Tree re-parses everything, so the dedupe keys are what keep the index stable");
    let keys: Vec<&str> = first.events.iter().filter_map(|e| e.dedupe_key.as_deref()).collect();
    assert_eq!(keys.len(), first.events.len());
    assert_eq!(keys[0], "3_ts_mixed#msg_ok", "<sessionId>#<message id>");
    // The repeated id inside one session collapses to one key; the undated row and
    // the metric-less row never became events at all.
    assert_eq!(keys[3], "3_ts_mixed#msg_dup");
    assert_eq!(first.cursor, ReadCursor(file.size));
}

#[test]
fn the_same_message_id_in_two_sessions_stays_two_events() {
    // The dedupe index is global, so without the session prefix one of these
    // would be dropped silently.
    let adapter = ClineAdapter;
    let dir = fixture_dir();
    let mut keys = Vec::new();
    for session in ["1_ts_a", "2_ts_b"] {
        let file = tree_file(dir.join(format!("{session}.messages.json")));
        let out = adapter.read(&file, ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 1, "{}", file.path.display());
        keys.push(out.events[0].dedupe_key.clone().unwrap());
        assert_eq!(out.events[0].session, session);
    }
    assert_eq!(keys, vec!["1_ts_a#msg_dup".to_string(), "2_ts_b#msg_dup".to_string()]);
    assert_ne!(keys[0], keys[1]);
}

fn fixture_dir() -> PathBuf {
    fixture("duplicate-id-across-sessions")
}

#[test]
fn probe_and_discover_respect_the_env_override_and_find_the_label() {
    let _env = lock_env();
    let dir = tempfile::tempdir().unwrap();
    let sess = dir.path().join("1788497024441_2hl7c");
    std::fs::create_dir_all(&sess).unwrap();
    let messages = sess.join("1788497024441_2hl7c.messages.json");
    std::fs::write(&messages, std::fs::read(fixture("mixed-rows.messages.json")).unwrap()).unwrap();
    std::fs::write(sess.join("1788497024441_2hl7c.json"), br#"{"cwd":"/Users/dev/workspace/tunnel","metadata":{"usage":{"inputTokens":1,"aggregateUsage":{}}}}"#).unwrap();
    std::env::set_var("CLINE_DATA_DIR_SESSIONS", dir.path());

    let adapter = ClineAdapter;
    let detected = adapter.probe().expect("the overridden root is readable");
    assert_eq!(detected.id, "cline");
    assert_eq!(detected.display, "Cline");
    assert_eq!(detected.roots, vec![dir.path().to_path_buf()]);
    assert!(!detected.roots[0].join("nonexistent").exists(), "probe hands back the root, not a walk result");

    let files = adapter.discover(&DateFilter::default());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].kind, FileKind::Tree);
    let out = adapter.read(&files[0], ReadCursor(0)).unwrap();
    assert_eq!(out.events.len(), 5);
    assert!(out.events.iter().all(|e| e.project.as_deref() == Some("tunnel")));
    assert!(out.events.iter().all(|e| e.session == "3_ts_mixed"), "the transcript's sessionId wins over the directory name");
    assert!(out.events.iter().all(|e| e.tool == "cline" && e.meter == Meter::Tokens));
    std::env::remove_var("CLINE_DATA_DIR_SESSIONS");
    let _ = adapter; // the real profile is the ignored smoke test's business
}

#[test]
fn broken_files_never_panic_and_never_bill() {
    let adapter = ClineAdapter;
    for name in ["malformed.messages.json", "not-an-object.messages.json", "null-messages.messages.json"] {
        let file = tree_file(fixture(name));
        let out: ReadOutcome = adapter.read(&file, ReadCursor(0)).unwrap();
        assert!(out.events.is_empty(), "{name}: {:?}", out.events);
        assert_eq!(out.cursor, ReadCursor(file.size), "{name}: a readable file is consumed even when it is junk");
    }
    // Unreadable: the cursor freezes so the indexer retries.
    let missing = SourceFile { path: fixture("gone.messages.json"), kind: FileKind::Tree, size: 100, mtime_ms: 0 };
    let out = adapter.read(&missing, ReadCursor(9)).unwrap();
    assert!(out.events.is_empty() && out.cursor == ReadCursor(9));
    // The one error case.
    assert!(matches!(adapter.read(&missing, ReadCursor(101)), Err(Error::Cursor { .. })));
}

#[test]
fn money_never_comes_from_the_source() {
    let adapter = ClineAdapter;
    let file = tree_file(fixture("cline-djat1.messages.json"));
    let out = adapter.read(&file, ReadCursor(0)).unwrap();
    assert_eq!(out.events.len(), 96);
    assert!(out.events.iter().all(|e| e.counts.credits == 0.0 && e.quota.is_none()));
    assert!(out.events.iter().all(|e| e.ts_ms > 1_700_000_000_000), "ts stays in milliseconds");
    assert!(out.events.iter().any(|e| e.counts.cache_read > 0.0));
    let prompt: f64 = out.events.iter().map(|e| e.counts.input + e.counts.cache_read).sum();
    assert_eq!(prompt, 11_816_157.0, "inputTokens already contains cacheReadTokens");
    let models: Vec<&str> = {
        let mut v: Vec<&str> = out.events.iter().filter_map(|e| e.model.as_deref()).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    assert_eq!(models, ["deepseek/deepseek-v4-flash"], "ids stay verbatim, prefix included");
    assert!(Path::new(&fixture("cline-djat1.json")).exists(), "the label file is a fixture too");
}

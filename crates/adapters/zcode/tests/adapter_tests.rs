//! Public-surface tests for the zcode adapter: what `usage-adapter-all` and the
//! CLI touch, driven through the [`SourceAdapter`] trait only.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use usage_adapter_zcode::{ZcodeAdapter, TOOL_ID};
use usage_core::{
    DateFilter, Error, FileKind, Meter, ReadCursor, Semantics, SourceAdapter, SourceFile, UsageForm,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn source_file(path: PathBuf, kind: FileKind) -> SourceFile {
    let meta = std::fs::metadata(&path).unwrap();
    let mtime_ms = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    SourceFile { path, kind, size: meta.len(), mtime_ms }
}

/// Env vars are process-global.
fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A temp `~/.zcode` with the real fixtures installed under `cli/`.
fn temp_home(with_db: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(cli.join("rollout")).unwrap();
    std::fs::create_dir_all(cli.join("db")).unwrap();
    std::fs::write(cli.join("rollout/model-io-sess_7de7ea43.jsonl"), std::fs::read(fixture("rollout-real.jsonl")).unwrap()).unwrap();
    std::fs::write(cli.join("rollout/model-io-sess_subagent_agent_bd60.jsonl"), std::fs::read(fixture("rollout-subagent.jsonl")).unwrap()).unwrap();
    std::fs::write(cli.join("rollout/zcode.log"), b"not ours\n").unwrap();
    if with_db {
        std::fs::copy(fixture("model_usage.sqlite"), cli.join("db/db.sqlite")).unwrap();
    }
    dir
}

#[test]
fn public_surface_is_frozen() {
    let adapter = ZcodeAdapter;
    assert_eq!(TOOL_ID, "zcode");
    assert_eq!(adapter.id(), "zcode");
    assert_eq!(adapter.display_name(), "ZCode");
    // Mirrors `ZcodeAdapter::semantics()` in src/lib.rs: one row or record per
    // billed call, model on that same row, ids usable for dedupe, no quota.
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
    let cloned: ZcodeAdapter = Default::default();
    assert_eq!(cloned.id(), adapter.id(), "Copy + Default derive");
    assert!(format!("{cloned:?}").contains("ZcodeAdapter"));
}

#[test]
fn the_db_is_the_source_and_carries_the_project_label() {
    let _env = lock_env();
    let home = temp_home(true);
    std::env::set_var("ZCODE_HOME", home.path());
    let adapter = ZcodeAdapter;

    let detected = adapter.probe().expect("the overridden root is readable");
    assert_eq!((detected.id.as_str(), detected.display.as_str()), ("zcode", "ZCode"));
    assert_eq!(detected.roots, vec![home.path().join("cli")]);
    assert!(detected.hint.unwrap().contains("model_usage"));

    let files = adapter.discover(&DateFilter::default());
    assert_eq!(files.len(), 1, "the rollout files stay unread while the db is live: {files:?}");
    assert_eq!(files[0].kind, FileKind::Sqlite);
    let out = adapter.read(&files[0], ReadCursor(0)).unwrap();
    assert_eq!(out.events.len(), 8, "the zero-usage error row is not billable");
    assert_eq!(out.cursor, ReadCursor(9), "the cursor is the highest consumed rowid");
    let resumed = adapter.read(&files[0], out.cursor).unwrap();
    assert!(resumed.events.is_empty() && resumed.cursor == out.cursor, "a re-read of the same rows is a no-op");
    let mut projects: Vec<&str> = out.events.iter().filter_map(|e| e.project.as_deref()).collect();
    projects.sort_unstable();
    projects.dedup();
    // Only two of the three sessions bill anything: the apppty row is the
    // zero-token `status = error` one, which never becomes an event.
    assert_eq!(projects, ["WorkFile", "bug-hunter"], "basename of session.directory");
    assert!(out.events.iter().all(|e| e.tool == "zcode" && e.meter == Meter::Tokens && e.counts.credits == 0.0));
    assert!(out.events.iter().all(|e| e.dedupe_key.as_ref().is_some_and(|k| k.matches('#').count() == 2)), "<session>#<requestId>#<attempt>");
    let input: f64 = out.events.iter().map(|e| e.counts.input).sum();
    let cached: f64 = out.events.iter().map(|e| e.counts.cache_read).sum();
    assert_eq!((input, cached), (18_998.0, 477_888.0), "the db folds the cached prefix into input_tokens; we net it back out");
    std::env::remove_var("ZCODE_HOME");
}

#[test]
fn the_rollout_fallback_keeps_every_attempt_separate() {
    let _env = lock_env();
    let home = temp_home(false);
    std::env::set_var("ZCODE_HOME", home.path());
    let adapter = ZcodeAdapter;

    let detected = adapter.probe().expect("rollout only");
    assert!(detected.hint.unwrap().contains("rollout"));
    let files = adapter.discover(&DateFilter::default());
    assert_eq!(files.len(), 2, "both layouts, nothing else: {files:?}");
    assert!(files.iter().all(|f| f.kind == FileKind::Jsonl));

    let out = adapter.read(&files[0], ReadCursor(0)).unwrap();
    assert_eq!(out.events.len(), 8, "one non-usage record and one torn line drop out");
    assert_ne!(out.cursor, ReadCursor(files[0].size), "the torn tail is left for the next pass");
    assert_eq!(out.events[0].dedupe_key.as_deref().unwrap(), "sess_7de7ea43-96fe-4efe-9bdb-4afd85127203#b0df2753-2722-4c18-a743-748c9c3fbdca#1");
    assert!(out.events.iter().all(|e| e.project.is_none()), "the rollout log carries no cwd");
    // Provider numbers are already disjoint stages, so nothing is netted here.
    assert_eq!(out.events[0].counts.input, 702.0);
    assert_eq!(out.events[0].counts.cache_read, 381_888.0);
    assert!(out.events.iter().all(|e| e.counts.input < e.counts.cache_read));

    let sub = adapter.read(&files[1], ReadCursor(0)).unwrap();
    assert_eq!(sub.events.len(), 3);
    assert_eq!(sub.events[0].session, "sess_subagent_agent_bd60e736-e6f4-495f-bb9d-6458cdc9db73");
    assert_eq!(sub.events[0].model.as_deref(), Some("GLM-5.3-Flash"), "verbatim model id");

    // The retry pair: two attempts of one requestId are two billed calls.
    let retry = source_file(fixture("rollout-retry.jsonl"), FileKind::Jsonl);
    let out = adapter.read(&retry, ReadCursor(0)).unwrap();
    assert_eq!(out.events.len(), 3);
    let keys: Vec<String> = out.events.iter().filter_map(|e| e.dedupe_key.clone()).collect();
    assert_eq!(&keys[..], &["sess_retry_probe#req_retry_probe#1".to_string(), "sess_retry_probe#req_retry_probe#2".to_string(), "sess_retry_probe#req_retry_probe#2".to_string()]);
    std::env::remove_var("ZCODE_HOME");
}

#[test]
fn cursor_discipline_and_broken_inputs() {
    let adapter = ZcodeAdapter;
    // A garbage-only rollout file reads clean and bills nothing.
    let junk = source_file(fixture("rollout-garbage.jsonl"), FileKind::Jsonl);
    let out = adapter.read(&junk, ReadCursor(0)).unwrap();
    assert!(out.events.is_empty(), "{:?}", out.events);
    // Unreadable file: cursor frozen, no error, no panic.
    let missing = SourceFile { path: fixture("absent.jsonl"), kind: FileKind::Jsonl, size: 10, mtime_ms: 0 };
    let out = adapter.read(&missing, ReadCursor(3)).unwrap();
    assert!(out.events.is_empty() && out.cursor == ReadCursor(3));
    // Past EOF is the one error, for both kinds. A file that cannot be opened at
    // all is not an error: the cursor simply freezes.
    assert!(matches!(adapter.read(&junk, ReadCursor(junk.size + 1)), Err(Error::Cursor { .. })));
    let db = source_file(fixture("model_usage.sqlite"), FileKind::Sqlite);
    assert!(matches!(adapter.read(&db, ReadCursor(10)), Err(Error::Cursor { .. })));
    // A foreign database yields nothing.
    let other = source_file(fixture("build.py"), FileKind::Sqlite);
    let out = adapter.read(&other, ReadCursor(0)).unwrap();
    assert!(out.events.is_empty() && out.cursor == ReadCursor(0), "not a model_usage db: {out:?}");
    assert!(Path::new(&fixture("rollout-real.jsonl")).exists());
}

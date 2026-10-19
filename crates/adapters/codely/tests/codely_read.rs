//! End-to-end reads against a Codely snapshot tree.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use usage_adapter_codely::{CodelyAdapter, TOOL_ID};
use usage_core::{FileKind, Meter, ReadCursor, Semantics, SourceAdapter};

/// `set_var` is process-global, so the tests that pin a root run one at a time.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const ENV: &str = "CODELY_CLI_HOME";

fn message(id: &str, stamp: &str, model: &str, input: u64, output: u64, cached: u64, thoughts: u64) -> Value {
    json!({
        "id": id,
        "timestamp": stamp,
        "type": "gemini",
        "model": model,
        "tokens": {
            "input": input,
            "output": output,
            "cached": cached,
            "thoughts": thoughts,
            "tool": 0,
            "total": input + output
        }
    })
}

fn snapshot(messages: Vec<Value>) -> String {
    json!({
        "tag": "sess-1",
        "timestamp": "2026-09-14T13:00:07.000Z",
        "type": "manual",
        "clientHistory": [{
            "role": "user",
            "parts": [{
                "text": "I'm currently working in the directory: C:/Projects/demo\nHere is the folder structure"
            }]
        }],
        "conversationRecord": {
            "sessionId": "sess-1",
            "projectHash": "demo-hash",
            "startTime": "2026-09-14T13:00:00.000Z",
            "lastUpdated": "2026-09-14T13:00:07.000Z",
            "messages": messages
        }
    })
    .to_string()
}

/// A pinned `~/.codely-cli` with one project directory under `tmp`.
struct Tree {
    dir: tempfile::TempDir,
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tmp").join("demo-hash")).unwrap();
        Self { dir }
    }

    fn home(&self) -> &Path {
        self.dir.path()
    }

    fn session_path(&self) -> PathBuf {
        self.home().join("tmp").join("demo-hash").join("chat-sess-1.json")
    }

    fn write(&self, body: &str) {
        std::fs::write(self.session_path(), body).unwrap();
    }
}

fn read_all() -> Vec<usage_core::UsageEvent> {
    let mut events = Vec::new();
    for file in CodelyAdapter.discover(&usage_core::DateFilter::default()) {
        let outcome = CodelyAdapter.read(&file, ReadCursor(0)).unwrap();
        assert_eq!(outcome.cursor.0, file.size, "a Tree read consumes the whole snapshot");
        events.extend(outcome.events);
    }
    events
}

#[test]
fn one_snapshot_bills_each_call_once_with_its_labels() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tree = Tree::new();
    // A stray file beside the snapshots must not become a source.
    std::fs::write(tree.home().join("tmp").join("demo-hash").join("logs.json"), "{}").unwrap();
    tree.write(&snapshot(vec![
        json!({"id": "user-1", "timestamp": "2026-09-14T13:00:00.000Z", "type": "user", "content": [{"text": "hi"}]}),
        message("m1", "2026-09-14T13:00:05.000Z", "codely-core", 1000, 50, 400, 0),
        json!({"id": "m2", "timestamp": "2026-09-14T13:00:06.000Z", "type": "gemini", "model": "GLM-5.3", "tokens": null}),
        message("m3", "2026-09-14T13:00:07.000Z", "GLM-5.3", 500, 20, 0, 5),
    ]));
    std::env::set_var(ENV, tree.home());

    let files = CodelyAdapter.discover(&usage_core::DateFilter::default());
    assert_eq!(files.len(), 1, "only chat-*.json is a source");
    assert_eq!(files[0].kind, FileKind::Tree);
    assert_eq!(CodelyAdapter.semantics(), Semantics {
        usage_form: usage_core::UsageForm::PerCall,
        meter: usage_core::Meter::Tokens,
        model_attr: usage_core::ModelAttr::Inline,
        dedupes_by_id: true,
        reports_quota: false,
    });

    let events = read_all();
    assert_eq!(events.len(), 2, "the user turn and the null-tokens turn are not calls");
    assert!(events.iter().all(|event| event.tool == TOOL_ID));
    assert!(events.iter().all(|event| event.meter == Meter::Tokens));
    assert!(events.iter().all(|event| event.project.as_deref() == Some("demo")));
    assert!(events.iter().all(|event| event.session == "sess-1"));
    assert_eq!(events[0].model.as_deref(), Some("codely-core"));
    assert_eq!(events[1].model.as_deref(), Some("GLM-5.3"));
    assert_eq!(events[0].counts.input, 600.0);
    assert_eq!(events[0].counts.cache_read, 400.0);
    assert_eq!(events[0].counts.total(), 1050.0);
    assert_eq!(events[1].counts.reasoning, 5.0);
    assert_eq!(events[1].counts.output, 20.0, "reasoning is a sub-breakdown, not extra output");
    assert_eq!(events[0].dedupe_key.as_deref(), Some("codely#sess-1#m1"));
    assert_eq!(events[1].dedupe_key.as_deref(), Some("codely#sess-1#m3"));
    assert_eq!(events[0].ts_ms, usage_core::parse_ts_ms("2026-09-14T13:00:05.000Z").unwrap());

    let probe = CodelyAdapter.probe().expect("a snapshot makes the tool detected");
    assert_eq!(probe.id, TOOL_ID);
    assert_eq!(probe.roots, vec![tree.home().join("tmp")]);

    std::env::remove_var(ENV);
}

#[test]
fn a_rewritten_snapshot_replays_the_same_identities_and_adds_only_the_new_call() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tree = Tree::new();
    std::env::set_var(ENV, tree.home());

    tree.write(&snapshot(vec![message("m1", "2026-09-14T13:00:05.000Z", "codely-core", 1000, 50, 400, 0)]));
    let first = read_all();
    assert_eq!(first.len(), 1);

    tree.write(&snapshot(vec![
        message("m1", "2026-09-14T13:00:05.000Z", "codely-core", 1000, 50, 400, 0),
        message("m2", "2026-09-14T13:00:09.000Z", "GLM-5.3", 90, 7, 0, 0),
    ]));
    let second = read_all();
    assert_eq!(second.len(), 2);
    assert_eq!(second[0], first[0], "the already-seen call keeps every field");
    assert_eq!(second[1].dedupe_key.as_deref(), Some("codely#sess-1#m2"));
    assert_eq!(
        second.iter().map(|event| event.dedupe_key.clone()).collect::<std::collections::HashSet<_>>().len(),
        2,
        "no two calls share a key"
    );

    let third = read_all();
    assert_eq!(third, second, "reading unchanged bytes is a no-op");
    std::env::remove_var(ENV);
}

#[test]
fn an_absent_install_probes_as_not_installed_and_a_shrunk_file_is_the_cursor_error() {
    let _guard = ENV_LOCK.lock().unwrap();
    let empty = tempfile::tempdir().unwrap();
    std::env::set_var(ENV, empty.path());
    assert!(CodelyAdapter.probe().is_none());
    assert!(CodelyAdapter.discover(&usage_core::DateFilter::default()).is_empty());

    let tree = Tree::new();
    std::env::set_var(ENV, tree.home());
    tree.write(&snapshot(vec![message("m1", "2026-09-14T13:00:05.000Z", "codely-core", 10, 1, 0, 0)]));
    let file = CodelyAdapter.discover(&usage_core::DateFilter::default()).remove(0);
    assert!(matches!(
        CodelyAdapter.read(&file, ReadCursor(file.size + 1)),
        Err(usage_core::Error::Cursor { .. })
    ));

    // The date filter is an upper bound on the file's mtime.
    assert!(CodelyAdapter
        .discover(&usage_core::DateFilter::new(None, Some(1)))
        .is_empty());
    std::env::remove_var(ENV);
}

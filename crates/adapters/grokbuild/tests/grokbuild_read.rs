//! End-to-end reads against a pinned Grok Build sessions tree.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use usage_adapter_grokbuild::{GrokBuildAdapter, TOOL_ID};
use usage_core::{DateFilter, FileKind, Meter, ReadCursor, SourceAdapter};

static ENV_LOCK: Mutex<()> = Mutex::new(());

const ENV: &str = "GROK_HOME";

struct Tree {
    dir: tempfile::TempDir,
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let session = dir
            .path()
            .join("sessions")
            .join("C%3A%5Cproj")
            .join("sess-1");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(
            session.join("summary.json"),
            r#"{"info":{"id":"sess-1","cwd":"C:\\proj"},"generated_title":"demo"}"#,
        )
        .unwrap();
        Self { dir }
    }

    fn home(&self) -> &Path {
        self.dir.path()
    }

    fn usage_path(&self) -> PathBuf {
        self.home()
            .join("sessions")
            .join("C%3A%5Cproj")
            .join("sess-1")
            .join("usage.json")
    }

    fn write_usage(&self, body: &str) {
        std::fs::write(self.usage_path(), body).unwrap();
    }
}

fn read_all() -> Vec<usage_core::UsageEvent> {
    let adapter = GrokBuildAdapter;
    let mut events = Vec::new();
    for file in adapter.discover(&DateFilter::default()) {
        let outcome = adapter.read(&file, ReadCursor(0)).unwrap();
        assert_eq!(file.kind, FileKind::Tree);
        events.extend(outcome.events);
    }
    events
}

#[test]
fn discovers_and_reads_turns_with_project_and_stable_keys() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tree = Tree::new();
    // SAFETY: serialised by ENV_LOCK; restored below.
    unsafe { std::env::set_var(ENV, tree.home()) };

    tree.write_usage(
        r#"{
          "sessionId": "sess-1",
          "updatedAt": "2026-11-03T16:53:50.025Z",
          "session": {
            "inputTokens": 300,
            "outputTokens": 40,
            "cachedReadTokens": 50,
            "cacheCreationTokens": 10,
            "reasoningTokens": 5,
            "primaryModelId": "grok-4.7-build"
          },
          "turns": [
            {
              "turnNumber": 1,
              "endedAt": "2026-11-03T16:50:00.000Z",
              "inputTokens": 100,
              "outputTokens": 10,
              "cachedReadTokens": 20,
              "cacheCreationTokens": 0,
              "reasoningTokens": 2,
              "primaryModelId": "grok-4.7-build"
            },
            {
              "turnNumber": 2,
              "endedAt": "2026-11-03T16:53:50.025Z",
              "inputTokens": 200,
              "outputTokens": 30,
              "cachedReadTokens": 30,
              "cacheCreationTokens": 10,
              "reasoningTokens": 3,
              "primaryModelId": "grok-4.7"
            }
          ]
        }"#,
    );

    let events = read_all();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].tool, TOOL_ID);
    assert_eq!(events[0].meter, Meter::Tokens);
    assert_eq!(events[0].project.as_deref(), Some(r"C:\proj"));
    assert_eq!(events[0].counts.input, 80.0);
    assert_eq!(events[0].counts.cache_read, 20.0);
    assert_eq!(events[1].counts.input, 160.0);
    assert_eq!(events[1].counts.cache_creation, 10.0);
    assert_eq!(events[1].counts.cache_read, 30.0);
    assert_eq!(
        events[0].dedupe_key.as_deref(),
        Some("grokbuild#sess-1#turn1")
    );
    assert_eq!(
        events[1].dedupe_key.as_deref(),
        Some("grokbuild#sess-1#turn2")
    );

    // Replay is idempotent at the event identity layer.
    let again = read_all();
    assert_eq!(again, events);

    unsafe { std::env::remove_var(ENV) };
}

#[test]
fn empty_home_probes_nothing() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var(ENV, dir.path()) };
    assert!(GrokBuildAdapter.probe().is_none());
    assert!(GrokBuildAdapter.discover(&DateFilter::default()).is_empty());
    unsafe { std::env::remove_var(ENV) };
}

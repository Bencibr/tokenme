//! Behaviour tests against fixture logs copied into a fake `~/.claude` install.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use usage_adapter_claude::{ClaudeAdapter, TOOL_ID};
use usage_core::{
    parse_ts_ms, Call, CallKind, DateFilter, Error, FileKind, Meter, ReadCursor, ReadOutcome,
    SourceAdapter, SourceFile, UsageEvent,
};

/// `CLAUDE_CONFIG_DIR` is process-global, so whoever flips it holds one lock.
static ENV: Mutex<()> = Mutex::new(());

struct EnvRoot {
    _guard: MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

impl EnvRoot {
    fn set(root: &Path) -> Self {
        let guard = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::set_var("CLAUDE_CONFIG_DIR", root);
        EnvRoot { _guard: guard, previous }
    }

    /// The real `~/.claude`, with any override cleared. Holding the same lock
    /// matters: a parallel test's `CLAUDE_CONFIG_DIR` would otherwise redirect
    /// `probe`/`discover` into somebody else's temp dir.
    fn cleared() -> Self {
        let guard = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        EnvRoot { _guard: guard, previous }
    }
}

impl Drop for EnvRoot {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }
}

const WORKSPACE: &str = "-Users-demo-proj";

/// `<tmp>/.claude/projects/-Users-demo-proj`, i.e. the real layout, so the
/// adapter can derive the config root from the log file's own path.
struct Install {
    dir: tempfile::TempDir,
}

impl Install {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(Self::workspace(dir.path())).unwrap();
        Install { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join(".claude")
    }

    fn projects(&self) -> PathBuf {
        self.root().join("projects")
    }

    fn workspace(tmp: &Path) -> PathBuf {
        tmp.join(".claude").join("projects").join(WORKSPACE)
    }

    fn log_path(&self, name: &str, nested: Option<&str>) -> PathBuf {
        let workspace = Self::workspace(self.dir.path());
        match nested {
            Some(dir) => workspace.join(dir).join(name),
            None => workspace.join(name),
        }
    }

    /// Copy a fixture into the tree; `nested` mirrors the `subagents/` layout.
    fn place(&self, fixture: &str, name: &str, nested: Option<&str>) -> SourceFile {
        let target = self.log_path(name, nested);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(fixture_path(fixture), &target).expect("copy fixture");
        source_file(&target)
    }

    /// The `CLAUDE_CONFIG_DIR` spelling: the flat file lives inside the root.
    fn write_config(&self, body: &str) {
        fs::write(self.root().join(".claude.json"), body).unwrap();
    }

    fn write_skills(&self, names: &[&str]) {
        for name in names {
            fs::create_dir_all(self.root().join("skills").join(name)).unwrap();
        }
    }

    fn append(&self, file: &SourceFile, bytes: &str) {
        let mut handle = OpenOptions::new().append(true).open(&file.path).unwrap();
        handle.write_all(bytes.as_bytes()).unwrap();
    }

    fn env(&self) -> EnvRoot {
        EnvRoot::set(&self.root())
    }
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn source_file(path: &Path) -> SourceFile {
    let meta = fs::metadata(path).unwrap();
    SourceFile {
        path: path.to_path_buf(),
        kind: FileKind::Jsonl,
        size: meta.len(),
        mtime_ms: millis(meta.modified().unwrap()),
    }
}

fn absent_source_file(path: &Path) -> SourceFile {
    SourceFile { path: path.to_path_buf(), kind: FileKind::Jsonl, size: 0, mtime_ms: 0 }
}

fn millis(when: SystemTime) -> i64 {
    when.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

fn read(file: &SourceFile, cursor: u64) -> ReadOutcome {
    ClaudeAdapter.read(file, ReadCursor(cursor)).expect("fixture read")
}

fn ids(events: &[UsageEvent]) -> Vec<&str> {
    events.iter().filter_map(|e| e.dedupe_key.as_deref()).collect()
}

fn event<'a>(events: &'a [UsageEvent], id: &str) -> &'a UsageEvent {
    events
        .iter()
        .find(|e| e.dedupe_key.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no event for {id}, got {:?}", ids(events)))
}

fn skill(name: &str) -> Call {
    Call { kind: CallKind::Skill, name: name.into() }
}

fn mcp(name: &str) -> Call {
    Call { kind: CallKind::Mcp, name: name.into() }
}

#[test]
fn streaming_lines_become_one_event_per_turn() {
    let install = Install::new();
    let file = install.place("main.jsonl", "sess-1.jsonl", None);
    let outcome = read(&file, 0);

    // msg_004 has tool calls but no usage, msg_broken is unparsable, and the
    // trailing record is still being written: none of them are events.
    assert_eq!(ids(&outcome.events), ["msg_001", "msg_002", "msg_003"]);

    let turn = event(&outcome.events, "msg_001");
    assert_eq!(turn.tool, TOOL_ID);
    assert_eq!(turn.session, "sess-1");
    assert_eq!(turn.project.as_deref(), Some("/Users/demo/proj"));
    assert_eq!(turn.model.as_deref(), Some("glm-5.3-flash"));
    assert_eq!(turn.meter, Meter::Tokens);
    assert_eq!(turn.source, file.key());
    assert_eq!(turn.ts_ms, parse_ts_ms("2026-09-23T09:00:02.000Z").unwrap());
    let counts = &turn.counts;
    assert_eq!((counts.input, counts.output, counts.reasoning), (1000.0, 300.0, 120.0));
    // The nested `cache_creation` split is 400 + 100: the authoritative
    // `cache_creation_input_tokens` of 500 must not become 1000.
    assert_eq!((counts.cache_creation, counts.cache_read), (500.0, 9000.0));
    assert_eq!(counts.total(), 10_800.0);
    // No whitelist on disk: neither the MCP server nor the slash command counts.
    assert_eq!(turn.calls, Vec::new());

    // Two lines, one id: the repeated usage is counted once, not summed.
    let retry = event(&outcome.events, "msg_002");
    assert_eq!((retry.counts.total(), retry.counts.input), (35.0, 20.0));
    assert_eq!(retry.ts_ms, parse_ts_ms("2026-09-23T09:00:04.000Z").unwrap());
    assert!(retry.calls.is_empty(), "unlisted mcp__figma must be dropped");

    // A proxy or sentinel id is kept verbatim so pricing can call it unpriced.
    assert_eq!(event(&outcome.events, "msg_003").model.as_deref(), Some("<synthetic>"));
    assert!(
        outcome.events.iter().all(|e| e.counts.total() < 900_000.0),
        "cost-state/system pseudo-usage leaked into the events"
    );
}

#[test]
fn only_whitelisted_servers_and_skills_are_recorded() {
    let install = Install::new();
    let file = install.place("main.jsonl", "sess-1.jsonl", None);
    install.write_config(
        r#"{"version":"2.1.278","mcpServers":{"bugx":{}},
            "projects":{"/Users/demo/proj":{"mcpServers":{"notlisted":{}}}}}"#,
    );
    install.write_skills(&["orca", "unrelated", "deep-research"]);

    let outcome = read(&file, 0);
    // The slash command from the user record lands on the turn it started.
    assert_eq!(
        event(&outcome.events, "msg_001").calls,
        vec![skill("deep-research"), mcp("bugx"), skill("orca")]
    );
    // `figma` appears in the log but is absent from the config, so it is out.
    assert_eq!(event(&outcome.events, "msg_002").calls, Vec::new());
    // Token totals never depend on call filtering.
    assert_eq!(event(&outcome.events, "msg_001").counts.total(), 10_800.0);
    assert_eq!(ids(&outcome.events), ["msg_001", "msg_002", "msg_003"]);
}

#[test]
fn resume_from_the_returned_cursor_adds_nothing() {
    let install = Install::new();
    let file = install.place("resume.jsonl", "sess-r.jsonl", None);
    let first = read(&file, 0);
    assert_eq!(ids(&first.events), ["msg_r1", "msg_r2"]);
    assert_eq!(first.cursor.0, file.size, "every line here is complete");

    let second = ClaudeAdapter.read(&file, first.cursor).unwrap();
    assert!(second.events.is_empty());
    assert_eq!(second.cursor, first.cursor);
}

#[test]
fn a_half_written_line_waits_and_the_append_delivers_it() {
    let install = Install::new();
    let file = install.place("main.jsonl", "sess-1.jsonl", None);

    let first = read(&file, 0);
    assert_eq!(ids(&first.events), ["msg_001", "msg_002", "msg_003"]);
    assert!(first.cursor.0 < file.size, "the partial tail must not be consumed");
    let left = file.size - first.cursor.0;
    assert!((20..200).contains(&left), "unexpected partial tail of {left} bytes");

    // Claude Code flushes the rest of the record; nothing else changed.
    install.append(&file, "xt\",\"text\":\"half a line\"}],\"usage\":{\"input_tokens\":7,\"output_tokens\":9}},\"sessionId\":\"sess-1\",\"cwd\":\"/Users/demo/proj\",\"timestamp\":\"2026-09-23T09:00:06.000Z\",\"uuid\":\"a-8\"}\n");

    let second = ClaudeAdapter.read(&file, first.cursor).unwrap();
    assert_eq!(ids(&second.events), ["msg_999"], "exactly the newly completed turn");
    let late = event(&second.events, "msg_999");
    assert_eq!((late.counts.input, late.counts.output), (7.0, 9.0));
    assert_eq!(late.model.as_deref(), Some("glm-5.3-flash"));
    assert_eq!(second.cursor.0, fs::metadata(&file.path).unwrap().len());
    assert!(ClaudeAdapter.read(&file, second.cursor).unwrap().events.is_empty());
}

#[test]
fn an_appended_turn_is_the_only_new_event() {
    let install = Install::new();
    let file = install.place("append.jsonl", "sess-a.jsonl", None);
    let first = read(&file, 0);
    assert_eq!(ids(&first.events), ["msg_a1"]);

    install.append(&file, "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_a2\",\"model\":\"step-5-preview\",\"content\":[{\"type\":\"text\",\"text\":\"two\"}],\"usage\":{\"input_tokens\":50,\"output_tokens\":6}},\"sessionId\":\"sess-a\",\"cwd\":\"/Users/demo/proj\",\"timestamp\":\"2026-09-23T11:00:02.000Z\"}\n");

    let grown = source_file(&file.path);
    let next = ClaudeAdapter.read(&grown, first.cursor).unwrap();
    assert_eq!(ids(&next.events), ["msg_a2"]);
    assert_eq!(next.events[0].counts.total(), 56.0);
    assert_eq!(next.cursor.0, grown.size);
}

#[test]
fn discover_walks_nested_session_dirs_and_filters_by_mtime() {
    let install = Install::new();
    let _env = install.env();
    let top = install.place("main.jsonl", "sess-1.jsonl", None);
    let nested = install.place("resume.jsonl", "agent-a.jsonl", Some("sess-1/subagents"));
    // `fs::copy` carries the fixture's mtime on macOS, and a checkout can be
    // days old — stamp placement time explicitly or the since-filter below
    // reads a fresh placement as ancient.
    for file in [&top, &nested] {
        let handle = OpenOptions::new().write(true).open(&file.path).unwrap();
        handle.set_modified(SystemTime::now()).unwrap();
    }
    // Subagent metadata sits next to the logs and is not a log.
    fs::write(nested.path.with_extension("meta.json"), "{}").unwrap();

    let all = ClaudeAdapter.discover(&DateFilter::default());
    assert_eq!(all.len(), 2, "found {:?}", paths(&all));
    assert!(paths(&all).contains(&top.path));
    assert!(paths(&all).contains(&nested.path));
    assert!(all.iter().all(|f| f.kind == FileKind::Jsonl));
    assert!(all.iter().all(|f| f.size > 0 && f.mtime_ms > 0));
    assert!(all.windows(2).all(|pair| pair[0].path < pair[1].path), "not sorted");

    let twenty_twenty = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_000_000);
    let handle = OpenOptions::new().write(true).open(&top.path).unwrap();
    handle.set_modified(twenty_twenty).unwrap();
    drop(handle);

    let since = SystemTime::now() - Duration::from_secs(86_400);
    let recent = ClaudeAdapter.discover(&DateFilter::new(Some(millis(since)), None));
    assert_eq!(paths(&recent), vec![nested.path.clone()], "the 2020 file must be skipped");
}

fn paths(files: &[SourceFile]) -> Vec<PathBuf> {
    files.iter().map(|f| f.path.clone()).collect()
}

#[test]
fn probe_reports_the_source_without_reading_every_file() {
    let install = Install::new();
    let _env = install.env();
    let adapter = ClaudeAdapter;
    assert!(adapter.probe().is_none(), "an empty projects tree is not an install");

    install.place("resume.jsonl", "agent-deep.jsonl", Some("sess-1/subagents"));
    let detected = adapter.probe().expect("one nested log is enough");
    assert_eq!(detected.id, TOOL_ID);
    assert_eq!(detected.display, "Claude Code");
    assert_eq!(detected.roots, vec![install.projects()]);
    assert_eq!(detected.hint, None, "no version recorded yet");

    install.write_config(r#"{"version":"2.1.278","mcpServers":{}}"#);
    assert_eq!(adapter.probe().unwrap().hint.as_deref(), Some("2.1.278"));
}

#[test]
fn unreadable_logs_are_reported_not_raised() {
    let install = Install::new();
    let missing = absent_source_file(&install.log_path("gone.jsonl", None));
    let outcome = ClaudeAdapter.read(&missing, ReadCursor(0)).unwrap();
    assert_eq!((outcome.events.len(), outcome.cursor), (0, ReadCursor(0)));

    // A directory opens fine but can never be read as a log.
    let dir = source_file(&install.projects());
    let outcome = ClaudeAdapter.read(&dir, ReadCursor(0)).unwrap();
    assert_eq!((outcome.events.len(), outcome.cursor), (0, ReadCursor(0)));
}

#[test]
fn a_truncated_log_rejects_a_cursor_past_its_end() {
    let install = Install::new();
    let file = install.place("main.jsonl", "sess-1.jsonl", None);
    let outcome = read(&file, 0);
    assert_eq!(ids(&outcome.events), ["msg_001", "msg_002", "msg_003"]);

    // Log rotation replaces a 6 KB file with a fresh header.
    fs::write(&file.path, b"short\n").unwrap();
    let err = ClaudeAdapter.read(&file, outcome.cursor).unwrap_err();
    assert!(matches!(err, Error::Cursor { cursor, .. } if cursor == outcome.cursor.0), "got {err:?}");
}

#[test]
#[ignore = "reads the real ~/.claude/projects on this machine"]
fn real_machine_logs_yield_thousands_of_events() {
    let _env = EnvRoot::cleared();
    let Some(root) = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")) else {
        eprintln!("no HOME: nothing to smoke test");
        return;
    };
    let adapter = ClaudeAdapter;
    let Some(detected) = adapter.probe() else {
        eprintln!("claude not detected under {}: nothing to smoke test", root.display());
        return;
    };
    let files = adapter.discover(&DateFilter::default());
    let mut events = Vec::new();
    let mut bytes = 0u64;
    for file in &files {
        let outcome = adapter.read(file, ReadCursor(0)).expect("real files must not error");
        bytes += outcome.cursor.0;
        events.extend(outcome.events);
    }

    let mut turns = std::collections::HashSet::new();
    let mut tokens = usage_core::TokenCounts::default();
    let mut models: std::collections::BTreeMap<String, usize> = Default::default();
    let mut calls: std::collections::BTreeMap<String, usize> = Default::default();
    for event in &events {
        turns.insert(event.dedupe_key.clone().unwrap_or_default());
        tokens += &event.counts;
        *models.entry(event.model.clone().unwrap_or_else(|| "?".into())).or_default() += 1;
        for call in &event.calls {
            *calls.entry(format!("{:?}/{}", call.kind, call.name)).or_default() += 1;
        }
    }
    eprintln!(
        "roots {:?} hint {:?}\n{} files, {bytes} bytes -> {} events ({} distinct ids), \
         {:?} tokens, {} models, {} call groups",
        detected.roots,
        detected.hint,
        files.len(),
        events.len(),
        turns.len(),
        (
            tokens.input as i64,
            tokens.cache_creation as i64,
            tokens.cache_read as i64,
            tokens.output as i64,
            tokens.reasoning as i64
        ),
        models.len(),
        calls.len(),
    );
    assert!(tokens.total() > 0.0, "total tokens are {}", tokens.total() as i64);
    for (model, count) in models.iter().take(8) {
        eprintln!("  model {model}: {count} events");
    }
    for (call, count) in calls.iter().take(8) {
        eprintln!("  call {call}: {count} events");
    }

    assert!(events.len() > 1000, "only {} events", events.len());
    assert!(models.len() > 1, "expected several models, got {models:?}");
}

/// A streamed assistant message is logged once per content block, and each line
/// carries a usage snapshot of that same single API call. Counting them as calls
/// inflates every headline number, so this fixture pins the arithmetic: eight
/// lines, three calls, and the totals below are the only defensible answer.
#[test]
fn streamed_partial_snapshots_belong_to_one_call() {
    let install = Install::new();
    let file = install.place("streamed.jsonl", "sess-stream.jsonl", None);
    let outcome = read(&file, 0);

    assert_eq!(ids(&outcome.events), ["msg_A", "msg_B", "msg_C"]);
    let input: f64 = outcome.events.iter().map(|e| e.counts.input).sum();
    let cache_read: f64 = outcome.events.iter().map(|e| e.counts.cache_read).sum();
    let output: f64 = outcome.events.iter().map(|e| e.counts.output).sum();
    let total: f64 = outcome.events.iter().map(|e| e.counts.total()).sum();
    assert_eq!((input, cache_read, output), (3600.0, 72000.0, 1500.0));
    assert_eq!(total, 77_100.0);
    // Repeated identical snapshots and growing partials both resolve to the
    // finished usage of the call, never to a sum of what was streamed.
    assert_eq!(event(&outcome.events, "msg_B").counts.output, 500.0);
    assert_eq!(event(&outcome.events, "msg_A").counts.total(), 23_400.0);
}

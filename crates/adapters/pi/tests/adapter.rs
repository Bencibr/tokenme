//! Behaviour tests against fixture logs copied into a fake `~/.pi/agent` install.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use usage_adapter_pi::{ColaAdapter, PiAdapter, TOOL_ID};
use usage_core::{
    parse_ts_ms, DateFilter, Error, FileKind, Meter, ReadCursor, ReadOutcome, SourceAdapter,
    SourceFile, TokenCounts, UsageEvent,
};

/// Both Pi overrides and Cola's are process-global, so whoever flips one holds
/// the lock.
static ENV: Mutex<()> = Mutex::new(());

const OVERRIDES: [&str; 3] = ["PI_AGENT_DIR", "PI_CODING_AGENT_DIR", "COLA_SESSIONS_DIR"];

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    /// Clears both overrides (a parallel test's value must not leak in) and then
    /// applies exactly what the caller asked for.
    fn with(overrides: &[(&'static str, &Path)]) -> Self {
        let lock = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved: Vec<(&'static str, Option<std::ffi::OsString>)> = OVERRIDES
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in OVERRIDES {
            std::env::remove_var(key);
        }
        for (key, value) in overrides {
            std::env::set_var(key, value);
        }
        EnvGuard { _lock: lock, saved }
    }

    /// `PI_AGENT_DIR` names the *sessions* directory, as in ccusage's adapter.
    fn sessions(root: &Path) -> Self {
        Self::with(&[("PI_AGENT_DIR", root)])
    }

    /// `PI_CODING_AGENT_DIR`, Pi's own variable, names the *config* directory.
    fn config(root: &Path) -> Self {
        Self::with(&[("PI_CODING_AGENT_DIR", root)])
    }

    /// `COLA_SESSIONS_DIR` names Cola's sessions root directly (Cola publishes no
    /// variable of its own; see `paths::COLA`).
    fn cola_sessions(root: &Path) -> Self {
        Self::with(&[("COLA_SESSIONS_DIR", root)])
    }

    /// The real `~/.pi/agent/sessions`, for the ignored smoke test.
    fn cleared() -> Self {
        Self::with(&[])
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

const SESSION: &str = "01a0c979-d41a-70c8-8b42-02c51ac2f6d7";
/// `append.jsonl` belongs to a different session, which is exactly why the
/// dedupe key is prefixed rather than the bare record id.
const OTHER: &str = "1a2b3c4d-5555-6666-7777-888888888888";
const WORKSPACE: &str = "--Users-demo-proj--";

/// `<tmp>/.pi/agent/sessions/--Users-demo-proj--`, the real layout.
struct Install {
    dir: tempfile::TempDir,
}

impl Install {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(self_sessions(dir.path())).unwrap();
        Install { dir }
    }

    fn config_root(&self) -> PathBuf {
        self.dir.path().join(".pi").join("agent")
    }

    fn sessions(&self) -> PathBuf {
        self_sessions(self.dir.path())
    }

    fn log_path(&self, name: &str, workspace: Option<&str>) -> PathBuf {
        self.sessions()
            .join(workspace.unwrap_or(WORKSPACE))
            .join(name)
    }

    /// Copy a fixture in under the name a real session file would have.
    fn place(&self, fixture: &str, name: &str, workspace: Option<&str>) -> SourceFile {
        let target = self.log_path(name, workspace);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(fixture_path(fixture), &target).expect("copy fixture");
        source_file(&target)
    }

    fn write_raw(&self, name: &str, body: &str) -> SourceFile {
        let target = self.log_path(name, None);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, body).unwrap();
        source_file(&target)
    }

    fn append(&self, file: &SourceFile, bytes: &str) {
        let mut handle = OpenOptions::new().append(true).open(&file.path).unwrap();
        handle.write_all(bytes.as_bytes()).unwrap();
    }
}

fn self_sessions(tmp: &Path) -> PathBuf {
    tmp.join(".pi").join("agent").join("sessions")
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
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
    SourceFile {
        path: path.to_path_buf(),
        kind: FileKind::Jsonl,
        size: 0,
        mtime_ms: 0,
    }
}

fn millis(when: SystemTime) -> i64 {
    when.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// `dedupe_key` is `<session>#<record id>`, so tests speak in short ids.
fn key(record: &str) -> String {
    format!("{SESSION}#{record}")
}

fn other_key(record: &str) -> String {
    format!("{OTHER}#{record}")
}

fn read(file: &SourceFile, cursor: u64) -> ReadOutcome {
    PiAdapter
        .read(file, ReadCursor(cursor))
        .expect("fixture read")
}

fn keys(events: &[UsageEvent]) -> Vec<String> {
    events.iter().filter_map(|e| e.dedupe_key.clone()).collect()
}

fn event(events: &[UsageEvent], id: &str) -> UsageEvent {
    let want = key(id);
    events
        .iter()
        .find(|e| e.dedupe_key.as_deref() == Some(want.as_str()))
        .cloned()
        .unwrap_or_else(|| panic!("no event for {id}, got {:?}", keys(events)))
}

/// The mapping table itself: Pi's `input` excludes `cacheRead`, `cacheWrite1h`
/// is a tier inside `cacheWrite`, and `reasoning` is a breakdown of `output`.
#[test]
fn usage_maps_onto_the_shared_token_stages() {
    let install = Install::new();
    let file = install.place("main.jsonl", "sess-main.jsonl", None);
    let outcome = read(&file, 0);

    // The user, toolResult and aborted records produce nothing, and the
    // malformed plus still-partial trailing lines are not consumed.
    assert_eq!(
        keys(&outcome.events),
        vec![key("1680bdc9"), key("aaaa0002")]
    );

    let first = &outcome.events[0];
    assert_eq!(first.tool, TOOL_ID);
    assert_eq!(first.session, SESSION);
    assert_eq!(first.project.as_deref(), Some("/Users/demo/proj"));
    assert_eq!(
        first.model.as_deref(),
        Some("9dev"),
        "`message.model`, verbatim"
    );
    assert_eq!(first.meter, Meter::Tokens);
    assert_eq!(first.source, file.key());
    assert!(first.calls.is_empty(), "Pi's tool calls are all built-ins");
    assert_eq!(
        first.ts_ms,
        parse_ts_ms("2026-09-22T14:16:55.123Z").unwrap()
    );
    let counts = &first.counts;
    assert_eq!(
        (
            counts.input,
            counts.cache_read,
            counts.output,
            counts.reasoning,
            counts.cache_creation
        ),
        (7616.0, 1152.0, 262.0, 153.0, 0.0)
    );
    assert_eq!(counts.credits, 0.0, "Pi's self-reported cost is ignored");

    // The second turn carries both cache-write fields: 5000 total, of which the
    // 1-hour tier is a subset, so it must stay 5000 and not become 10000.
    let second = event(&outcome.events, "aaaa0002");
    assert_eq!(
        (
            second.counts.input,
            second.counts.cache_creation,
            second.counts.cache_read,
            second.counts.output
        ),
        (18955.0, 5000.0, 25088.0, 683.0)
    );
    assert_eq!(second.model.as_deref(), Some("deepseek-v4-flash"));

    // SEMANTICS: `usage` is per call, never a running total — each event's
    // decomposition adds up to Pi's own `totalTokens` for that one record.
    let total: f64 = outcome.events.iter().map(|e| e.counts.total()).sum();
    assert_eq!(
        total, 58_756.0,
        "9030 + 49726 are the two records' own totalTokens"
    );
}

/// A repeated record id is one call written twice, so the snapshots merge by
/// keeping the largest — summing them would bill the same turn twice.
#[test]
fn a_repeated_record_id_keeps_the_largest_snapshot() {
    let install = Install::new();
    let file = install.place("streamed.jsonl", "sess-stream.jsonl", None);
    let outcome = read(&file, 0);

    assert_eq!(keys(&outcome.events).len(), 2, "two ids, three lines");
    let merged = &outcome.events[0];
    assert_eq!(
        merged.dedupe_key.as_deref(),
        Some("0f0f0f0f-1111-2222-3333-444444444444#7777aaaa")
    );
    // 1300 partial + 10680 finished would be 11980 if the snapshots were summed.
    assert_eq!(merged.counts.total(), 10_680.0);
    assert_eq!(
        (
            merged.counts.input,
            merged.counts.cache_read,
            merged.counts.output
        ),
        (1200.0, 9000.0, 480.0)
    );
    assert_eq!(merged.counts.reasoning, 200.0);
    // Labels come from the first line of the record: the later re-write only
    // grows the usage.
    assert_eq!(
        merged.ts_ms,
        parse_ts_ms("2026-09-20T09:00:01.000Z").unwrap()
    );

    // The coordinator's real sample: 23407 in / 42 out, all-zero cost.
    let sample = &outcome.events[1];
    assert_eq!(
        (
            sample.counts.input,
            sample.counts.output,
            sample.counts.total()
        ),
        (23407.0, 42.0, 23449.0)
    );
    assert_eq!(sample.model.as_deref(), Some("mimo"));
    assert_eq!(sample.project.as_deref(), Some("/Users/demo/stream"));
    assert_eq!(
        outcome.events.iter().map(|e| e.counts.total()).sum::<f64>(),
        34_129.0,
        "35429 would mean the partial snapshot was added on top"
    );
}

/// Without a `session` header line the file still labels itself, and a record
/// with no timestamp of its own must not fall through to epoch 0.
#[test]
fn a_header_less_file_labels_itself_from_its_path() {
    let install = Install::new();
    let name = "2026-09-21T10-11-12-345Z_7c7c7c7c-0000-1111-2222-333333333333.jsonl";
    let file = install.place("orphan.jsonl", name, None);
    let outcome = read(&file, 0);

    assert_eq!(
        keys(&outcome.events),
        vec![
            "7c7c7c7c-0000-1111-2222-333333333333#9999aaaa",
            "7c7c7c7c-0000-1111-2222-333333333333#9999aaab",
        ]
    );
    for ev in &outcome.events {
        assert_eq!(
            ev.session, "7c7c7c7c-0000-1111-2222-333333333333",
            "the uuid in the file name"
        );
        // The mangled directory decodes losslessly here because no segment
        // contains a dash; that is the documented limit of this fallback.
        assert_eq!(ev.project.as_deref(), Some("/Users/demo/proj"));
    }
    assert_eq!(
        outcome.events[0].ts_ms, file.mtime_ms,
        "undated record, so the file's mtime"
    );
    assert!(outcome.events[0].ts_ms > 0);
    assert_eq!(
        outcome.events[1].ts_ms,
        parse_ts_ms("2026-09-21T10:30:00.000Z").unwrap()
    );
    assert_eq!(outcome.events[1].counts.total(), 34.0);
}

#[test]
fn resume_from_the_returned_cursor_adds_nothing() {
    let install = Install::new();
    let file = install.place("append.jsonl", "sess-a.jsonl", None);
    let first = read(&file, 0);
    assert_eq!(keys(&first.events), vec![other_key("1111aaaa")]);
    assert_eq!(first.cursor.0, file.size, "every line here is complete");

    let second = PiAdapter.read(&file, first.cursor).unwrap();
    assert!(second.events.is_empty());
    assert_eq!(
        second.cursor, first.cursor,
        "an unchanged cursor when nothing was processed"
    );
}

/// The trailing half-line is a record mid-write: it stays unread, and the cursor
/// only moves once the rest of it lands.
#[test]
fn a_half_written_line_waits_and_the_append_delivers_it() {
    let install = Install::new();
    let file = install.place("main.jsonl", "sess-main.jsonl", None);
    let first = read(&file, 0);
    assert_eq!(keys(&first.events), vec![key("1680bdc9"), key("aaaa0002")]);
    let left = file.size - first.cursor.0;
    assert!(
        (100..400).contains(&left),
        "unexpected partial tail of {left} bytes"
    );

    install.append(&file, "f-written\"}],\"usage\":{\"input\":50,\"output\":6,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":56,\"cost\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"total\":0}},\"stopReason\":\"stop\",\"timestamp\":1758550640000}}\n");

    let second = PiAdapter.read(&file, first.cursor).unwrap();
    assert_eq!(
        keys(&second.events),
        vec![key("dddd0006")],
        "exactly the newly completed turn"
    );
    let late = event(&second.events, "dddd0006");
    assert_eq!(
        (late.counts.input, late.counts.output, late.counts.total()),
        (50.0, 6.0, 56.0)
    );
    assert_eq!(late.model.as_deref(), Some("9dev"));
    // The `session` header line is behind the cursor now, and the labels still
    // resolve: the record's `cwd`, not the lossy directory decode.
    assert_eq!(late.session, SESSION);
    assert_eq!(late.project.as_deref(), Some("/Users/demo/proj"));
    assert_eq!(late.ts_ms, parse_ts_ms("2026-09-22T14:17:20.000Z").unwrap());
    let grown = source_file(&file.path);
    assert_eq!(second.cursor.0, grown.size);
    assert!(PiAdapter
        .read(&grown, second.cursor)
        .unwrap()
        .events
        .is_empty());
}

#[test]
fn an_appended_turn_is_the_only_new_event() {
    let install = Install::new();
    let file = install.place("append.jsonl", "sess-a.jsonl", None);
    let first = read(&file, 0);
    assert_eq!(keys(&first.events), vec![other_key("1111aaaa")]);

    install.append(&file, "{\"type\":\"message\",\"id\":\"2222bbbb\",\"parentId\":\"1111aaaa\",\"timestamp\":\"2026-09-21T10:00:09.000Z\",\"message\":{\"role\":\"assistant\",\"api\":\"openai-completions\",\"provider\":\"9router\",\"model\":\"9dev\",\"content\":[],\"usage\":{\"input\":10,\"output\":2,\"cacheRead\":128,\"cacheWrite\":0,\"totalTokens\":140,\"cost\":{}},\"stopReason\":\"stop\"}}\n");

    let grown = source_file(&file.path);
    let next = PiAdapter.read(&grown, first.cursor).unwrap();
    assert_eq!(keys(&next.events), vec![other_key("2222bbbb")]);
    assert_eq!(next.events[0].counts.total(), 140.0);
    assert_eq!(next.events[0].counts.cache_read, 128.0);
    assert_eq!(next.cursor.0, grown.size);
    // Only the new turn is emitted, and it keeps the header's labels even
    // though the header line itself is behind the cursor.
    assert_eq!(next.events[0].session, OTHER);
    assert_eq!(
        next.events[0].project.as_deref(),
        Some("/Users/demo/append")
    );
}

#[test]
fn a_malformed_line_is_skipped_without_losing_the_window() {
    let install = Install::new();
    let file = install.write_raw(
        "sess-junk.jsonl",
        "{\"type\":\"session\",\"version\":3,\"id\":\"s-junk\",\"timestamp\":\"2026-09-22T14:16:39.708Z\",\"cwd\":\"/Users/demo/proj\"}\n\
         {not json at all}\n\
         {\"type\":\"message\",\"id\":\"x1\",\"message\":{\"role\":\"assistant\",\"model\":\"9dev\",\"usage\":{\"input\":1,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":2}}}\n\
         {\"type\":\"message\",\"id\":\"x2\",\"message\":\"a string, not an object\"}\n\
         {\"type\":\"message\",\"id\":\"x3\",\"message\":{\"role\":\"assistant\",\"model\":\"9dev\",\"usage\":{\"input\":\"7\",\"output\":\"3\",\"cacheRead\":null,\"cacheWrite\":-5,\"totalTokens\":10}}}\n",
    );
    let outcome = read(&file, 0);
    assert_eq!(
        outcome.events.len(),
        2,
        "the two parsable assistant records survive"
    );
    assert_eq!(
        outcome.cursor.0, file.size,
        "garbage lines count as processed bytes"
    );
    assert_eq!(
        outcome.events[1].counts.total(),
        10.0,
        "stringified numbers still parse"
    );
    assert_eq!(
        outcome.events[1].counts.cache_read, 0.0,
        "null and negative stay zero"
    );
    assert_eq!(outcome.events[0].session, "s-junk");
}

#[test]
fn discover_lists_logs_and_skips_derived_transcripts() {
    let install = Install::new();
    let _env = EnvGuard::sessions(&install.sessions());
    let top = install.place("append.jsonl", "sess-a.jsonl", None);
    let other = install.place("orphan.jsonl", "sess-b.jsonl", Some("--Users-demo-other--"));
    // `pi-subagents` replays calls already billed by the parent session here.
    let derived = install.place("streamed.jsonl", "agent.jsonl", Some("subagent-artifacts"));
    fs::write(install.log_path("notes.md", None), "not a log").unwrap();

    let all = PiAdapter.discover(&DateFilter::default());
    assert_eq!(
        paths(&all),
        vec![other.path.clone(), top.path.clone()],
        "found {:?}",
        paths(&all)
    );
    assert!(!paths(&all).contains(&derived.path));
    assert!(all.iter().all(|f| f.kind == FileKind::Jsonl));
    assert!(
        all.iter().all(|f| f.size > 0 && f.mtime_ms > 0),
        "stat once, both fields filled"
    );

    let ancient = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_000_000);
    let handle = OpenOptions::new().write(true).open(&top.path).unwrap();
    handle.set_modified(ancient).unwrap();
    drop(handle);
    let since = millis(SystemTime::now() - Duration::from_secs(86_400));
    let recent = PiAdapter.discover(&DateFilter::new(Some(since), None));
    assert_eq!(
        paths(&recent),
        vec![other.path],
        "the 2020 file must be skipped by mtime"
    );
}

#[test]
fn probe_short_circuits_on_the_first_readable_log() {
    let install = Install::new();
    let _env = EnvGuard::sessions(&install.sessions());
    assert!(
        PiAdapter.probe().is_none(),
        "an absent sessions dir is not an install"
    );

    let second = install.place("streamed.jsonl", "z-second.jsonl", None);
    let first = install.place("append.jsonl", "a-first.jsonl", None);
    let detected = PiAdapter.probe().expect("one readable log is enough");
    assert_eq!(detected.id, TOOL_ID);
    assert_eq!(detected.display, "Pi");
    assert_eq!(detected.roots, vec![install.sessions()]);
    // Both fixtures start with a v3 header; `probe` reads only the one it finds.
    assert_eq!(detected.hint.as_deref(), Some("session format v3"));
    assert!(first.size > 0 && second.size > first.size);
}

#[test]
fn the_config_dir_override_resolves_the_sessions_subdir() {
    let install = Install::new();
    install.place("append.jsonl", "sess-a.jsonl", None);
    let _env = EnvGuard::config(&install.config_root());
    let detected = PiAdapter
        .probe()
        .expect("PI_CODING_AGENT_DIR/sessions is the real layout");
    assert_eq!(detected.roots, vec![install.sessions()]);
    assert_eq!(PiAdapter.discover(&DateFilter::default()).len(), 1);
}

/// Cola's sessions root as it looks on disk: one directory per session key, each
/// holding `<ISO>_<uuid>.jsonl` plus the app's own `state.json`/`run.json`.
struct ColaInstall {
    dir: tempfile::TempDir,
}

impl ColaInstall {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".cola").join("sessions")).unwrap();
        ColaInstall { dir }
    }

    fn sessions(&self) -> PathBuf {
        self.dir.path().join(".cola").join("sessions")
    }

    fn write(&self, key: &str, name: &str, body: &str) -> SourceFile {
        let target = self.sessions().join(key).join(name);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, body).unwrap();
        source_file(&target)
    }
}

/// Header plus three billed turns and two unbilled records, in Cola's own shapes
/// (`provider:"cola"`, `cost` riding along, `reasoning` on some records only).
const COLA_PARENT: &str = concat!(
    "{\"type\":\"session\",\"version\":3,\"id\":\"01a0252a-fc23-7844-a17b-94d62a069429\",\"timestamp\":\"2026-08-21T16:32:49.187Z\",\"cwd\":\"/Users/me/workspace/memory-bank\"}\n",
    "{\"type\":\"message\",\"id\":\"0fe20590\",\"parentId\":\"137f0735\",\"timestamp\":\"2026-08-21T16:32:53.277Z\",\"message\":{\"role\":\"assistant\",\"api\":\"openai-completions\",\"provider\":\"cola\",\"model\":\"deepseek-v4-pro\",\"content\":[],\"usage\":{\"input\":7987,\"output\":165,\"cacheRead\":12288,\"cacheWrite\":0,\"reasoning\":42,\"totalTokens\":20440,\"cost\":{\"input\":0.0001,\"output\":0.0002,\"cacheRead\":0,\"cacheWrite\":0,\"total\":0.0003}},\"stopReason\":\"tool-calls\"}}\n",
    "{\"type\":\"message\",\"id\":\"a1b2c3d4\",\"parentId\":\"0fe20590\",\"timestamp\":\"2026-08-21T16:32:54.000Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"bookmark these\"}]}}\n",
    "{\"type\":\"message\",\"id\":\"ee00ffff\",\"parentId\":\"a1b2c3d4\",\"timestamp\":\"2026-08-21T16:32:55.000Z\",\"message\":{\"role\":\"assistant\",\"model\":\"deepseek-v4-pro\",\"content\":[],\"usage\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":0,\"cost\":{}},\"stopReason\":\"aborted\"}}\n",
    "{\"type\":\"message\",\"id\":\"23ca0671\",\"parentId\":\"a1b2c3d4\",\"timestamp\":\"2026-08-21T16:33:31.167Z\",\"message\":{\"role\":\"assistant\",\"api\":\"openai-completions\",\"provider\":\"cola\",\"model\":\"deepseek-v4-pro\",\"content\":[],\"usage\":{\"input\":2967,\"output\":180,\"cacheRead\":2304,\"cacheWrite\":0,\"totalTokens\":5451,\"cost\":{}},\"stopReason\":\"stop\"}}\n",
);

/// A sub-agent session: Cola files these as their own directory and their own
/// transcript, unlike Pi's `subagent-artifacts/` copies, so they are billed.
const COLA_SUBAGENT: &str = concat!(
    "{\"type\":\"session\",\"version\":3,\"id\":\"01a0252a-fc23-7844-a17b-94d62a069428\",\"timestamp\":\"2026-08-21T16:33:49.187Z\",\"cwd\":\"/Users/me/workspace/neuro\"}\n",
    "{\"type\":\"message\",\"id\":\"99aa0011\",\"timestamp\":\"2026-08-21T16:34:01.000Z\",\"message\":{\"role\":\"assistant\",\"provider\":\"cola\",\"model\":\"luna-02\",\"content\":[],\"usage\":{\"input\":9686,\"output\":178,\"cacheRead\":0,\"cacheWrite\":512,\"reasoning\":38,\"totalTokens\":10376,\"cost\":{}},\"stopReason\":\"stop\"}}\n",
);

#[test]
fn cola_is_detected_from_its_own_sessions_root() {
    let install = ColaInstall::new();
    install.write(
        "desktop-local",
        "2026-08-21T16-32-49-187Z_01a0252a-fc23-7844-a17b-94d62a069429.jsonl",
        COLA_PARENT,
    );
    install.write(
        "desktop-local-subagent-2a0f4b0f",
        "2026-08-21T16-33-28-021Z_01a0252a-fc23-7844-a17b-94d62a069428.jsonl",
        COLA_SUBAGENT,
    );
    // Cola's own bookkeeping, which is not a transcript.
    install.write("desktop-local", "state.json", "{\"kind\":\"state\"}");

    let other = tempfile::tempdir().unwrap();
    let sessions = install.sessions();
    let _env = EnvGuard::with(&[
        ("COLA_SESSIONS_DIR", sessions.as_path()),
        // Pi pointed somewhere empty: the two roots must never cross.
        ("PI_AGENT_DIR", other.path()),
    ]);
    let detected = ColaAdapter.probe().expect("cola detected");
    assert_eq!(detected.id, "cola");
    assert_eq!(detected.display, "Cola");
    assert_eq!(detected.roots, vec![install.sessions()]);
    assert_eq!(detected.hint.as_deref(), Some("session format v3"));

    let files = ColaAdapter.discover(&DateFilter::default());
    assert_eq!(files.len(), 2, "only the *.jsonl transcripts, not state.json");
    assert!(files.iter().all(|f| f.kind == FileKind::Jsonl));
    assert!(
        PiAdapter.discover(&DateFilter::default()).is_empty(),
        "Pi reads its own root, never Cola's"
    );
}

/// The whole point of the sibling: same records, same stages, different tool id.
#[test]
fn cola_transcripts_are_stamped_cola_and_map_the_same_stages() {
    let install = ColaInstall::new();
    let parent = install.write(
        "desktop-local",
        "2026-08-21T16-32-49-187Z_01a0252a-fc23-7844-a17b-94d62a069429.jsonl",
        COLA_PARENT,
    );
    let subagent = install.write(
        "desktop-local-subagent-2a0f4b0f",
        "2026-08-21T16-33-28-021Z_01a0252a-fc23-7844-a17b-94d62a069428.jsonl",
        COLA_SUBAGENT,
    );
    let _env = EnvGuard::cola_sessions(&install.sessions());

    let mut events = Vec::new();
    for file in [&parent, &subagent] {
        let out = ColaAdapter.read(file, ReadCursor(0)).expect("fixture read");
        assert_eq!(out.cursor.0, file.size, "every line here is complete");
        events.extend(out.events);
    }

    assert_eq!(events.len(), 3, "the user turn and the aborted turn bill nothing");
    assert!(
        events.iter().all(|e| e.tool == "cola"),
        "a sibling's spend must never land on pi: {:?}",
        events.iter().map(|e| e.tool.clone()).collect::<Vec<_>>()
    );
    let mut totals = TokenCounts::default();
    for e in &events {
        assert_eq!(e.meter, Meter::Tokens);
        assert_eq!(e.dedupe_key.as_deref().unwrap().split('#').count(), 2);
        assert!(e.ts_ms > 0);
        assert!(e.quota.is_none() && e.calls.is_empty());
        totals += &e.counts;
    }
    // input 7987+2967+9686, output 165+180+178, cacheRead 12288+2304+0,
    // cacheWrite 0+0+512, reasoning 42+0+38.
    assert_eq!(totals.input, 20_640.0);
    assert_eq!(totals.output, 523.0, "reasoning is a sub-split, never added on top");
    assert_eq!(totals.cache_read, 14_592.0);
    assert_eq!(totals.cache_creation, 512.0);
    assert_eq!(totals.reasoning, 80.0);
    assert_eq!(totals.credits, 0.0, "Cola's own cost object is ignored");
    assert_eq!(
        totals.total(),
        36_267.0,
        "== the three records' own totalTokens (20440 + 5451 + 10376)"
    );

    let first = events
        .iter()
        .find(|e| e.dedupe_key.as_deref() == Some("01a0252a-fc23-7844-a17b-94d62a069429#0fe20590"))
        .expect("the first billed turn");
    assert_eq!(first.session, "01a0252a-fc23-7844-a17b-94d62a069429");
    assert_eq!(
        first.project.as_deref(),
        Some("/Users/me/workspace/memory-bank"),
        "the header's cwd, since Cola's session key is not a mangled path"
    );
    assert_eq!(first.model.as_deref(), Some("deepseek-v4-pro"));
    assert_eq!(first.source, parent.key());
    assert_eq!(first.counts.input, 7987.0);
    assert_eq!(first.counts.cache_read, 12288.0);
    // Unlike OpenCode, this dialect's `totalTokens` already excludes reasoning:
    // it is a breakdown of `output`, so it is reported but never folded in.
    assert_eq!(first.counts.output, 165.0);
    assert_eq!(first.counts.reasoning, 42.0);
    assert_eq!(first.counts.total(), 20_440.0, "== the record's own totalTokens");
}

#[test]
fn unreadable_logs_are_reported_not_raised() {
    let install = Install::new();
    let missing = absent_source_file(&install.log_path("gone.jsonl", None));
    let outcome = PiAdapter.read(&missing, ReadCursor(0)).unwrap();
    assert_eq!((outcome.events.len(), outcome.cursor), (0, ReadCursor(0)));

    // A directory opens fine but can never be read as a log.
    let dir = SourceFile {
        path: install.sessions(),
        kind: FileKind::Jsonl,
        size: 1,
        mtime_ms: 1,
    };
    let outcome = PiAdapter.read(&dir, ReadCursor(0)).unwrap();
    assert_eq!((outcome.events.len(), outcome.cursor), (0, ReadCursor(0)));
}

#[test]
fn a_truncated_log_rejects_a_cursor_past_its_end() {
    let install = Install::new();
    let file = install.place("main.jsonl", "sess-main.jsonl", None);
    let outcome = read(&file, 0);
    assert_eq!(
        keys(&outcome.events),
        vec![key("1680bdc9"), key("aaaa0002")]
    );

    // Log rotation replaces a 3 KB file with a fresh header.
    fs::write(&file.path, b"short\n").unwrap();
    let err = PiAdapter.read(&file, outcome.cursor).unwrap_err();
    assert!(
        matches!(err, Error::Cursor { cursor, .. } if cursor == outcome.cursor.0),
        "got {err:?}"
    );
}

/// Real-machine cross-check of the per-call semantics: summing our stages per
/// record must reproduce Pi's own `totalTokens` for the whole tree.
#[test]
#[ignore = "reads the real ~/.pi/agent/sessions on this machine"]
fn real_machine_logs_decompose_into_pis_own_totals() {
    let _env = EnvGuard::cleared();
    let Some(sessions) = dirs::home_dir().map(|h| h.join(".pi").join("agent").join("sessions"))
    else {
        eprintln!("no HOME: nothing to smoke test");
        return;
    };
    let adapter = PiAdapter;
    let Some(detected) = adapter.probe() else {
        eprintln!(
            "pi not detected under {}: nothing to smoke test",
            sessions.display()
        );
        return;
    };
    let files = adapter.discover(&DateFilter::default());
    let mut events = Vec::new();
    let mut bytes = 0u64;
    let mut reported = 0.0f64;
    for file in &files {
        let outcome = adapter
            .read(file, ReadCursor(0))
            .expect("real files must not error");
        bytes += outcome.cursor.0;
        reported += own_total_tokens(&file.path);
        events.extend(outcome.events);
    }

    let mut tokens = TokenCounts::default();
    let mut keys_seen = std::collections::HashSet::new();
    let mut models: std::collections::BTreeMap<String, usize> = Default::default();
    let mut projects: std::collections::BTreeMap<String, usize> = Default::default();
    for ev in &events {
        assert!(
            keys_seen.insert(ev.dedupe_key.clone()),
            "duplicate dedupe_key {:?}",
            ev.dedupe_key
        );
        assert!(ev.ts_ms > 0, "an event dated 1970: {:?}", ev.dedupe_key);
        tokens += &ev.counts;
        *models
            .entry(ev.model.clone().unwrap_or_else(|| "?".into()))
            .or_default() += 1;
        *projects
            .entry(ev.project.clone().unwrap_or_else(|| "?".into()))
            .or_default() += 1;
    }
    let ours = tokens.total();
    eprintln!(
        "roots {:?} hint {:?}\n{} files, {bytes} bytes -> {} events ({} distinct ids), {} models, {} projects",
        detected.roots,
        detected.hint,
        files.len(),
        events.len(),
        keys_seen.len(),
        models.len(),
        projects.len(),
    );
    eprintln!(
        "tokens in {:.0} cache_creation {:.0} cache_read {:.0} out {:.0} reasoning {:.0} total {:.0}",
        tokens.input, tokens.cache_creation, tokens.cache_read, tokens.output, tokens.reasoning, ours
    );
    assert!(
        (ours - reported).abs() < 1.0,
        "per-call decomposition must equal the sum of Pi's own totalTokens: ours {ours}, reported {reported}"
    );
    assert!(
        tokens.reasoning <= tokens.output,
        "reasoning must stay inside output"
    );
    for (model, count) in models.iter().take(12) {
        eprintln!("  model {model}: {count} events");
    }
    assert!(files.len() > 100, "only {} files", files.len());
    assert!(events.len() > 10_000, "only {} events", events.len());
}

/// Independent re-reading of the same files: the sum of every assistant
/// record's own `totalTokens`, which is what a per-call mapping must reproduce.
fn own_total_tokens(path: &Path) -> f64 {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut per_record: HashMap<String, f64> = HashMap::new();
    let mut anon = 0.0;
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if record.get("type").and_then(serde_json::Value::as_str) != Some("message") {
            continue;
        }
        let Some(message) = record.get("message") else {
            continue;
        };
        if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
            continue;
        }
        let total = message
            .get("usage")
            .and_then(|u| u.get("totalTokens"))
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        match record.get("id").and_then(serde_json::Value::as_str) {
            // Largest snapshot per id, exactly like the adapter.
            Some(id) => {
                let slot = per_record.entry(id.to_string()).or_insert(0.0);
                *slot = slot.max(total);
            }
            None => anon += total,
        }
    }
    per_record.values().sum::<f64>() + anon
}

fn paths(files: &[SourceFile]) -> Vec<PathBuf> {
    files.iter().map(|f| f.path.clone()).collect()
}

/// Real-machine cross-check for the sibling: our per-stage sums over
/// `~/.cola/sessions` must equal a line-by-line JSON scan written here, against
/// the record shape, with no adapter code in the way. Both are printed.
#[test]
#[ignore = "reads the real ~/.cola/sessions on this machine"]
fn real_machine_cola_sessions_match_an_independent_scan() {
    let _env = EnvGuard::cleared();
    let Some(sessions) = dirs::home_dir().map(|h| h.join(".cola").join("sessions")) else {
        eprintln!("no HOME: nothing to measure");
        return;
    };
    let Some(detected) = ColaAdapter.probe() else {
        eprintln!("cola not detected under {}: nothing to measure", sessions.display());
        return;
    };
    let files = ColaAdapter.discover(&DateFilter::default());

    let mut ours = TokenCounts::default();
    let mut events = 0usize;
    let mut ids = std::collections::HashSet::new();
    for file in &files {
        let mut cursor = ReadCursor(0);
        loop {
            let out = ColaAdapter.read(file, cursor).expect("real files must not error");
            for e in &out.events {
                assert_eq!(e.tool, "cola", "a sibling must not be billed as pi");
                assert!(ids.insert(e.dedupe_key.clone()), "duplicate dedupe_key {:?}", e.dedupe_key);
                ours += &e.counts;
                events += 1;
            }
            if out.cursor == cursor {
                break;
            }
            cursor = out.cursor;
        }
    }
    let direct = scan_cola_records(&files);

    eprintln!(
        "cola {:?} hint {:?}\n{} files -> {} events",
        detected.roots,
        detected.hint,
        files.len(),
        events,
    );
    eprintln!(
        "adapter   in {:.0} cache_creation {:.0} cache_read {:.0} out {:.0} reasoning {:.0} total {:.0}",
        ours.input, ours.cache_creation, ours.cache_read, ours.output, ours.reasoning, ours.total()
    );
    eprintln!(
        "scan      in {:.0} cache_creation {:.0} cache_read {:.0} out {:.0} reasoning {:.0} total {:.0}",
        direct.input,
        direct.cache_creation,
        direct.cache_read,
        direct.output,
        direct.reasoning,
        direct.total()
    );
    for (label, ours, direct) in [
        ("input", ours.input, direct.input),
        ("cache_creation", ours.cache_creation, direct.cache_creation),
        ("cache_read", ours.cache_read, direct.cache_read),
        ("output", ours.output, direct.output),
        ("reasoning", ours.reasoning, direct.reasoning),
        ("total", ours.total(), direct.total()),
    ] {
        assert!(
            (ours - direct).abs() < 1.0,
            "{label} differs: adapter {ours}, independent scan {direct}"
        );
    }
    assert!(events > 50, "expected the 65 usage records measured here, got {events}");
    assert!(files.len() >= 7, "expected one file per session dir, got {}", files.len());
}

/// Every billed Cola turn, summed straight from the JSONL: `input`,
/// `cacheWrite` and `cacheRead` are separate stages, `output` stands alone
/// (`reasoning` is a breakdown of it), and a repeated record id is one call
/// written twice so only its largest snapshot counts.
fn scan_cola_records(files: &[SourceFile]) -> TokenCounts {
    let mut per_id: HashMap<(PathBuf, String), TokenCounts> = HashMap::new();
    let mut anon = Vec::new();
    for file in files {
        let Ok(text) = fs::read_to_string(&file.path) else { continue };
        for line in text.lines() {
            let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if record.get("type").and_then(serde_json::Value::as_str) != Some("message") {
                continue;
            }
            let Some(message) = record.get("message").filter(|m| m.is_object()) else {
                continue;
            };
            if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(usage) = message.get("usage").filter(|u| u.is_object()) else { continue };
            let field = |key: &str| {
                usage
                    .get(key)
                    .and_then(serde_json::Value::as_f64)
                    .filter(|n| *n > 0.0)
                    .unwrap_or(0.0)
            };
            let counts = TokenCounts {
                input: field("input"),
                cache_creation: field("cacheWrite"),
                cache_read: field("cacheRead"),
                output: field("output"),
                reasoning: field("reasoning"),
                credits: 0.0,
            };
            if counts.total() == 0.0 {
                continue;
            }
            match record.get("id").and_then(serde_json::Value::as_str) {
                Some(id) => {
                    let slot = per_id
                        .entry((file.path.clone(), id.to_string()))
                        .or_default();
                    if counts.total() > slot.total() {
                        *slot = counts;
                    }
                }
                None => anon.push(counts),
            }
        }
    }
    let mut out = TokenCounts::default();
    for counts in per_id.values().chain(anon.iter()) {
        out += counts;
    }
    out
}

//! Behaviour tests against synthetic transcripts written into a fake
//! `$ATOMCODE_HOME/sessions` install, plus one ignored cross-check against the
//! real `~/.atomcode` on this machine.
//!
//! Every convention asserted here (inclusive `prompt`, `turn_id#ts` identity, ms
//! `ts`) is measured and cited in `src/parser.rs`'s module doc.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use usage_adapter_atomcode::{AtomCodeAdapter, TOOL_ID};
use usage_core::{
    parse_ts_ms, DateFilter, Error, FileKind, Meter, ReadCursor, ReadOutcome, SourceAdapter,
    SourceFile, TokenCounts, UsageEvent,
};

/// `ATOMCODE_HOME` is process-global, so whoever flips it holds the lock.
static ENV: Mutex<()> = Mutex::new(());
const OVERRIDE: &str = "ATOMCODE_HOME";

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Option<OsString>,
}

impl EnvGuard {
    /// Points AtomCode's data dir at a fixture tree.
    fn home(root: &Path) -> Self {
        Self::set(Some(root))
    }

    /// The real `~/.atomcode`, for the ignored smoke test.
    fn cleared() -> Self {
        Self::set(None)
    }

    fn set(value: Option<&Path>) -> Self {
        let lock = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = std::env::var_os(OVERRIDE);
        match value {
            Some(path) => std::env::set_var(OVERRIDE, path),
            None => std::env::remove_var(OVERRIDE),
        }
        EnvGuard { _lock: lock, saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.saved {
            Some(value) => std::env::set_var(OVERRIDE, value),
            None => std::env::remove_var(OVERRIDE),
        }
    }
}

const PROJECT: &str = "45d727130d2f41d9";
const OTHER_PROJECT: &str = "af6efc199a23a581";
const SESSION: &str = "242f9f29-b726-4555-bf38-604c610149b3";
const WORKSPACE: &str = "/Users/me/workspace/deveco";

/// `<tmp>/.atomcode/sessions/<project_hash>`, the real layout.
struct Install {
    dir: tempfile::TempDir,
}

impl Install {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".atomcode")).unwrap();
        Install { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join(".atomcode")
    }

    fn sessions(&self) -> PathBuf {
        self.home().join("sessions")
    }

    fn log_path(&self, id: &str, project: &str) -> PathBuf {
        self.sessions().join(project).join(format!("{id}.jsonl"))
    }

    /// Writes `<id>.jsonl` and, unless told otherwise, the `<id>.meta` beside it
    /// that carries the only workspace label AtomCode records.
    fn place(&self, id: &str, body: &str, working_dir: Option<&str>) -> SourceFile {
        self.place_in(id, body, working_dir, PROJECT)
    }

    fn place_in(&self, id: &str, body: &str, working_dir: Option<&str>, project: &str) -> SourceFile {
        let log = self.log_path(id, project);
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, body).unwrap();
        if let Some(dir) = working_dir {
            // AtomCode pretty-prints the meta; the parser must not care.
            fs::write(
                log.with_extension("meta"),
                format!(
                    "{{\n  \"v\": 1,\n  \"id\": \"{id}\",\n  \"working_dir\": \"{dir}\",\n  \"turn_count\": 1\n}}\n"
                ),
            )
            .unwrap();
        }
        source_file(&log)
    }

    fn append(&self, file: &SourceFile, bytes: &str) {
        let mut handle = OpenOptions::new().append(true).open(&file.path).unwrap();
        handle.write_all(bytes.as_bytes()).unwrap();
    }
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

/// One `TurnRecord` as `transcript.rs` writes it, minus the `iso` mirror.
fn turn(session: &str, turn_id: u64, ts: i64, prompt: u64, completion: u64, cached: u64) -> String {
    format!(
        r#"{{"v":1,"ts":{ts},"session_id":"{session}","turn_id":{turn_id},"undone":false,"user":"上下文多少","assistant":"GLM-5.2 是 200K","reasoning":"The user wants me to","tools":[],"usage":{{"prompt":{prompt},"completion":{completion},"cached":{cached}}}}}"#
    )
}

fn read(file: &SourceFile, cursor: u64) -> ReadOutcome {
    AtomCodeAdapter
        .read(file, ReadCursor(cursor))
        .expect("fixture read")
}

fn key(turn_id: u64, ts: i64) -> String {
    format!("{SESSION}#{turn_id}#{ts}")
}

fn keys(events: &[UsageEvent]) -> Vec<String> {
    events.iter().filter_map(|e| e.dedupe_key.clone()).collect()
}

fn event(events: &[UsageEvent], turn_id: u64, ts: i64) -> UsageEvent {
    let want = key(turn_id, ts);
    events
        .iter()
        .find(|e| e.dedupe_key.as_deref() == Some(want.as_str()))
        .cloned()
        .unwrap_or_else(|| panic!("no event for {want}, got {:?}", keys(events)))
}

/// The mapping table itself: `prompt` is the inclusive prompt total, `cached` is
/// its cache-read subset, and the transcript records neither a cache write nor a
/// model.
#[test]
fn usage_maps_onto_the_shared_token_stages() {
    let install = Install::new();
    // Real first two turns of `242f9f29…`, in the order the journal holds them.
    let body = format!(
        "{}\n{}\n",
        turn(SESSION, 1, 1785118996634, 18993, 1018, 18176),
        turn(SESSION, 2, 1785120106781, 67972, 12220, 67456),
    );
    let file = install.place(SESSION, &body, Some(WORKSPACE));
    let outcome = read(&file, 0);

    assert_eq!(keys(&outcome.events), vec![key(1, 1785118996634), key(2, 1785120106781)]);
    let first = &outcome.events[0];
    assert_eq!(first.tool, TOOL_ID);
    assert_eq!(first.tool, "atomcode");
    assert_eq!(first.session, SESSION);
    assert_eq!(first.project.as_deref(), Some(WORKSPACE));
    assert_eq!(first.model, None, "`TurnRecord` carries no model field");
    assert_eq!(first.meter, Meter::Tokens);
    assert_eq!(first.source, file.key());
    assert!(first.calls.is_empty(), "a turn with no tool calls records none");
    assert_eq!(first.ts_ms, 1785118996634, "`ts` is epoch milliseconds, verbatim");
    assert_eq!(
        (first.counts.input, first.counts.cache_read, first.counts.output),
        (817.0, 18176.0, 1018.0),
        "`prompt` 18993 minus `cached` 18176 is the net input"
    );
    assert_eq!(first.counts.total(), 20011.0, "== prompt + completion");
    assert_eq!(first.counts.cache_creation, 0.0, "Anthropic writes cache_creation into prompt");
    assert_eq!(first.counts.reasoning, 0.0, "the record's `reasoning` field is prose");
    assert_eq!(first.counts.credits, 0.0);

    let second = event(&outcome.events, 2, 1785120106781);
    assert_eq!(
        (second.counts.input, second.counts.cache_read, second.counts.output),
        (516.0, 67456.0, 12220.0)
    );

    // SEMANTICS: one record per turn, summed as they stand.
    let mut all = TokenCounts::default();
    for ev in &outcome.events {
        all += &ev.counts;
    }
    assert_eq!((all.input, all.cache_read, all.output), (1333.0, 85632.0, 13238.0));
    assert_eq!(all.total(), 100_203.0);
}

/// A turn flushed at an error terminal that billed nothing must not reach the
/// totals, and a cache hit that overruns the prompt must not go negative.
#[test]
fn a_zero_usage_turn_is_skipped_and_an_overrun_hit_stays_non_negative() {
    let install = Install::new();
    let body = format!(
        "{}\n{}\n",
        turn(SESSION, 1, 1784361677893, 0, 0, 0),
        // `b7954235…` turn 2: `cached` is the max over rounds, `prompt` the last
        // round's, so the hit legitimately exceeds the prompt by 2535.
        turn(SESSION, 2, 1784361678893, 99865, 23706, 102400),
    );
    let file = install.place(SESSION, &body, Some(WORKSPACE));
    let outcome = read(&file, 0);

    assert_eq!(keys(&outcome.events), vec![key(2, 1784361678893)]);
    let kept = &outcome.events[0];
    assert_eq!(kept.counts.input, 0.0, "clamped, never negative");
    assert_eq!(kept.counts.cache_read, 102_400.0);
    assert_eq!(kept.counts.total(), 126_106.0);
    assert_eq!(outcome.cursor.0, file.size, "both lines counted as processed bytes");
}

/// `turn_id` is "monotonic within the session" and re-used; the flush time is
/// what separates two billed calls, while an identical key is one snapshot.
#[test]
fn a_repeated_turn_id_bills_both_calls_but_a_repeated_key_once() {
    let install = Install::new();
    // Written twice, byte for byte: one flush the journal holds twice.
    let twice = turn(SESSION, 5, 1784356883391, 125967, 152, 123904);
    let body = [
        // The real `b7954235…` collision: same turn, 62 s apart, different prompts.
        turn(SESSION, 4, 1784356785786, 129996, 3176, 129024),
        turn(SESSION, 4, 1784356847788, 124747, 4182, 122880),
        twice.clone(),
        twice,
    ]
    .join("\n")
        + "\n";
    let file = install.place(SESSION, &body, Some(WORKSPACE));
    let outcome = read(&file, 0);

    assert_eq!(
        keys(&outcome.events),
        vec![key(4, 1784356785786), key(4, 1784356847788), key(5, 1784356883391)],
        "both turn-4 calls are billed, the duplicated turn-5 line is not"
    );
    assert_eq!(event(&outcome.events, 4, 1784356785786).counts.total(), 133_172.0);
    assert_eq!(event(&outcome.events, 4, 1784356847788).counts.total(), 128_929.0);
    let twice = event(&outcome.events, 5, 1784356883391);
    assert_eq!(twice.counts.total(), 126_119.0, "the snapshot is kept, not summed");
    assert_eq!(twice.counts.input, 2063.0);

    // Distinct sessions may share a `turn_id`; the prefix keeps them apart.
    let body = format!(
        "{}\n{}\n",
        turn(SESSION, 1, 1784361677893, 100, 5, 64),
        turn("1296ccb5-86eb-4899-ae7a-996bbba28857", 1, 1784361677893, 200, 6, 64),
    );
    let other = install.place("1296ccb5-86eb-4899-ae7a-996bbba28857", &body, None);
    let outcome = read(&other, 0);
    assert_eq!(outcome.events.len(), 2, "an identical turn id in two sessions is two turns");
    // 200 prompt of which 64 were cached, plus 6 out.
    assert_eq!(outcome.events[1].counts.total(), 206.0);
    assert_eq!(
        outcome.events[1].project, None,
        "no `<id>.meta` beside it, so no label is invented"
    );
}

/// `ts` (ms) wins, `iso` (RFC 3339 with an offset) is its mirror, and a record
/// with neither is filed under the file's mtime rather than epoch 0.
#[test]
fn timestamps_are_read_in_both_dialects() {
    let install = Install::new();
    let body = format!(
        "{}\n",
        r#"{"v":1,"ts":1784361677893,"iso":"2026-07-18T08:01:17.893+00:00","session_id":"dialects","turn_id":1,"user":"a","assistant":"b","usage":{"prompt":10,"completion":1,"cached":0}}"#
    ) + &format!(
        "{}\n",
        // No `ts`: the RFC 3339 mirror dates it, offset and all.
        r#"{"v":1,"iso":"2026-07-18T09:02:03.500+00:00","session_id":"dialects","turn_id":2,"user":"a","assistant":"b","usage":{"prompt":20,"completion":2,"cached":0}}"#
    ) + &format!(
        "{}\n",
        // A 10-digit `ts` is seconds, and it still wins over a bogus `iso`.
        r#"{"v":1,"ts":1784361677,"iso":"not-a-date","session_id":"dialects","turn_id":3,"user":"a","assistant":"b","usage":{"prompt":30,"completion":3,"cached":0}}"#
    ) + &format!(
        "{}\n",
        r#"{"v":1,"session_id":"dialects","turn_id":4,"user":"a","assistant":"b","usage":{"prompt":40,"completion":4,"cached":0}}"#
    );
    let file = install.place("dialects", &body, Some(WORKSPACE));
    let outcome = read(&file, 0);
    assert_eq!(outcome.events.len(), 4);
    assert_eq!(outcome.events[0].ts_ms, 1784361677893);
    assert_eq!(
        outcome.events[1].ts_ms,
        parse_ts_ms("2026-07-18T09:02:03.500+00:00").unwrap()
    );
    assert_eq!(outcome.events[2].ts_ms, 1784361677000, "seconds scaled to ms");
    assert_eq!(outcome.events[3].ts_ms, file.mtime_ms, "undated, so the file's mtime");
    for ev in &outcome.events {
        assert!(ev.ts_ms > 1_700_000_000_000, "a 1970 bucket: {:?}", ev.dedupe_key);
        assert_eq!(ev.session, "dialects");
    }
    assert_eq!(outcome.events[3].dedupe_key, None, "no `ts`, so nothing to key on");
}

/// Append-only journal with a byte cursor: the second pass sees only the turns
/// that landed since, and a half-written line waits for the rest of itself.
#[test]
fn reading_resumes_from_the_returned_byte_cursor() {
    let install = Install::new();
    let first = turn(SESSION, 1, 1785118996634, 18993, 1018, 18176);
    // Cut a real record at its `usage` key: the journal genuinely holds the
    // prefix while the turn is still streaming.
    let full = turn(SESSION, 2, 1785120106781, 67972, 12220, 67456);
    let cut = full.find("\"usage\"").expect("fixture record carries a usage field");
    let file = install.place(SESSION, &format!("{first}\n{}", &full[..cut]), Some(WORKSPACE));

    let pass = read(&file, 0);
    assert_eq!(keys(&pass.events), vec![key(1, 1785118996634)]);
    assert_eq!(
        pass.cursor.0,
        first.len() as u64 + 1,
        "the cursor stops before the line still being written"
    );
    assert!(pass.cursor.0 < file.size);

    // Re-reading from the cursor is a no-op while the tail stays torn.
    let again = AtomCodeAdapter.read(&file, pass.cursor).unwrap();
    assert!(again.events.is_empty());
    assert_eq!(again.cursor, pass.cursor);

    install.append(&file, &format!("{}\n", &full[cut..]));
    let grown = source_file(&file.path);
    let next = AtomCodeAdapter.read(&grown, again.cursor).unwrap();
    assert_eq!(keys(&next.events), vec![key(2, 1785120106781)]);
    assert_eq!(next.cursor.0, grown.size, "every byte is consumed now");
    let late = &next.events[0];
    assert_eq!(
        (late.counts.input, late.counts.cache_read, late.counts.output),
        (516.0, 67456.0, 12220.0)
    );
    // The header-ish labels survive a window that no longer contains the earlier
    // lines: the meta beside the file, not the previous window.
    assert_eq!(late.session, SESSION);
    assert_eq!(late.project.as_deref(), Some(WORKSPACE));

    let exhausted = AtomCodeAdapter.read(&grown, next.cursor).unwrap();
    assert!(exhausted.events.is_empty());
    assert_eq!(exhausted.cursor, next.cursor);
}

/// A new turn appended to the journal is the only new event, and a truncated
/// transcript rejects the stale cursor instead of silently re-reading.
#[test]
fn an_appended_turn_is_the_only_new_event_and_a_shrink_is_reported() {
    let install = Install::new();
    let file = install.place(
        "fa4d2255-010c-4a34-baf5-c3c4be2532e3",
        &format!("{}\n", turn("fa4d2255", 1, 1784361677893, 27376, 1182, 26880)),
        None,
    );
    let first = read(&file, 0);
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.events[0].counts.total(), 28_558.0);

    install.append(&file, &(turn("fa4d2255", 2, 1784361720279, 27562, 98, 27392) + "\n"));
    let grown = source_file(&file.path);
    let next = AtomCodeAdapter.read(&grown, first.cursor).unwrap();
    assert_eq!(next.events.len(), 1);
    assert_eq!(next.events[0].dedupe_key.as_deref(), Some("fa4d2255#2#1784361720279"));
    assert_eq!(next.events[0].counts.input, 170.0);
    assert_eq!(next.cursor.0, grown.size);

    // Log rotation replaces an 8 KB transcript with a fresh header.
    fs::write(&file.path, b"{\"v\":1}\n").unwrap();
    let err = AtomCodeAdapter.read(&grown, next.cursor).unwrap_err();
    assert!(
        matches!(err, Error::Cursor { cursor, .. } if cursor == next.cursor.0),
        "got {err:?}"
    );
}

#[test]
fn tool_calls_record_only_the_extensions() {
    let install = Install::new();
    let body = format!(
        "{}\n",
        r#"{"v":1,"ts":1785118996634,"session_id":"callers","turn_id":1,"user":"a","assistant":"b","tools":[{"name":"bash","args":"{\"command\":\"ls\"}","result":"ok","is_error":false},{"name":"mcp__bugx__run","args":"{}","result":"ok","is_error":false},{"name":"use_skill","args":"{\"name\":\"atomcode:ask\",\"arguments\":\"q\"}","result":"ok","is_error":false}],"usage":{"prompt":500,"completion":20,"cached":400}}"#
    );
    let file = install.place("callers", &body, Some(WORKSPACE));
    let outcome = read(&file, 0);
    let calls = &outcome.events[0].calls;
    assert_eq!(calls.len(), 2, "`bash` is a built-in");
    assert_eq!(calls[0].name, "bugx");
    assert_eq!(calls[0].kind, usage_core::CallKind::Mcp);
    assert_eq!(calls[1].name, "atomcode:ask");
    assert_eq!(calls[1].kind, usage_core::CallKind::Skill);
}

#[test]
fn discover_and_probe_follow_the_env_override() {
    let install = Install::new();
    let first = install.place(
        SESSION,
        &format!("{}\n", turn(SESSION, 1, 1785118996634, 18993, 1018, 18176)),
        Some(WORKSPACE),
    );
    let second = install.place_in(
        "1296ccb5-86eb-4899-ae7a-996bbba28857",
        &format!("{}\n", turn("1296ccb5", 1, 1784903392036, 33764, 3288, 20224)),
        None,
        OTHER_PROJECT,
    );
    // The siblings AtomCode writes beside a transcript: none is a source.
    fs::write(install.log_path(SESSION, PROJECT).with_extension("snapshot"), "{}").unwrap();
    fs::write(install.log_path(SESSION, PROJECT).with_extension("ui.json"), "{}").unwrap();
    fs::write(install.log_path(SESSION, PROJECT).with_extension("lease"), "").unwrap();
    fs::write(install.sessions().join("config-note.md"), "not a log").unwrap();

    let _env = EnvGuard::home(&install.home());
    let all = AtomCodeAdapter.discover(&DateFilter::default());
    assert_eq!(
        all.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        // Sorted by full path, so the project bucket dominates the session id:
        // `45d7…` before `af6e…`.
        vec![first.path.clone(), second.path.clone()],
        "sorted, transcripts only"
    );
    assert!(all.iter().all(|f| f.kind == FileKind::Jsonl));

    let detected = AtomCodeAdapter.probe().expect("one readable transcript is enough");
    assert_eq!(detected.id, TOOL_ID);
    assert_eq!(detected.display, "AtomCode");
    assert_eq!(detected.roots, vec![install.sessions()]);
    assert_eq!(detected.hint.as_deref(), Some("turn record v1"));

    // A mtime older than `since` is skipped before anything is parsed.
    let ancient = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_000_000);
    OpenOptions::new()
        .write(true)
        .open(&first.path)
        .unwrap()
        .set_modified(ancient)
        .unwrap();
    let fresh = source_file(&first.path);
    assert!(fresh.mtime_ms < millis(SystemTime::now() - Duration::from_secs(86_400)));
    let recent = AtomCodeAdapter.discover(&DateFilter::new(Some(fresh.mtime_ms + 1), None));
    assert_eq!(recent.len(), 1, "the 2020 transcript must be skipped");
    assert_eq!(recent[0].path, second.path);

    // An empty override counts as unset, so no fixture tree is implied.
    drop(_env);
    let _empty = EnvGuard::set(Some(Path::new("")));
    assert_ne!(
        AtomCodeAdapter.probe().map(|d| d.roots),
        Some(vec![install.sessions()]),
        "`ATOMCODE_HOME=` must not resolve to the tree beside the fixtures"
    );
}

#[test]
fn unreadable_transcripts_are_reported_not_raised() {
    let install = Install::new();
    let missing = absent_source_file(&install.log_path("gone", PROJECT));
    let outcome = AtomCodeAdapter.read(&missing, ReadCursor(0)).unwrap();
    assert_eq!((outcome.events.len(), outcome.cursor), (0, ReadCursor(0)));

    // A directory opens fine but can never be read as a transcript.
    let dir = SourceFile {
        path: install.sessions(),
        kind: FileKind::Jsonl,
        size: 1,
        mtime_ms: 1,
    };
    let outcome = AtomCodeAdapter.read(&dir, ReadCursor(0)).unwrap();
    assert_eq!((outcome.events.len(), outcome.cursor), (0, ReadCursor(0)));

    // Garbage lines are skipped, and their bytes still move the cursor on.
    let junk = install.place(
        "junk",
        "{not json at all}\n\
         {\"v\":1,\"ts\":1785118996634,\"session_id\":\"junk\",\"turn_id\":1,\"usage\":\"a string\"}\n\
         \n\
         {\"v\":1,\"ts\":1785118996635,\"session_id\":\"junk\",\"turn_id\":2,\"usage\":{\"prompt\":\"7\",\"completion\":null,\"cached\":-5}}\n",
        None,
    );
    let outcome = read(&junk, 0);
    assert_eq!(outcome.events.len(), 1, "the one parsable usage record survives");
    assert_eq!(outcome.events[0].counts.input, 7.0, "a stringified number still parses");
    assert_eq!(outcome.events[0].counts.output, 0.0, "null and negative stay zero");
    assert_eq!(outcome.cursor.0, junk.size, "garbage counts as processed bytes");
}

/// Real-machine cross-check: what the adapter totals must equal what an
/// independent line-by-line scan of the same transcripts computes.
#[test]
#[ignore = "reads the real ~/.atomcode/sessions on this machine"]
fn real_machine_transcripts_match_an_independent_scan() {
    let _env = EnvGuard::cleared();
    let Some(home) = dirs::home_dir() else {
        eprintln!("no HOME: nothing to smoke test");
        return;
    };
    let adapter = AtomCodeAdapter;
    let Some(detected) = adapter.probe() else {
        eprintln!(
            "atomcode not detected under {}: nothing to smoke test",
            home.join(".atomcode").display()
        );
        return;
    };
    assert_eq!(detected.roots, vec![home.join(".atomcode").join("sessions")]);
    assert_eq!(
        detected.hint.as_deref(),
        Some("turn record v1"),
        "`probe` must name the schema even though a real record runs past its head window"
    );
    let files = adapter.discover(&DateFilter::default());
    let mut events = Vec::new();
    let mut bytes = 0_u64;
    for file in &files {
        let outcome = adapter.read(file, ReadCursor(0)).expect("real files must not error");
        assert_eq!(outcome.cursor.0, file.size, "a real transcript ends on a newline");
        bytes += outcome.cursor.0;
        events.extend(outcome.events);
        // A second pass from the returned cursor must add nothing at all.
        let again = adapter.read(file, outcome.cursor).unwrap();
        assert!(again.events.is_empty() && again.cursor == outcome.cursor);
    }

    let scan = Scan::run(&files);
    let mut ours = TokenCounts::default();
    let mut seen = HashSet::new();
    for ev in &events {
        assert!(
            seen.insert(ev.dedupe_key.clone()),
            "duplicate dedupe_key {:?}",
            ev.dedupe_key
        );
        assert!(ev.dedupe_key.is_some(), "every real record is namable: {ev:?}");
        assert!(ev.ts_ms > 1_700_000_000_000, "an event dated 1970: {:?}", ev.dedupe_key);
        assert_eq!(ev.tool, TOOL_ID);
        assert_eq!(ev.model, None, "the transcript records no model");
        assert_eq!(ev.counts.cache_creation, 0.0);
        assert_eq!(ev.counts.reasoning, 0.0);
        assert!(!ev.session.is_empty());
        ours += &ev.counts;
    }

    eprintln!(
        "roots {:?} hint {:?}\n{} transcripts, {bytes} bytes -> {} events ({} distinct keys)",
        detected.roots,
        detected.hint,
        files.len(),
        events.len(),
        seen.len(),
    );
    eprintln!(
        "adapter : input {:.0} cache_read {:.0} output {:.0} total {:.0}",
        ours.input, ours.cache_read, ours.output, ours.total()
    );
    eprintln!(
        "scan    : net input {:.0} cached {:.0} completion {:.0} prompt {:.0} clamp {:.0}",
        scan.net_input, scan.cached, scan.completion, scan.prompt, scan.clamp_loss
    );
    eprintln!(
        "raw     : prompt {:.0} completion {:.0} cached {:.0} ({} usage records, {} of them reusing a (session,turn_id), {} clamp rows)",
        scan.prompt, scan.completion, scan.cached, scan.records, scan.collisions, scan.clamped
    );

    // Independent equality: each stage must match the direct scan exactly.
    assert_eq!(ours.output, scan.completion, "output == every record's own `completion`");
    assert_eq!(ours.cache_read, scan.cached, "cache_read == every record's own `cached`");
    assert_eq!(ours.input, scan.net_input, "input == the clamped `prompt - cached`");
    assert_eq!(
        ours.total(),
        scan.prompt + scan.completion + scan.clamp_loss,
        "our total is the prompt total plus the completion total, less nothing \
         but the one clamp (cached overrun across a turn's rounds)"
    );
    assert_eq!(ours.credits, 0.0);

    // The measured baseline this adapter was written against, as a floor: an
    // append-only journal can only grow.
    assert!(
        scan.prompt >= 7_088_400.0 && scan.completion >= 372_734.0 && scan.cached >= 6_919_936.0,
        "expected at least the measured 7 088 400 / 372 734 / 6 919 936, got {:?}",
        (scan.prompt, scan.completion, scan.cached)
    );
    assert!(scan.records >= 64, "only {} usage records", scan.records);
    assert_eq!(events.len(), scan.records, "one event per usage record");
    assert!(files.len() >= 20, "only {} transcripts", files.len());
}

/// The same arithmetic done without the adapter: a direct scan of the same
/// `.jsonl` files, keyed and merged from scratch.
#[derive(Default)]
struct Scan {
    records: usize,
    prompt: f64,
    completion: f64,
    cached: f64,
    net_input: f64,
    clamp_loss: f64,
    clamped: usize,
    collisions: usize,
}

impl Scan {
    fn run(files: &[SourceFile]) -> Scan {
        let mut scan = Scan::default();
        // Largest snapshot per identity, then summed: an independent restatement
        // of the adapter's rules, written straight from the JSON.
        let mut best: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut per_turn: HashMap<(String, u64), usize> = HashMap::new();
        for file in files {
            let Ok(text) = fs::read_to_string(&file.path) else { continue };
            for line in text.lines() {
                let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let Some(usage) = record.get("usage").filter(|u| u.is_object()) else {
                    continue;
                };
                let field = |name: &str| -> f64 {
                    usage.get(name).and_then(serde_json::Value::as_f64).unwrap_or(0.0)
                };
                let (prompt, completion, cached) =
                    (field("prompt"), field("completion"), field("cached"));
                scan.records += 1;
                let session = record
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let turn = record.get("turn_id").and_then(serde_json::Value::as_u64).unwrap_or(0);
                let ts = record.get("ts").and_then(serde_json::Value::as_i64).unwrap_or(0);
                *per_turn.entry((session.clone(), turn)).or_default() += 1;
                let key = format!("{session}#{turn}#{ts}");
                let total = prompt + completion;
                match best.get_mut(&key) {
                    Some(previous) if previous.0 >= total => {}
                    Some(previous) => *previous = (total, prompt, completion, cached),
                    None => {
                        order.push(key.clone());
                        best.insert(key, (total, prompt, completion, cached));
                    }
                }
            }
        }
        scan.collisions = per_turn.values().filter(|&&seen| seen > 1).sum::<usize>()
            - per_turn.values().filter(|&&seen| seen > 1).count();
        for key in order {
            let (_, prompt, completion, cached) = best[&key];
            scan.prompt += prompt;
            scan.completion += completion;
            scan.cached += cached;
            scan.net_input += (prompt - cached).max(0.0);
            if cached > prompt {
                scan.clamp_loss += cached - prompt;
                scan.clamped += 1;
            }
        }
        scan
    }
}

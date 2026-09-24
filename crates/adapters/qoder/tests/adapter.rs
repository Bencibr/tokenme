//! Behaviour tests against fixture logs copied into a fake `~/.qoder` install.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::Value;
use usage_adapter_qoder::{QoderAdapter, TOOL_ID};
use usage_core::{
    parse_ts_ms, DateFilter, Error, FileKind, Meter, ModelAttr, ReadCursor, ReadOutcome, Semantics,
    SourceAdapter, SourceFile, UsageEvent, UsageForm,
};

/// Every root this adapter honours is process-global, so whoever flips them holds
/// one lock. The two IDE-home vars are in the list because a test that only points
/// at `QODER_CONFIG_DIR` would otherwise have `discover` reach into the real
/// `~/Library/Application Support/Qoder` cache db next to it.
static ENV: Mutex<()> = Mutex::new(());
const NAMES: [&str; 4] = [
    "QODER_CONFIG_DIR",
    "QODERCN_CONFIG_DIR",
    "QODER_HOME",
    "QODER_CN_HOME",
];

struct EnvRoot {
    _guard: MutexGuard<'static, ()>,
    previous: [Option<std::ffi::OsString>; 4],
}

impl EnvRoot {
    /// Points **both** stores — the transcript tree and the IDE cache db — at `root`,
    /// so nothing a hermetic test does can touch a real install.
    fn point_at(root: &Path) -> Self {
        let this = Self::take();
        std::env::set_var(NAMES[0], root);
        std::env::set_var(NAMES[2], root);
        for name in [NAMES[1], NAMES[3]] {
            std::env::remove_var(name);
        }
        this
    }

    /// The real `~/.qoder` and the real application support, with any override
    /// cleared. Holding the same lock matters: a parallel test's override would
    /// otherwise redirect `probe`/`discover` into somebody else's temp dir.
    fn cleared() -> Self {
        let this = Self::take();
        for name in NAMES {
            std::env::remove_var(name);
        }
        this
    }

    fn take() -> Self {
        let guard = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::array::from_fn(|i| std::env::var_os(NAMES[i]));
        EnvRoot {
            _guard: guard,
            previous,
        }
    }
}

impl Drop for EnvRoot {
    fn drop(&mut self) {
        for (i, name) in NAMES.into_iter().enumerate() {
            match self.previous[i].take() {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

const WORKSPACE: &str = "-Users-demo-proj";
const SESSION: &str = "6f1d2c3b-9a8b-4c7d-8e9f-0a1b2c3d4e5f";
const SIDE_SESSION: &str = "2b2b2b2b-0000-4000-8000-000000000002";

/// `<tmp>/.qoder/projects/-Users-demo-proj`, i.e. the real layout.
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
        self.dir.path().join(".qoder")
    }

    fn projects(&self) -> PathBuf {
        self.root().join("projects")
    }

    fn workspace(tmp: &Path) -> PathBuf {
        tmp.join(".qoder").join("projects").join(WORKSPACE)
    }

    /// `nested` mirrors the `<session>/subagents/` layout the real tree uses.
    fn log_path(&self, name: &str, nested: Option<&str>) -> PathBuf {
        match nested {
            Some(dir) => Self::workspace(self.dir.path()).join(dir).join(name),
            None => Self::workspace(self.dir.path()).join(name),
        }
    }

    fn place(&self, fixture: &str, name: &str, nested: Option<&str>) -> SourceFile {
        let target = self.log_path(name, nested);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(fixture_path(fixture), &target).expect("copy fixture");
        source_file(&target)
    }

    fn append(&self, file: &SourceFile, bytes: &str) {
        let mut handle = OpenOptions::new().append(true).open(&file.path).unwrap();
        handle.write_all(bytes.as_bytes()).unwrap();
    }

    /// Where the IDE's cache db sits inside this install, given `QODER_HOME` points
    /// at the same temp root: `<root>/SharedClientCache/cache/db/local.db`.
    fn cache_db_path(&self) -> PathBuf {
        self.root()
            .join("SharedClientCache")
            .join("cache")
            .join("db")
            .join("local.db")
    }

    /// Writes the vendor's own `chat_message` table (its DDL, column order and all)
    /// with four rows: two calls, one of them the *same* `request_id` the transcript
    /// fixture bills in credits, one `user` row like the only row this machine's real
    /// database holds, and one `token_info` cell that is not JSON.
    fn place_cache_db(&self) -> SourceFile {
        let path = self.cache_db_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(CACHE_DDL).unwrap();
        conn.execute_batch(CACHE_SESSION_DDL).unwrap();
        conn.execute(
            "INSERT INTO chat_session VALUES (?1, '/Users/demo/proj', 'proj')",
            [SESSION],
        )
        .unwrap();
        for (id, request, role, tokens, model) in CACHE_ROWS {
            conn.execute(
                "INSERT INTO chat_message (id, session_id, request_id, role, content, summary, \
                 summary_modified, summary_trigger, tool_result, token_info, model_info, extra, \
                 gmt_create) VALUES (?1,?2,?3,?4,'body','','',0,'',?5,?6,'',?7)",
                rusqlite::params![id, SESSION, request, role, tokens, model, 1_785_201_010_881i64],
            )
            .unwrap();
        }
        drop(conn);
        let meta = fs::metadata(&path).unwrap();
        SourceFile {
            path,
            kind: FileKind::Sqlite,
            size: meta.len(),
            mtime_ms: millis(meta.modified().unwrap()),
        }
    }

    fn env(&self) -> EnvRoot {
        EnvRoot::point_at(&self.root())
    }
}

/// `.schema chat_message` off this machine's
/// `~/Library/Application Support/Qoder/SharedClientCache/cache/db/local.db`.
const CACHE_DDL: &str = "CREATE TABLE chat_message (
        id varchar(64) primary key,
        session_id VARCHAR(64),
        request_id VARCHAR(64),
        role       VARCHAR(64),
        content text,
        summary text,
        summary_modified INTEGER,
        summary_trigger INTEGER DEFAULT 0,
        tool_result text,
        token_info text,
        model_info text,
        extra text DEFAULT '',
        gmt_create INTEGER
    );";

const CACHE_SESSION_DDL: &str = "CREATE TABLE chat_session (
        session_id varchar(64) primary key,
        project_uri VARCHAR(256),
        project_name VARCHAR(128)
    );";

/// `(id, request_id, role, token_info, model_info)`. `req-bbbb` is deliberately the
/// id `credits.jsonl` also carries, to prove one call in both stores is one key.
const CACHE_ROWS: [(&str, &str, &str, &str, &str); 4] = [
    (
        "ide-msg-1",
        "req-bbbb",
        "assistant",
        r#"{"prompt_tokens":58299,"cached_tokens":57853,"completion_tokens":2812,"max_input_tokens":200000}"#,
        r#"{"model_key":"qfmodel"}"#,
    ),
    (
        "ide-msg-2",
        "req-dddd",
        "assistant",
        r#"{"prompt_tokens":1000,"cached_tokens":400,"completion_tokens":20}"#,
        r#"{"model_key":"auto"}"#,
    ),
    ("ide-msg-3", "req-eee", "user", "", ""),
    (
        "ide-msg-4",
        "req-fff",
        "assistant",
        "not-json",
        r#"{"model_key":"qfmodel"}"#,
    ),
];

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

fn read(file: &SourceFile, cursor: u64) -> ReadOutcome {
    QoderAdapter
        .read(file, ReadCursor(cursor))
        .expect("fixture read")
}

fn keys(events: &[UsageEvent]) -> Vec<String> {
    events.iter().filter_map(|e| e.dedupe_key.clone()).collect()
}

/// The transcript key when a record carries no `usage.request_id`.
fn key(session: &str, id: &str) -> String {
    format!("{session}#{id}")
}

/// The key both stores use for one call: the vendor's own `request_id`.
fn rkey(request: &str) -> String {
    format!("qoder#{request}")
}

fn event<'a>(events: &'a [UsageEvent], wanted: &str) -> &'a UsageEvent {
    events
        .iter()
        .find(|e| e.dedupe_key.as_deref() == Some(wanted))
        .unwrap_or_else(|| panic!("no event for {wanted}, got {:?}", keys(events)))
}

fn credits_total(events: &[UsageEvent]) -> f64 {
    events.iter().map(|e| e.counts.credits).sum()
}

/// The headline arithmetic of this source, pinned line by line: three billable
/// messages out of twelve records, the streamed repeats of one message counted
/// once, and zero on every token stage.
#[test]
fn credits_are_the_only_meter_and_repeated_ids_merge() {
    let install = Install::new();
    let file = install.place("credits.jsonl", "sess-main.jsonl", None);
    let outcome = read(&file, 0);

    // `chatcmpl-cccc` (0 credits), the `user`/`active-leaf`/`workspace-directories`
    // plumbing, the unparsable line and the half-flushed tail are all not events.
    assert_eq!(
        keys(&outcome.events),
        [rkey("req-aaaa"), rkey("req-bbbb"), rkey("req-dddd")]
    );
    assert!(
        (credits_total(&outcome.events) - 1.597_469_642_857_142_8).abs() < 1e-12,
        "credits are {:#?}",
        credits_total(&outcome.events)
    );

    let turn = event(&outcome.events, &rkey("req-aaaa"));
    assert_eq!(turn.tool, TOOL_ID);
    assert_eq!(turn.session, SESSION, "the key is the request, the session still travels");
    assert_eq!(turn.project.as_deref(), Some("/Users/demo/proj"));
    assert_eq!(turn.model.as_deref(), Some("qfmodel"));
    assert_eq!(turn.meter, Meter::Credits);
    assert_eq!(turn.source, file.key());
    assert_eq!(turn.ts_ms, parse_ts_ms("2026-09-23T09:00:02.000Z").unwrap());
    assert!(turn.calls.is_empty());
    assert!(turn.quota.is_none());
    // Three lines carry this one message id (two with identical usage): the charge
    // is 0.2774696428571428, never twice that.
    assert_eq!(turn.counts.credits, 0.277_469_642_857_142_8);

    // Partial 0.5 then finished 0.9 under the same id: the largest snapshot wins.
    assert_eq!(
        event(&outcome.events, &rkey("req-bbbb"))
            .counts
            .credits,
        0.9
    );

    // `chatcmpl-dddd` claims 9 000 input / 400 output / 40 000 cache-read / 4 096
    // cache-write tokens. Qoder priced the turn in credits server-side, so none of
    // that may leak into the token meter, where the pricing layer would multiply it
    // by a USD rate.
    let claimer = event(&outcome.events, &rkey("req-dddd"));
    let counts = &claimer.counts;
    assert_eq!(
        (
            counts.input,
            counts.cache_creation,
            counts.cache_read,
            counts.output,
            counts.reasoning
        ),
        (0.0, 0.0, 0.0, 0.0, 0.0)
    );
    assert_eq!(counts.total(), 0.0);
    assert_eq!(counts.credits, 0.42);
    assert_eq!(claimer.model.as_deref(), Some("qmodel_38max"));
    assert!(!counts.is_zero());
    // Internal aliases stay verbatim: renaming one would attach a price never charged.
    assert!(outcome.events.iter().all(|e| e.counts.credits > 0.0));
}

#[test]
fn zero_credit_records_are_skipped_but_the_billable_flag_is_not_the_test() {
    let install = Install::new();
    let main = install.place("credits.jsonl", "sess-main.jsonl", None);
    let side = install.place(
        "subagent.jsonl",
        "agent-aExplore-0c2383.jsonl",
        Some("sub-sess/subagents"),
    );

    let events = read(&main, 0).events;
    // The interrupted turn reported `credits: 0` next to `billable: false`.
    assert!(
        !keys(&events).contains(&rkey("req-cccc")),
        "{:?}",
        keys(&events)
    );
    // `billable: false` alone is not a skip condition: this record is real billed
    // work, and 5 400 of 5 555 records on this machine are flagged the same way.
    assert_eq!(
        event(&events, &rkey("req-aaaa"))
            .counts
            .credits,
        0.277_469_642_857_142_8
    );

    let side = read(&side, 0).events;
    assert_eq!(keys(&side), [rkey("req-sa1")]);
    // The subagent record omits `cwd`: the mangled project dir labels it instead.
    assert_eq!(side[0].project.as_deref(), Some("/Users/demo/proj"));
    assert_eq!(side[0].counts.credits, 0.75);
}

/// Two records claiming one `dedupe_key` would silently drop a row in the
/// indexer's global unique index, so both halves of the key policy are pinned: the
/// vendor's `request_id` when a record carries one, and the session-scoped
/// `<session>#<message.id>` when it does not.
#[test]
fn one_message_id_in_two_sessions_still_yields_two_events() {
    let install = Install::new();
    let main = install.place("credits.jsonl", "sess-main.jsonl", None);
    let side = install.place("subagent.jsonl", "agent-aExplore-0c2383.jsonl", None);

    let mut all = read(&main, 0).events;
    all.extend(read(&side, 0).events);

    // Both files hold a billable `chatcmpl-aaaa`, under different sessions — and
    // each one carries its own `request_id`, which is what keeps them apart now.
    let distinct: HashSet<&str> = all.iter().filter_map(|e| e.dedupe_key.as_deref()).collect();
    assert_eq!(
        distinct.len(),
        all.len(),
        "dedupe keys collided: {:?}",
        keys(&all)
    );
    assert!(all.iter().any(|e| e.dedupe_key.as_deref() == Some(rkey("req-aaaa").as_str())));
    assert!(all.iter().any(|e| e.dedupe_key.as_deref() == Some(rkey("req-sa1").as_str())));
    assert!((credits_total(&all) - 2.347_469_642_857_143).abs() < 1e-12);

    // And with no `request_id` anywhere, two sessions sharing a bare
    // `chatcmpl-shared` must not fold into one event.
    let bare = |session: &str, file: &str| {
        let record = serde_json::json!({
            "type": "assistant",
            "sessionId": session,
            "timestamp": "2026-09-23T09:00:02.000Z",
            "message": {
                "id": "chatcmpl-shared",
                "model": "qfmodel",
                // No `request_id` anywhere: nothing to key on but the session.
                "usage": { "credits": 0.5, "input_tokens": 0 }
            }
        });
        let target = install.log_path(file, None);
        fs::write(&target, format!("{record}\n").as_bytes()).unwrap();
        source_file(&target)
    };
    let mut unnamed = read(&bare(SESSION, "bare-a.jsonl"), 0).events;
    unnamed.extend(read(&bare(SIDE_SESSION, "bare-b.jsonl"), 0).events);
    assert_eq!(
        keys(&unnamed),
        [
            key(SESSION, "chatcmpl-shared"),
            key(SIDE_SESSION, "chatcmpl-shared")
        ]
    );
}

#[test]
fn resume_from_the_returned_cursor_adds_nothing() {
    let install = Install::new();
    let file = install.place("resume.jsonl", "sess-r.jsonl", None);
    let first = read(&file, 0);
    assert_eq!(
        keys(&first.events),
        [rkey("req-r1"), rkey("req-r2")]
    );
    assert!((credits_total(&first.events) - 0.9).abs() < 1e-12);
    assert_eq!(first.cursor.0, file.size, "every line here is complete");

    let second = QoderAdapter.read(&file, first.cursor).unwrap();
    assert!(second.events.is_empty());
    assert_eq!(second.cursor, first.cursor);
}

#[test]
fn a_half_written_line_waits_and_the_append_delivers_it() {
    let install = Install::new();
    let file = install.place("credits.jsonl", "sess-main.jsonl", None);

    let first = read(&file, 0);
    assert_eq!(first.events.len(), 3);
    assert!(
        first.cursor.0 < file.size,
        "the partial tail must not be consumed"
    );
    let left = file.size - first.cursor.0;
    assert!(
        (100..400).contains(&left),
        "unexpected partial tail of {left} bytes"
    );

    // Qoder flushes the rest of the record; nothing else changed.
    install.append(&file, concat!(r##"xt"}],"id":"chatcmpl-late","model":"qfmodel","role":"assistant","stop_reason":"end_turn","type":"message","usage":{"billable":false,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"context_usage_ratio":0.6,"credits":7.0,"input_tokens":0,"original_credits":7.0,"output_tokens":0,"request_id":"req-late","speed":"standard"}}"##, r#","sessionId":"6f1d2c3b-9a8b-4c7d-8e9f-0a1b2c3d4e5f","timestamp":"2026-09-23T09:00:09.000Z","type":"assistant","userType":"external","uuid":"e-1","version":"1.1.57"}"#, "\n"));

    let second = QoderAdapter.read(&file, first.cursor).unwrap();
    assert_eq!(
        keys(&second.events),
        [rkey("req-late")],
        "exactly the newly completed turn"
    );
    assert_eq!(second.events[0].counts.credits, 7.0);
    assert_eq!(second.events[0].model.as_deref(), Some("qfmodel"));
    assert_eq!(second.cursor.0, fs::metadata(&file.path).unwrap().len());
    assert!(QoderAdapter
        .read(&file, second.cursor)
        .unwrap()
        .events
        .is_empty());
}

#[test]
fn discover_walks_nested_session_dirs_and_filters_by_mtime() {
    let install = Install::new();
    let _env = install.env();
    let top = install.place("credits.jsonl", "sess-main.jsonl", None);
    let nested = install.place(
        "subagent.jsonl",
        "agent-aExplore-0c2383.jsonl",
        Some(&format!("{SESSION}/subagents")),
    );
    let transcript = install.place(
        "resume.jsonl",
        "task-70c.session.execution.jsonl",
        Some("transcript"),
    );
    // Side files sit next to the logs and are not logs.
    fs::write(nested.path.with_extension("meta.json"), "{}").unwrap();
    let memory = Install::workspace(install.dir.path())
        .join(SESSION)
        .join("memory")
        .join("MEMORY.md");
    fs::create_dir_all(memory.parent().unwrap()).unwrap();
    fs::write(&memory, "# notes\n").unwrap();

    let all = QoderAdapter.discover(&DateFilter::default());
    assert_eq!(
        paths(&all),
        vec![
            nested.path.clone(),
            top.path.clone(),
            transcript.path.clone()
        ],
        "sorted by path"
    );
    assert!(all.iter().all(|f| f.kind == FileKind::Jsonl));
    assert!(all.iter().all(|f| f.size > 0 && f.mtime_ms > 0));

    let twenty_twenty = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_000_000);
    let handle = OpenOptions::new().write(true).open(&top.path).unwrap();
    handle.set_modified(twenty_twenty).unwrap();
    drop(handle);

    let since = SystemTime::now() - Duration::from_secs(86_400);
    let recent = QoderAdapter.discover(&DateFilter::new(Some(millis(since)), None));
    assert_eq!(
        paths(&recent),
        vec![nested.path.clone(), transcript.path.clone()],
        "the 2020 file must be skipped"
    );
}

#[test]
fn probe_reports_the_source_without_reading_every_file() {
    let install = Install::new();
    let _env = install.env();
    let adapter = QoderAdapter;
    assert!(
        adapter.probe().is_none(),
        "an empty projects tree is not an install"
    );

    install.place(
        "resume.jsonl",
        "sess-r.jsonl",
        Some("9c9e8d7c-1111-4222-8333-444444444444/subagents"),
    );
    let detected = adapter.probe().expect("one nested log is enough");
    assert_eq!(detected.id, TOOL_ID);
    assert_eq!(detected.display, "Qoder");
    assert_eq!(detected.roots, vec![install.projects()]);
    // The hint names both stores, and every real record stamps the CLI version
    // that wrote the transcript one.
    assert_eq!(detected.roots, vec![install.projects()], "no cache db in this root");
    assert_eq!(detected.hint.as_deref(), Some("1 logs v1.1.57"));
}

#[test]
fn unreadable_logs_are_reported_not_raised() {
    let install = Install::new();
    let missing = absent_source_file(&install.log_path("gone.jsonl", None));
    let outcome = QoderAdapter.read(&missing, ReadCursor(0)).unwrap();
    assert_eq!((outcome.events.len(), outcome.cursor), (0, ReadCursor(0)));

    // A directory opens fine but can never be read as a log.
    let dir = source_file(&install.projects());
    let outcome = QoderAdapter.read(&dir, ReadCursor(0)).unwrap();
    assert_eq!((outcome.events.len(), outcome.cursor), (0, ReadCursor(0)));
}

#[test]
fn a_truncated_log_rejects_a_cursor_past_its_end() {
    let install = Install::new();
    let file = install.place("credits.jsonl", "sess-main.jsonl", None);
    let outcome = read(&file, 0);
    assert_eq!(outcome.events.len(), 3);

    // Log rotation replaces a 5 KB transcript with a fresh header.
    fs::write(&file.path, b"short\n").unwrap();
    let err = QoderAdapter.read(&file, outcome.cursor).unwrap_err();
    assert!(
        matches!(err, Error::Cursor { cursor, .. } if cursor == outcome.cursor.0),
        "got {err:?}"
    );
}

#[test]
fn semantics_declare_a_credit_meter() {
    // Spelled out field by field, because `usage-index` and the money math key
    // their behaviour off these: `Meter::Credits` is what keeps `summarize` out of
    // the dollar path and off the unpriced-models list.
    assert_eq!(
        QoderAdapter.semantics(),
        Semantics {
            usage_form: UsageForm::PerCall,
            meter: Meter::Credits,
            model_attr: ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    );
    assert_eq!(QoderAdapter.id(), TOOL_ID);
    assert_eq!(QoderAdapter.display_name(), "Qoder");
}

fn paths(files: &[SourceFile]) -> Vec<PathBuf> {
    files.iter().map(|f| f.path.clone()).collect()
}

/// A census over the raw lines, so the smoke test can say what was skipped and why
/// instead of only how much survived.
#[derive(Default, Debug)]
struct Census {
    lines: usize,
    partial_tails: usize,
    unparsable: usize,
    non_assistant: usize,
    streamed_blocks: usize,
    zero_credit: usize,
    billable_false: usize,
    nonzero_tokens: usize,
    usage_records: usize,
    raw_credits: f64,
}

impl Census {
    fn tally(&mut self, path: &Path) {
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            let trimmed = line.trim_end_matches('\r');
            if trimmed.is_empty() {
                continue;
            }
            self.lines += 1;
            if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
                self.partial_tails += 1;
                continue;
            }
            let Ok(record) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                self.unparsable += 1;
                continue;
            };
            let message =
                if record.get("type").and_then(serde_json::Value::as_str) == Some("assistant") {
                    record.get("message")
                } else {
                    self.non_assistant += 1;
                    continue;
                };
            let usage = message
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.as_object());
            let Some(usage) = usage else {
                // A streamed content block: repeats the message id, reports no meter.
                self.streamed_blocks += 1;
                continue;
            };
            self.usage_records += 1;
            let credits = usage
                .get("credits")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            self.raw_credits += credits;
            if credits == 0.0 {
                self.zero_credit += 1;
            }
            if usage.get("billable").and_then(serde_json::Value::as_bool) == Some(false) {
                self.billable_false += 1;
            }
            let has_tokens = [
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ]
            .iter()
            .any(|k| {
                usage
                    .get(*k)
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0)
                    != 0.0
            });
            if has_tokens {
                self.nonzero_tokens += 1;
            }
        }
    }
}

#[test]
#[ignore = "reads the real ~/.qoder/projects on this machine"]
fn real_machine_logs_meter_everything_in_credits() {
    let _env = EnvRoot::cleared();
    let adapter = QoderAdapter;
    let Some(detected) = adapter.probe() else {
        eprintln!("no ~/.qoder/projects: nothing to smoke test");
        return;
    };
    let files = adapter.discover(&DateFilter::default());
    // This smoke test is about the transcript store; the cache db has its own.
    let logs = files.iter().filter(|f| f.kind == FileKind::Jsonl).count();
    let mut events = Vec::new();
    let mut census = Census::default();
    let mut bytes = 0u64;
    let mut seen: HashSet<String> = HashSet::new();
    let mut keyed_by_request = 0;

    for file in files.iter().filter(|f| f.kind == FileKind::Jsonl) {
        let outcome = adapter
            .read(file, ReadCursor(0))
            .expect("real files must not error");
        bytes += outcome.cursor.0;
        census.tally(&file.path);
        for event in outcome.events {
            // Two records claiming one dedupe key would silently drop a row in the
            // indexer's global unique index, so fail loudly instead.
            assert!(
                seen.insert(event.dedupe_key.clone().unwrap()),
                "colliding dedupe_key {:?}",
                event.dedupe_key
            );
            assert_eq!(event.meter, Meter::Credits);
            assert_eq!(
                event.counts.total(),
                0.0,
                "a token number leaked for {:?}",
                event.dedupe_key
            );
            // Every real record carries `usage.request_id`, which is the key both
            // stores use: count them, because a source that stopped writing one
            // would fall back to `<session>#<message.id>` and stop folding with the
            // IDE cache db silently.
            keyed_by_request += usize::from(
                event.dedupe_key.as_deref().is_some_and(|k| k.starts_with("qoder#")),
            );
            events.push(event);
        }
    }

    let models: std::collections::BTreeMap<String, (usize, f64)> =
        events
            .iter()
            .fold(std::collections::BTreeMap::new(), |mut acc, e| {
                let slot = acc
                    .entry(e.model.clone().unwrap_or_else(|| "?".into()))
                    .or_insert((0, 0.0));
                slot.0 += 1;
                slot.1 += e.counts.credits;
                acc
            });
    let sessions: HashSet<&str> = events.iter().map(|e| e.session.as_str()).collect();
    let credits = credits_total(&events);
    eprintln!(
        "roots {:?} hint {:?}\n{} files, {bytes} bytes, {} raw lines\n{census:?}",
        detected.roots,
        detected.hint,
        logs,
        census.lines,
    );
    eprintln!(
        "{credits:.4} credits in {} events ({} usage records merged, {} zero-credit and {} streamed-block records skipped); raw sum {:.4}",
        events.len(),
        census.usage_records,
        census.zero_credit,
        census.streamed_blocks,
        census.raw_credits,
    );
    eprintln!(
        "{} sessions, {} models, {} billable:false, {} with nonzero token fields",
        sessions.len(),
        models.len(),
        census.billable_false,
        census.nonzero_tokens,
    );
    for (model, (count, credits)) in &models {
        eprintln!("  model {model}: {count} events, {credits:.4} credits");
    }

    assert!(events.len() > 4_000, "only {} events", events.len());
    assert!(credits > 900.0, "credits are {credits}");
    assert!(models.len() >= 2, "expected several models, got {models:?}");
    // Every real record prices in credits and reports no tokens at all.
    assert_eq!(
        census.nonzero_tokens, 0,
        "the source started writing token counts"
    );
    // Raw minus the zero-credit records: also true only while no message id
    // repeats *with* usage, which is what the merge exists to defend against.
    assert_eq!(events.len(), census.usage_records - census.zero_credit);
    assert_eq!(
        keyed_by_request,
        events.len(),
        "every real usage record carries a request_id; the rest would not fold"
    );
    assert!(
        (credits - census.raw_credits).abs() < 1e-6,
        "credits drifted from the raw sum"
    );
}

/// The two stores side by side: one tool id, one `SourceFile` each, two meters, and
/// the calls that exist in both landing on one key.
#[test]
fn one_tool_id_lists_both_stores_and_folds_the_overlap() {
    let install = Install::new();
    let _env = install.env();
    let jsonl = install.place("credits.jsonl", "sess-main.jsonl", None);
    let db = install.place_cache_db();

    let files = QoderAdapter.discover(&DateFilter::default());
    assert_eq!(
        paths(&files),
        vec![jsonl.path.clone(), db.path.clone()],
        "both stores, transcripts first"
    );
    assert_eq!(files[0].kind, FileKind::Jsonl);
    assert_eq!(files[1].kind, FileKind::Sqlite);

    let credits = read(&files[0], 0);
    let tokens = read(&files[1], 0);
    assert_eq!(
        keys(&credits.events),
        [rkey("req-aaaa"), rkey("req-bbbb"), rkey("req-dddd")]
    );
    assert_eq!(
        keys(&tokens.events),
        [rkey("req-bbbb"), rkey("req-dddd")],
        "the `not-json` cell and the `user` row produced nothing"
    );
    assert!(credits
        .events
        .iter()
        .all(|e| e.meter == Meter::Credits && e.counts.total() == 0.0));
    assert!(tokens
        .events
        .iter()
        .all(|e| e.meter == Meter::Tokens && e.counts.credits == 0.0));
    let (input, cache_read, output) = tokens.events.iter().fold(
        (0.0, 0.0, 0.0),
        |mut acc, e| {
            acc.0 += e.counts.input;
            acc.1 += e.counts.cache_read;
            acc.2 += e.counts.output;
            acc
        },
    );
    assert_eq!(
        (input, cache_read, output),
        (1_046.0, 58_253.0, 2_832.0),
        "prompt minus cached, never prompt plus cached"
    );
    assert_eq!(tokens.cursor, ReadCursor(4), "four rows, highest rowid");

    // The overlap: five events, three distinct keys, so the indexer's unique
    // `event_dedupe` index folds the two duplicated calls down to one row each.
    let mut all = credits.events;
    all.extend(tokens.events);
    let distinct: HashSet<&str> = all.iter().filter_map(|e| e.dedupe_key.as_deref()).collect();
    assert_eq!(distinct.len(), 3, "{:?}", keys(&all));
    assert_eq!(all.len(), 5);
    assert!(all.iter().any(|e| e.meter == Meter::Credits && e.dedupe_key.as_deref() == Some(rkey("req-bbbb").as_str())), "the credits copy of the call is the one that stays in the credit meter");

    // `probe` names both stores and quotes the counts it found in each.
    let detected = QoderAdapter.probe().expect("both stores are installed here");
    assert_eq!(detected.id, TOOL_ID);
    assert_eq!(
        detected.roots,
        vec![install.projects(), install.cache_db_path()]
    );
    assert_eq!(
        detected.hint.as_deref(),
        Some("1 logs v1.1.57 · cache db 4 rows, 3 token messages, 4 requests")
    );
    // A `since` in the future hides both: the cache db is bounded by its own mtime,
    // exactly like the transcript walk.
    let future = SystemTime::now() + Duration::from_secs(86_400);
    assert!(
        QoderAdapter
            .discover(&DateFilter::new(Some(millis(future)), None))
            .is_empty()
    );
}

/// The real IDE database, asked twice: once through this adapter and once through a
/// `SELECT` plus the test's own arithmetic over the same rows.
#[test]
#[ignore = "reads the real Qoder IDE cache db"]
fn the_live_cache_db_agrees_with_its_own_sql() {
    let _env = EnvRoot::cleared();
    let adapter = QoderAdapter;
    let files = adapter.discover(&DateFilter::default());
    let Some(db) = files.iter().find(|f| f.kind == FileKind::Sqlite) else {
        eprintln!("[qoder] no IDE cache db on this machine: nothing to smoke test");
        return;
    };
    let conn = rusqlite::Connection::open_with_flags(
        &db.path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("the live db opens read-only");
    let total: i64 = conn
        .query_row("SELECT count(*) FROM chat_message", [], |r| r.get(0))
        .unwrap();
    let max_rowid: i64 = conn
        .query_row("SELECT COALESCE(max(rowid), 0) FROM chat_message", [], |r| r.get(0))
        .unwrap();
    // Row by row, because `json_extract` aborts the whole query on one bad cell.
    let cells: Vec<String> = conn
        .prepare(
            "SELECT token_info FROM chat_message \
             WHERE role = 'assistant' AND token_info IS NOT NULL ORDER BY rowid",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, Option<String>>(0).map(|v| v.unwrap_or_default()))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    let mut valid = 0;
    let mut nonzero = 0;
    let mut want = usage_core::TokenCounts::default();
    for cell in &cells {
        let Ok(value) = serde_json::from_str::<Value>(cell) else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        valid += 1;
        let field = |k: &str| value.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        let (prompt, completion) = (field("prompt_tokens"), field("completion_tokens"));
        let cached = field("cached_tokens").min(prompt);
        if prompt + completion == 0.0 {
            continue;
        }
        nonzero += 1;
        want.input += (prompt - cached).max(0.0);
        want.cache_read += cached;
        want.output += completion;
    }

    let outcome = adapter.read(db, ReadCursor(0)).expect("a live db must not error");
    let mut got = usage_core::TokenCounts::default();
    let mut keys: HashSet<String> = HashSet::new();
    for event in &outcome.events {
        assert_eq!(event.tool, TOOL_ID, "still the one tool id");
        assert_eq!(event.meter, Meter::Tokens, "this store is token-metered");
        assert_eq!(event.counts.credits, 0.0);
        assert!(
            keys.insert(event.dedupe_key.clone().unwrap()),
            "colliding dedupe_key {:?}",
            event.dedupe_key
        );
        assert!(event.ts_ms > 1_700_000_000_000, "a real timestamp: {event:?}");
        got += &event.counts;
    }
    eprintln!(
        "[qoder] cache db {:?} · {} bytes · rows={total} rowids≤{max_rowid} · assistant token_info cells={} · valid JSON={} · nonzero tokens={}",
        db.path,
        db.size,
        cells.len(),
        valid,
        nonzero
    );
    eprintln!(
        "[qoder] {} events · input={:.0} cacheRead={:.0} output={:.0} total={:.0} · models {:?}",
        outcome.events.len(),
        got.input,
        got.cache_read,
        got.output,
        got.total(),
        outcome
            .events
            .iter()
            .map(|e| e.model.clone().unwrap_or_else(|| "?".into()))
            .collect::<std::collections::BTreeSet<_>>()
    );
    assert_eq!(outcome.cursor, ReadCursor(max_rowid as u64), "cursor is the highest rowid");
    assert_eq!(outcome.events.len(), nonzero, "one event per token-bearing row");
    assert_eq!(got.input, want.input, "Σ input must equal the SELECT recompute");
    assert_eq!(got.cache_read, want.cache_read, "Σ cache_read");
    assert_eq!(got.output, want.output, "Σ output");
    assert_eq!(got.credits, 0.0);
    // Reading it again adds nothing.
    let again = adapter.read(db, outcome.cursor).unwrap();
    assert!(again.events.is_empty() && again.cursor == outcome.cursor, "{:?}", again.cursor);
}

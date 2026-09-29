//! Behaviour tests for the Antigravity adapter, against fixture databases built
//! from real (privacy-trimmed, UUID-scrubbed) `gen_metadata` blobs.
//!
//! Nothing here reads `~/.gemini`: `ANTIGRAVITY_DATA_DIR` points at a temp
//! install, and the bytes come from `tests/fixtures/`, whose `expected.tsv` was
//! measured by `build.py` — an independent decoder, so these assertions are not
//! this crate checking itself.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;
use usage_adapter_antigravity::{AntigravityAdapter, TOOL_ID};
use usage_core::{DateFilter, FileKind, Meter, ReadCursor, SourceAdapter, SourceFile};

/// `ANTIGRAVITY_DATA_DIR` is process-global, so whoever flips it holds one lock.
/// A guard must therefore be taken once per test and never from a helper: the
/// lock is not reentrant, and a nested `point_at` would deadlock the suite.
static ENV: Mutex<()> = Mutex::new(());

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn point_at(root: &Path) -> Self {
        let _lock = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("ANTIGRAVITY_DATA_DIR");
        std::env::set_var("ANTIGRAVITY_DATA_DIR", root);
        EnvGuard { _lock, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("ANTIGRAVITY_DATA_DIR", value),
            None => std::env::remove_var("ANTIGRAVITY_DATA_DIR"),
        }
    }
}

const SESSION: &str = "11111111-2222-3333-4444-555555555555";
/// Deliberately far from every turn in the fixtures: if a timestamp ever falls
/// back to the conversation's created-at, a test can tell that it did.
const CREATED_MS: i64 = 1_577_836_800_000;

/// One row of `expected.tsv`, i.e. what the second decoder measured on the real
/// database this fixture was trimmed from.
#[derive(Debug, Clone)]
struct Expected {
    idx: i64,
    prefix: Option<i64>,
    input: Option<i64>,
    total_output: Option<i64>,
    cache_read: Option<i64>,
    out_a: Option<i64>,
    out_b: Option<i64>,
    response_id: String,
    ts_ms: Option<i64>,
}

impl Expected {
    /// What the adapter is expected to emit for this row, in our own stages.
    fn counts(&self) -> (f64, f64, f64, f64) {
        let input = self.input.unwrap_or(0) as f64;
        let cache_read = self.cache_read.unwrap_or(0) as f64;
        let output = self.total_output.unwrap_or(0) as f64;
        (input, cache_read, output, self.out_b.unwrap_or(0) as f64)
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

fn read_lines(name: &str) -> Vec<String> {
    let text = std::fs::read_to_string(fixtures().join(name))
        .unwrap_or_else(|err| panic!("fixture {name} is committed: {err}"));
    text.lines().map(str::to_string).collect()
}

fn expected_rows() -> Vec<Expected> {
    read_lines("expected.tsv")
        .into_iter()
        .map(|line| {
            let mut cols = line.split('\t').map(|cell| cell.to_string());
            let num = |cell: &str| (!cell.is_empty()).then(|| cell.parse::<i64>().expect("numeric column"));
            let idx = cols.next().unwrap_or_default();
            let _source = cols.next().unwrap_or_default();
            let _orig = cols.next().unwrap_or_default();
            Expected {
                idx: idx.parse().expect("fixture index"),
                prefix: num(&cols.next().unwrap_or_default()),
                input: num(&cols.next().unwrap_or_default()),
                total_output: num(&cols.next().unwrap_or_default()),
                cache_read: num(&cols.next().unwrap_or_default()),
                out_a: num(&cols.next().unwrap_or_default()),
                out_b: num(&cols.next().unwrap_or_default()),
                response_id: cols.next().unwrap_or_default(),
                ts_ms: num(&cols.next().unwrap_or_default()),
            }
        })
        .collect()
}

fn gen_blobs() -> Vec<Vec<u8>> {
    read_lines("gen_metadata.hex")
        .into_iter()
        .map(|line| hex(&line))
        .collect()
}

fn step_blobs() -> Vec<Option<Vec<u8>>> {
    read_lines("steps_metadata.hex")
        .into_iter()
        .map(|line| (!line.trim().is_empty()).then(|| hex(&line)))
        .collect()
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex fixture"))
        .collect()
}

/// A tiny independent encoder, so a test can build a second streamed partial of
/// a real response without borrowing this crate's own writer.
fn ev(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out.push(if value == 0 { byte } else { byte | 0x80 });
        if value == 0 {
            return out;
        }
    }
}

fn tag_varint(field: u64, value: u64) -> Vec<u8> {
    let mut out = ev(field << 3);
    out.extend(ev(value));
    out
}

fn tag_bytes(field: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = ev((field << 3) | 2);
    out.extend(ev(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

/// `trajectory_metadata_blob.data` in the real shape — `#1` the workspace folder,
/// `#2` the created-at — with synthetic values.
fn conversation_blob() -> Vec<u8> {
    let mut blob = tag_bytes(1, &tag_bytes(1, b"file:///Users/demo/antigravity-fixture"));
    let mut created = tag_varint(1, (CREATED_MS / 1000) as u64);
    created.extend(tag_varint(2, 0));
    blob.extend(tag_bytes(2, &created));
    blob
}

/// A database in the shape the CLI writes, holding the given generations.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("conversations")).expect("conversations dir");
        Fixture { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn conversations(&self) -> PathBuf {
        self.path().join("conversations")
    }

    fn db_path(&self, session: &str) -> PathBuf {
        self.conversations().join(format!("{session}.db"))
    }

    /// Writes one conversation database with the real DDL.
    fn build(&self, session: &str, gens: &[(i64, &[u8])], steps: &[&[u8]]) -> PathBuf {
        let path = self.db_path(session);
        let conn = Connection::open(&path).expect("create fixture db");
        conn.execute_batch(
            "CREATE TABLE `trajectory_meta` (`trajectory_id` text, `cascade_id` text, `trajectory_type` integer, `source` integer, PRIMARY KEY (`trajectory_id`));
             CREATE TABLE `steps` (`idx` integer, `step_type` integer NOT NULL DEFAULT 0, `status` integer NOT NULL DEFAULT 0, `has_subtrajectory` numeric NOT NULL DEFAULT false, `metadata` blob, `error_details` blob, `permissions` blob, `task_details` blob, `render_info` blob, `step_payload` blob, `step_format` integer NOT NULL DEFAULT 0, PRIMARY KEY (`idx`));
             CREATE TABLE `gen_metadata` (`idx` integer, `data` blob, `size` integer NOT NULL DEFAULT 0, PRIMARY KEY (`idx`));
             CREATE TABLE `trajectory_metadata_blob` (`id` text DEFAULT \"main\", `data` blob, PRIMARY KEY (`id`));",
        )
        .expect("real schema");
        for (idx, blob) in gens {
            conn.execute(
                "INSERT INTO gen_metadata (idx, data, size) VALUES (?1, ?2, ?3)",
                rusqlite::params![idx, blob, blob.len() as i64],
            )
            .expect("insert gen");
        }
        for (n, metadata) in steps.iter().enumerate() {
            conn.execute(
                "INSERT INTO steps (idx, step_type, metadata) VALUES (?1, 15, ?2)",
                rusqlite::params![n as i64, metadata],
            )
            .expect("insert step");
        }
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            rusqlite::params![conversation_blob()],
        )
        .expect("insert conversation blob");
        conn.execute("INSERT INTO trajectory_meta VALUES ('t1', ?1, 4, 1)", rusqlite::params![session])
            .expect("insert trajectory_meta");
        drop(conn);
        path
    }

    /// Appends one generation to an existing fixture database, as the CLI does
    /// turn by turn.
    fn append_gen(&self, path: &Path, idx: i64, blob: &[u8]) {
        let conn = Connection::open(path).expect("reopen fixture");
        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (?1, ?2, ?3)",
            rusqlite::params![idx, blob, blob.len() as i64],
        )
        .expect("append gen");
    }
}

fn source_files() -> Vec<SourceFile> {
    AntigravityAdapter.discover(&DateFilter::default())
}

fn keys(files: &[SourceFile]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for file in files {
        let outcome = AntigravityAdapter.read(file, ReadCursor(0)).expect("read is infallible");
        for event in &outcome.events {
            out.insert(event.dedupe_key.clone().expect("every event carries a key"));
        }
    }
    out
}

#[test]
fn probe_stops_at_the_first_readable_database() {
    let fix = Fixture::new();
    let gens: Vec<(i64, Vec<u8>)> = gen_blobs().into_iter().enumerate().map(|(i, b)| (i as i64, b)).collect();
    let refs: Vec<(i64, &[u8])> = gens.iter().map(|(i, b)| (*i, b.as_slice())).collect();
    fix.build(SESSION, &refs, &[]);
    let _env = EnvGuard::point_at(fix.path());

    let det = AntigravityAdapter.probe().expect("a fixture install is detected");
    assert_eq!(det.id, TOOL_ID);
    assert_eq!(det.display, "Antigravity");
    assert_eq!(det.roots, vec![fix.path().to_path_buf()], "the env root is what gets reported");
    assert_eq!(det.hint.as_deref(), Some("8 generations"), "count(*), not max(rowid)");
    drop(_env);

    // An install with nothing readable in it is not a source at all. (A second
    // guard would deadlock the suite's one lock, so the first is dropped first.)
    let empty = Fixture::new();
    std::fs::write(empty.db_path("junk"), b"not a database").unwrap();
    let _empty_env = EnvGuard::point_at(empty.path());
    assert!(AntigravityAdapter.probe().is_none(), "no decodable database, no source");


}

#[test]
fn discover_lists_one_entry_per_conversation_and_skips_sidecars() {
    let fix = Fixture::new();
    let blob = gen_blobs().remove(1);
    let first = fix.build(SESSION, &[(1, blob.as_slice())], &[]);
    let second = fix.build("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE", &[(0, blob.as_slice())], &[]);
    std::fs::write(fix.conversations().join(format!("{SESSION}.db-wal")), b"wal").unwrap();
    std::fs::write(fix.conversations().join(format!("{SESSION}.db-shm")), b"shm").unwrap();
    std::fs::write(fix.conversations().join("conversation_summaries.db"), b"sqlite?").unwrap();
    // (the file the CLI's own summary index is named for is excluded by name)

    let _env = EnvGuard::point_at(fix.path());
    let files = source_files();
    let listed: BTreeSet<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
    assert_eq!(
        listed,
        BTreeSet::from([first.clone(), second]),
        "the WAL sidecars and the summaries database stay out of the listing"
    );
    for file in &files {
        assert_eq!(file.kind, FileKind::Tree, "one conversation per file, so the file is the unit");
        assert!(file.size > 0);
        assert!(file.mtime_ms > CREATED_MS, "mtime is epoch milliseconds, got {}", file.mtime_ms);
        assert_eq!(std::fs::metadata(&file.path).map(|m| m.len()).unwrap(), file.size, "one stat, no re-read");
    }

    // A retention window prunes by the file's last write, without opening it.
    let written = files[0].mtime_ms;
    assert_eq!(
        AntigravityAdapter.discover(&DateFilter::new(Some(written), None)).len(),
        2,
        "a window opening at the last write still keeps both conversations"
    );
    assert_eq!(
        AntigravityAdapter.discover(&DateFilter::new(Some(written + 86_400_000), None)).len(),
        0,
        "a conversation that stopped before the window is not opened at all"
    );
}

#[test]
fn real_fixture_rows_map_to_the_independently_measured_numbers() {
    let fix = Fixture::new();
    let gens = gen_blobs();
    let steps = step_blobs();
    let refs: Vec<(i64, &[u8])> = gens.iter().enumerate().map(|(i, b)| (i as i64, b.as_slice())).collect();
    let step_refs: Vec<&[u8]> = steps.iter().filter_map(Option::as_deref).collect();
    let path = fix.build(SESSION, &refs, &step_refs);
    let _env = EnvGuard::point_at(fix.path());

    let files = AntigravityAdapter.discover(&DateFilter::default());
    let outcome = AntigravityAdapter.read(&files[0], ReadCursor(0)).unwrap();
    let by_key: HashMap<String, usage_core::UsageEvent> =
        outcome.events.iter().map(|e| (e.dedupe_key.clone().unwrap(), e.clone())).collect();

    // Rows 6 and 7 carry an empty usage message: real rows, nothing to bill.
    let want_rows = expected_rows();
    let billable: Vec<&Expected> = want_rows.iter().filter(|r| r.total_output.unwrap_or(0) > 0).collect();
    assert_eq!(billable.len(), 6);
    assert_eq!(outcome.events.len(), billable.len(), "and no other row invented an event");

    for want in &billable {
        let key = format!("{SESSION}#{}", want.response_id);
        let event = by_key.get(&key).unwrap_or_else(|| panic!("missing {key} in {keys:?}", keys = by_key.keys()));
        let (input, cache_read, output, reasoning) = want.counts();
        assert_eq!(event.counts.input, input, "input is #4.2 for row {}", want.idx);
        assert_eq!(event.counts.cache_read, cache_read, "cache read is #4.5 for row {}", want.idx);
        assert_eq!(event.counts.output, output, "output is #4.3 for row {}", want.idx);
        assert_eq!(event.counts.reasoning, reasoning, "#4.10, reported not added");
        assert_eq!(event.counts.cache_creation, 0.0, "the 1-of-5 #4.4 claim stays unbilled");
        assert_ne!(
            event.counts.input,
            want.prefix.unwrap_or(0) as f64,
            "#4.1 is a fixed prefix, never the billed input"
        );
        assert_eq!(event.model.as_deref(), Some("gemini-3.6-flash"), "the model comes from #1.19");
        assert_eq!(event.counts.credits, 0.0, "no source-reported cost leaks through");
        assert_eq!(event.meter, Meter::Tokens);
        assert_eq!(event.tool, TOOL_ID);
        assert_eq!(event.session, SESSION);
        assert_eq!(event.project.as_deref(), Some("/Users/demo/antigravity-fixture"));
        assert_eq!(event.source, files[0].key());
        // The turn is dated by its own wall clock, not by the conversation's
        // created-at (which this fixture keeps in 2020 precisely to catch that).
        let want_ts = want.ts_ms.expect("fixture row is dated");
        assert!(
            event.ts_ms.abs_diff(want_ts) < 5_000,
            "row {} dated {} instead of {}",
            want.idx,
            event.ts_ms,
            want_ts
        );
        assert_ne!(event.ts_ms, CREATED_MS);
        // The invariant the whole mapping rests on, per row.
        assert_eq!(want.total_output, Some(want.out_a.unwrap_or(0) + want.out_b.unwrap_or(0)));
    }
    assert_eq!(path.file_name().unwrap().to_string_lossy(), format!("{SESSION}.db"));
}

#[test]
fn totals_survive_the_merge_and_no_row_is_silently_dropped() {
    let fix = Fixture::new();
    let gens = gen_blobs();
    let refs: Vec<(i64, &[u8])> = gens.iter().enumerate().map(|(i, b)| (i as i64, b.as_slice())).collect();
    fix.build(SESSION, &refs, &[]);
    let _env = EnvGuard::point_at(fix.path());

    let files = AntigravityAdapter.discover(&DateFilter::default());
    let audit = AntigravityAdapter.audit(&files);
    assert_eq!(audit.rows, refs.len(), "every fixture row was looked at");
    assert_eq!(audit.split_mismatches, 0, "#4.3 == #4.9 + #4.10 on every row");
    assert_eq!(audit.merged, 0, "one responseId per row in this fixture");
    assert_eq!(audit.skipped.len(), 2, "the two empty-usage rows, and nothing else");
    assert!(audit.skipped.iter().all(|s| s.reason == "no-usage"), "{:?}", audit.skipped);
    // Σ(input + cache_read + output) over the events equals the raw wire sum,
    // because nothing merged: the identity the smoke test prints.
    assert_eq!(audit.raw.total_output, audit.raw.out_a + audit.raw.out_b);
    assert_eq!(audit.raw_grand_total(), audit.event_grand_total());
    assert!(audit.raw_grand_total() > 0.0, "the fixtures do carry tokens: {:?}", audit.raw);
    assert_eq!(audit.models, BTreeSet::from(["gemini-3.6-flash".to_string()]));
    assert_eq!(audit.unmodelled, 0);
}

#[test]
fn rereading_a_changed_database_is_idempotent_and_a_new_row_adds_exactly_one() {
    let fix = Fixture::new();
    let gens = gen_blobs();
    let first: Vec<(i64, &[u8])> = gens[..4].iter().enumerate().map(|(i, b)| (i as i64, b.as_slice())).collect();
    let path = fix.build(SESSION, &first, &[]);
    let _env = EnvGuard::point_at(fix.path());

    let before = keys(&AntigravityAdapter.discover(&DateFilter::default()));
    assert_eq!(before.len(), 4, "rows 0..4 all carry usage");
    assert_eq!(before, keys(&AntigravityAdapter.discover(&DateFilter::default())), "a re-read is a no-op");

    // The CLI appends the next turn; the file is re-read wholesale and only the
    // new responseId becomes a new event.
    fix.append_gen(&path, 4, &gens[4]);
    touch(&path);
    let after = keys(&AntigravityAdapter.discover(&DateFilter::default()));
    let added: Vec<&String> = after.difference(&before).collect();
    assert_eq!(added.len(), 1, "one more row, one more event: {added:?}");
    assert!(before.is_subset(&after), "no earlier turn disappeared");
    assert_eq!(after.len(), 5);
}

#[test]
fn streamed_partials_of_one_response_merge_by_field_max_not_sum() {
    let fix = Fixture::new();
    let real = gen_blobs().remove(1);
    let want = expected_rows()[1].clone();
    // A second row for the *same* responseId, reporting a smaller snapshot of the
    // same call — what a stream that flushed twice looks like.
    let partial = {
        let mut usage = tag_varint(1, 1071);
        usage.extend(tag_varint(2, 10));
        usage.extend(tag_varint(3, 4));
        usage.extend(tag_varint(9, 3));
        usage.extend(tag_varint(10, 1));
        usage.extend(tag_bytes(11, want.response_id.as_bytes()));
        let mut cm = tag_bytes(4, &usage);
        cm.extend(tag_bytes(19, b"gemini-3.6-flash"));
        tag_bytes(1, &cm)
    };
    fix.build(SESSION, &[(0, &partial), (1, &real), (2, &partial)], &[]);
    let _env = EnvGuard::point_at(fix.path());

    let files = AntigravityAdapter.discover(&DateFilter::default());
    let outcome = AntigravityAdapter.read(&files[0], ReadCursor(0)).unwrap();
    let audit = AntigravityAdapter.audit(&files);
    assert_eq!(outcome.events.len(), 1, "three rows, one call");
    assert_eq!(audit.merged, 2, "both partials folded into the real snapshot");
    let event = &outcome.events[0];
    let (input, cache_read, output, _) = want.counts();
    assert_eq!(event.dedupe_key.as_deref(), Some(&format!("{SESSION}#{}", want.response_id)[..]));
    assert_eq!(event.counts.input, input, "the largest snapshot wins, not the sum");
    assert_eq!(event.counts.cache_read, cache_read);
    assert_eq!(event.counts.output, output);
    assert_ne!(event.counts.input, input + 20.0, "summing partials would double this turn");
}

#[test]
fn a_row_without_a_response_id_gets_a_stable_key_instead_of_a_new_one_each_pass() {
    let fix = Fixture::new();
    let mut usage = tag_varint(2, 500);
    usage.extend(tag_varint(3, 20));
    let mut cm = tag_bytes(4, &usage);
    cm.extend(tag_bytes(19, b"gemini-3.6-flash"));
    let blob = tag_bytes(1, &cm);
    fix.build(SESSION, &[(7, &blob)], &[]);
    let _env = EnvGuard::point_at(fix.path());

    let files = AntigravityAdapter.discover(&DateFilter::default());
    let once = AntigravityAdapter.read(&files[0], ReadCursor(0)).unwrap();
    let twice = AntigravityAdapter.read(&files[0], ReadCursor(0)).unwrap();
    assert_eq!(once.events.len(), 1);
    assert_eq!(
        once.events[0].dedupe_key.as_deref(),
        Some(&format!("{SESSION}#gen7")[..]),
        "idx is the integer primary key, so it repeats exactly"
    );
    assert_eq!(once.events[0].dedupe_key, twice.events[0].dedupe_key);
    assert_eq!(once.events[0].model.as_deref(), Some("gemini-3.6-flash"));
    assert_eq!(once.events[0].ts_ms, CREATED_MS, "with no stamp anywhere, the conversation dates it");
}

#[test]
fn a_missing_gen_metadata_column_or_blob_reads_as_nothing() {
    let fix = Fixture::new();
    let path = fix.build(SESSION, &[(0, &[0x08, 0x01])], &[]);
    {
        // A NULL blob and a row whose data is pure garbage: both are tolerated.
        let conn = Connection::open(&path).unwrap();
        conn.execute("INSERT INTO gen_metadata VALUES (1, NULL, 0)", []).unwrap();
        conn.execute("INSERT INTO gen_metadata VALUES (2, x'ffffffffffffffff', 8)", []).unwrap();
        conn.execute("INSERT INTO gen_metadata VALUES (3, x'0a03414243', 3)", []).unwrap();
        conn.execute("INSERT INTO gen_metadata VALUES (4, ?1, 9)", rusqlite::params![gen_blobs().remove(6)]).unwrap();
    }
    let _env = EnvGuard::point_at(fix.path());

    let files = AntigravityAdapter.discover(&DateFilter::default());
    let outcome = AntigravityAdapter.read(&files[0], ReadCursor(0)).unwrap();
    assert!(outcome.events.is_empty(), "nothing here is billable");
    assert_eq!(outcome.cursor, ReadCursor(0), "a Tree file never advances a cursor");
    let audit = AntigravityAdapter.audit(&files);
    assert_eq!(audit.rows, 4, "a NULL blob is still a row, it just carries no generation");
    assert_eq!(audit.unusable, 0);
    assert_eq!(audit.decoded, 0);
    let reasons: BTreeSet<&str> = audit.skipped.iter().map(|s| s.reason).collect();
    assert_eq!(
        reasons,
        BTreeSet::from(["no-chat-model", "truncated", "no-usage"]),
        "each shape is reported as what it is, and nothing panicked"
    );
}

#[test]
fn an_absent_or_corrupt_database_never_fails_the_pass() {
    {
        let _env = EnvGuard::point_at(Path::new("/nonexistent/antigravity-install"));
        assert!(AntigravityAdapter.discover(&DateFilter::default()).is_empty());
    }

    let fix = Fixture::new();
    let junk = fix.conversations().join("torn.db");
    std::fs::write(&junk, b"this is not sqlite at all, not even a header").unwrap();
    let _env = EnvGuard::point_at(fix.path());
    let files = AntigravityAdapter.discover(&DateFilter::default());
    assert_eq!(files.len(), 1, "one file listed, and it happens not to be a database");
    assert_eq!(files.len(), 1);
    let outcome = AntigravityAdapter.read(&files[0], ReadCursor(0)).expect("read is infallible");
    assert_eq!(outcome, usage_core::ReadOutcome::default());
    assert_eq!(AntigravityAdapter.audit(&files).unusable, 1);
}

/// The pitfall the reference implementations hit: uncheckpointed rows live in
/// the `-wal`, so anything that reads only the main file misses the turns the
/// user has just run.
#[test]
fn a_live_wal_database_shows_rows_that_never_reached_the_main_file() {
    let fix = Fixture::new();
    let gens = gen_blobs();
    let path = fix.build(SESSION, &[(0, gens[0].as_slice())], &[]);
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=wal; PRAGMA wal_autocheckpoint=0;").unwrap();
    writer.execute("INSERT INTO gen_metadata VALUES (1, ?1, 10)", rusqlite::params![gens[1]]).unwrap();
    writer.execute("INSERT INTO gen_metadata VALUES (2, ?1, 10)", rusqlite::params![gens[2]]).unwrap();
    let wal = fix.conversations().join(format!("{SESSION}.db-wal"));
    assert!(wal.is_file() && std::fs::metadata(&wal).unwrap().len() > 0, "the sidecar carries the rows");

    let _env = EnvGuard::point_at(fix.path());
    let files = AntigravityAdapter.discover(&DateFilter::default());
    let outcome = AntigravityAdapter.read(&files[0], ReadCursor(0)).unwrap();
    assert_eq!(outcome.events.len(), 3, "rows still sitting in the -wal are counted");
    let keys: BTreeSet<String> = outcome.events.iter().filter_map(|e| e.dedupe_key.clone()).collect();
    assert_eq!(keys.len(), 3);
    drop(writer);
}

/// And the ladder's second rung: when a read-only handle cannot build the
/// wal-index at all, the private copy must serve the same rows — while leaving
/// the user's directory byte-for-byte untouched.
#[test]
#[cfg(unix)]
fn a_wal_index_a_read_only_handle_cannot_rebuild_falls_back_to_a_private_copy() {
    let fix = Fixture::new();
    let gens = gen_blobs();
    let path = fix.build(SESSION, &[(0, gens[0].as_slice())], &[]);
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=wal; PRAGMA wal_autocheckpoint=0;").unwrap();
    writer.execute("INSERT INTO gen_metadata VALUES (1, ?1, 10)", rusqlite::params![gens[1]]).unwrap();
    let bytes = std::fs::read(&path).expect("fixture bytes");
    // Take the wal-index away while the writer keeps the -wal alive, then make the
    // directory unwritable: a read-only handle now has no way to recreate the
    // wal-index, which is precisely the case the copy exists for.
    std::fs::remove_file(fix.conversations().join(format!("{SESSION}.db-shm"))).unwrap();
    let before = listing(fix.conversations());
    readonly(fix.conversations(), true);

    let events = {
        let _env = EnvGuard::point_at(fix.path());
        let files = AntigravityAdapter.discover(&DateFilter::default());
        AntigravityAdapter.read(&files[0], ReadCursor(0)).unwrap().events
    };
    readonly(fix.conversations(), false);

    assert_eq!(events.len(), 2, "the copy served both the checkpointed and the -wal row");
    assert_eq!(listing(fix.conversations()), before, "the source directory gained nothing");
    assert_eq!(std::fs::read(&path).expect("still readable"), bytes, "and was never rewritten");
    let leftovers = temp_leftovers();
    assert!(leftovers.is_empty(), "the private copy is cleaned up: {leftovers:?}");
    drop(writer);
}

#[cfg(unix)]
fn readonly(dir: PathBuf, set: bool) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(if set { 0o555 } else { 0o755 })).unwrap();
}

#[cfg(unix)]
fn listing(dir: PathBuf) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .expect("read dir")
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect()
}

/// Copies this process made and did not remove. Filtered to our own pid, because
/// a run the harness killed earlier leaves directories no `Drop` ever got to
/// clean, and those are not this test's leak.
#[cfg(unix)]
fn temp_leftovers() -> Vec<PathBuf> {
    let root = std::env::temp_dir();
    let mine = format!("tokenme-antigravity-{}-", std::process::id());
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&mine)))
        .collect()
}

/// SQLite lets a re-opened file keep its old mtime granularity, so nudge it to
/// be sure the indexer's `(size, mtime)` change test fires.
fn touch(path: &Path) {
    let meta = std::fs::metadata(path).unwrap();
    let mtime = meta.modified().unwrap() + std::time::Duration::from_secs(1);
    let file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(mtime)).unwrap();
}

/// A second, smaller proof that a turn can be dated without its own stamp: drop
/// `#1.#9` from a fixture row and the `steps` row must supply its time.
#[test]
fn a_generation_without_its_own_timestamp_is_dated_from_the_steps_table() {
    let gens = gen_blobs();
    let steps = step_blobs();
    let want = expected_rows()[1].clone();
    let step = steps[1].clone().expect("fixture row 1 has a matching step row");
    let stamped = strip_timing(&gens[1]);
    assert_ne!(stamped, gens[1], "the row really did carry a #9 timing message");

    let fix = Fixture::new();
    fix.build(SESSION, &[(1, &stamped)], &[step.as_slice()]);
    let _env = EnvGuard::point_at(fix.path());

    let files = AntigravityAdapter.discover(&DateFilter::default());
    let outcome = AntigravityAdapter.read(&files[0], ReadCursor(0)).unwrap();
    assert_eq!(outcome.events.len(), 1);
    let event = &outcome.events[0];
    assert_eq!(event.counts.input, want.counts().0, "the tokens still came from gen_metadata");
    let ts = event.ts_ms;
    assert_ne!(ts, CREATED_MS, "the conversation's created-at was not needed");
    assert!(ts > CREATED_MS + 86_400_000, "no placeholder epoch: {ts}");
    assert!(
        ts.abs_diff(want.ts_ms.expect("fixture row is dated")) < 5_000,
        "steps dated it {ts}, its own stamp said {}",
        want.ts_ms.unwrap()
    );
}

/// `gen_metadata.data` with the chat-model message's `#9` (timing) removed.
fn strip_timing(blob: &[u8]) -> Vec<u8> {
    let chat = sub_message(blob, 1).expect("chat model message");
    tag_bytes(1, &drop_field(chat, 9))
}

fn sub_message(buf: &[u8], field: u64) -> Option<&[u8]> {
    let mut pos = 0;
    while pos < buf.len() {
        let (tag, next) = dec_varint(buf, pos)?;
        pos = next;
        match tag & 7 {
            0 => pos = dec_varint(buf, pos)?.1,
            1 => pos += 8,
            5 => pos += 4,
            2 => {
                let (len, next) = dec_varint(buf, pos)?;
                pos = next;
                let end = pos.checked_add(len as usize)?;
                if tag >> 3 == field {
                    return buf.get(pos..end);
                }
                pos = end;
            }
            _ => return None,
        }
        if pos > buf.len() {
            return None;
        }
    }
    None
}

/// The same message minus every occurrence of `drop`, everything else verbatim.
fn drop_field(buf: &[u8], drop: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let Some((tag, next)) = dec_varint(buf, pos) else { break };
        let mut end = next;
        let (field, wire) = (tag >> 3, tag & 7);
        match wire {
            0 => match dec_varint(buf, end) {
                Some((_, after)) => end = after,
                None => break,
            },
            1 => end += 8,
            5 => end += 4,
            2 => match dec_varint(buf, end) {
                Some((len, after)) => end = after + len as usize,
                None => break,
            },
            _ => break,
        }
        if end > buf.len() {
            break;
        }
        if field != drop {
            out.extend_from_slice(&buf[pos..end]);
        }
        pos = end;
    }
    out
}

fn dec_varint(buf: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut out = 0u64;
    let mut shift = 0;
    loop {
        let byte = *buf.get(pos)?;
        pos += 1;
        out |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((out, pos));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

#[test]
fn the_fixture_helpers_agree_with_the_bytes_they_trim() {
    let gens = gen_blobs();
    let chat = sub_message(&gens[1], 1).expect("chat model");
    assert!(sub_message(chat, 9).is_some(), "real rows carry their timing");
    assert!(sub_message(&drop_field(chat, 9), 9).is_none(), "and it can be removed surgically");
    assert!(sub_message(&drop_field(chat, 9), 4).is_some(), "the usage message survives");
}

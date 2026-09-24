//! End-to-end reads against a temp database built with OpenCode's real shapes.

mod common;

use common::*;
use usage_adapter_opencode::{Crow5Adapter, MimocodeAdapter, OpenCodeAdapter};
use usage_core::{Meter, ReadCursor, SourceAdapter, SourceFile};

fn read(db_path: &std::path::Path, cursor: ReadCursor) -> usage_core::ReadOutcome {
    let meta = std::fs::metadata(db_path).ok();
    let file = match meta {
        Some(m) => SourceFile {
            path: db_path.to_path_buf(),
            kind: usage_core::FileKind::Sqlite,
            size: m.len(),
            mtime_ms: 0,
        },
        None => SourceFile {
            path: db_path.to_path_buf(),
            kind: usage_core::FileKind::Sqlite,
            size: 0,
            mtime_ms: 0,
        },
    };
    OpenCodeAdapter.read(&file, cursor).expect("a locked or missing db is never Err")
}

/// The whole point of the mapping: our total equals the source's own billable
/// total, with reasoning folded into output rather than added on top.
#[test]
fn assistant_row_becomes_one_priced_event() {
    let d = db(true);
    let conn = d.conn();
    project(&conn, "prj_1", Some("mycloud"));
    session(&conn, "ses_1", "prj_1", "/Users/me/workspace/test");
    let rowid = message(&conn, "msg_0ce080864001v7gm5KqU8bkCHX", "ses_1", 1787799862846, ASSISTANT_SAMPLE);
    drop(conn);

    let out = read(&d.path, ReadCursor(0));
    assert_eq!(out.events.len(), 1);
    let e = &out.events[0];
    assert_eq!(e.tool, "opencode");
    assert_eq!(e.ts_ms, 1787799862846, "time_created is already milliseconds");
    assert_eq!(e.session, "ses_1");
    assert_eq!(e.project.as_deref(), Some("/Users/me/workspace/test"), "session.directory wins");
    assert_eq!(e.model.as_deref(), Some("muse-spark-1.2-contributor-free"), "bare modelID, no provider prefix");
    assert_eq!(e.counts.input, 55073.0);
    assert_eq!(e.counts.cache_read, 241.0);
    assert_eq!(e.counts.cache_creation, 0.0);
    assert_eq!(e.counts.output, 70.0 + 214.0);
    assert_eq!(e.counts.reasoning, 214.0);
    assert_eq!(e.counts.total(), 55598.0, "== source tokens.total");
    assert_eq!(e.meter, Meter::Tokens);
    assert_eq!(e.dedupe_key.as_deref(), Some("msg_0ce080864001v7gm5KqU8bkCHX"));
    assert_eq!(e.source, d.key());
    assert!(e.quota.is_none() && e.calls.is_empty());
    assert_eq!(out.cursor, ReadCursor(rowid as u64));

    let sem = OpenCodeAdapter.semantics();
    assert!(sem.dedupes_by_id && !sem.reports_quota);
}

#[test]
fn non_billable_rows_are_never_ingested() {
    let d = db(true);
    let conn = d.conn();
    project(&conn, "prj_1", None);
    session(&conn, "ses_1", "prj_1", "/w/a");
    message(&conn, "u1", "ses_1", 1787799862846, &user_data("hello"));
    // An assistant row with no tokens at all (aborted before the first delta).
    message(&conn, "a0", "ses_1", 1787799862900, &assistant_data("m", 0, 0, 0, 0, 0, "/w/a"));
    // A row whose JSON is not the documented shape at all.
    message(&conn, "junk", "ses_1", 1787799862950, r#"{"role":"assistant","#);
    let last = message(&conn, "a1", "ses_1", 1787799863000, &assistant_data("m", 100, 80, 10, 10, 0, "/w/a"));
    drop(conn);

    let out = read(&d.path, ReadCursor(0));
    assert_eq!(out.events.len(), 1, "user, zero-token and torn rows are skipped");
    assert_eq!(out.events[0].dedupe_key.as_deref(), Some("a1"));
    assert_eq!(out.cursor, ReadCursor(last as u64), "the cursor still passes over the skipped rows");
}

/// `session.directory` is the label; a pruned session row falls back to the
/// project name, and a message with neither falls back to its own cwd.
#[test]
fn project_label_falls_back_through_project_name_and_message_cwd() {
    let d = db(true);
    let conn = d.conn();
    project(&conn, "prj_named", Some("kept-name"));
    session(&conn, "ses_empty_dir", "prj_named", "   ");
    message(&conn, "a1", "ses_empty_dir", 1787799862846, &assistant_data("m", 30, 20, 10, 0, 0, "/msg/cwd"));
    message(&conn, "a2", "ses_orphan", 1787799862846, &assistant_data("m", 30, 20, 10, 0, 0, "/other/cwd"));
    drop(conn);

    let out = read(&d.path, ReadCursor(0));
    assert_eq!(out.events.len(), 2);
    assert_eq!(out.events[0].project.as_deref(), Some("kept-name"), "blank directory -> project.name");
    assert_eq!(out.events[1].project.as_deref(), Some("/other/cwd"), "no session row -> the message cwd");
}

#[test]
fn rowid_cursor_resumes_and_only_sees_new_rows() {
    let d = db(true);
    let conn = d.conn();
    project(&conn, "prj_1", None);
    session(&conn, "ses_1", "prj_1", "/w/a");
    let first_two = message(&conn, "a1", "ses_1", 1787799862846, &assistant_data("m", 100, 90, 10, 0, 0, "/w/a"));
    message(&conn, "a2", "ses_1", 1787799862900, &assistant_data("m", 100, 90, 10, 0, 0, "/w/a"));
    drop(conn);

    let out = read(&d.path, ReadCursor(0));
    assert_eq!(out.events.len(), 2);
    let cursor = out.cursor;
    assert_eq!(cursor, ReadCursor(first_two as u64 + 1));

    let again = read(&d.path, cursor);
    assert!(again.events.is_empty(), "nothing new means nothing replayed");
    assert_eq!(again.cursor, cursor, "and the cursor does not move");

    let conn = d.conn();
    message(&conn, "a3", "ses_1", 1787799863000, &assistant_data("m", 100, 90, 10, 0, 0, "/w/a"));
    let last = message(&conn, "a4", "ses_1", 1787799863100, &assistant_data("m", 100, 90, 10, 0, 0, "/w/a"));
    drop(conn);

    let later = read(&d.path, cursor);
    assert_eq!(later.events.len(), 2, "only the rows appended after the cursor");
    assert_eq!(
        later.events.iter().map(|e| e.dedupe_key.clone().unwrap()).collect::<Vec<_>>(),
        vec!["a3", "a4"]
    );
    assert_eq!(later.cursor, ReadCursor(last as u64));
}

/// The app owns this database; a read must never take, or wait for, a write lock.
#[test]
fn reading_while_a_live_writer_holds_the_database_still_works() {
    let d = db(true);
    let conn = d.conn();
    project(&conn, "prj_1", None);
    session(&conn, "ses_1", "prj_1", "/w/a");
    message(&conn, "a1", "ses_1", 1787799862846, &assistant_data("m", 100, 90, 10, 0, 0, "/w/a"));
    drop(conn);

    let writer = d.conn();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let hidden = assistant_data("m", 999, 999, 0, 0, 0, "/w/a");
    writer
        .execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('a2','ses_1',1787799863000,1787799863000,?1)",
            rusqlite::params![hidden],
        )
        .unwrap();

    let started = std::time::Instant::now();
    let out = read(&d.path, ReadCursor(0));
    assert_eq!(out.events.len(), 1, "the uncommitted row is invisible to us, as it should be");
    assert!(started.elapsed().as_secs() < 2, "read blocked behind the writer: {:?}", started.elapsed());

    // If we had taken any write lock, this commit would have failed.
    writer.execute_batch("COMMIT").unwrap();
    drop(writer);

    let after = read(&d.path, out.cursor);
    assert_eq!(after.events.len(), 1);
    assert_eq!(after.events[0].dedupe_key.as_deref(), Some("a2"));
}

#[test]
fn a_locking_writer_on_a_plain_journal_database_degrades_to_ok() {
    let d = db(false);
    let conn = d.conn();
    project(&conn, "prj_1", None);
    session(&conn, "ses_1", "prj_1", "/w/a");
    message(&conn, "a1", "ses_1", 1787799862846, &assistant_data("m", 100, 90, 10, 0, 0, "/w/a"));
    drop(conn);

    let writer = d.conn();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let out = std::panic::catch_unwind(|| read(&d.path, ReadCursor(0)));
    let out = out.expect("a busy database must never panic");
    assert!(out.events.len() <= 1, "either the committed rows or nothing, never an error");
    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);

    assert_eq!(read(&d.path, ReadCursor(0)).events.len(), 1, "and the next pass recovers");
}

#[test]
fn missing_and_not_a_database_files_are_ok() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.opencode.db");
    let out = read(&missing, ReadCursor(7));
    assert!(out.events.is_empty());
    assert_eq!(out.cursor, ReadCursor(7), "an unreadable db must not rewind the cursor");

    let junk = dir.path().join("opencode.db");
    std::fs::write(&junk, b"this is not a sqlite database at all").unwrap();
    let out = read(&junk, ReadCursor(0));
    assert!(out.events.is_empty());

    // An empty `message` table is a valid, if uninteresting, database.
    let empty = db(true);
    assert!(read(&empty.path, ReadCursor(0)).events.is_empty());
}

/// A provider-prefixed `modelID` is reported exactly as the source wrote it, so
/// `PricingMap` gets the same key a user would type.
#[test]
fn model_ids_are_passed_through_untouched() {
    let d = db(true);
    let conn = d.conn();
    project(&conn, "prj_1", None);
    session(&conn, "ses_1", "prj_1", "/w/a");
    message(&conn, "a1", "ses_1", 1787799862846, &assistant_data("deepseek-flash-v-wb", 30, 20, 10, 0, 0, "/w/a"));
    message(&conn, "a2", "ses_1", 1787799862900, &assistant_data("", 30, 20, 10, 0, 0, "/w/a"));
    drop(conn);

    let out = read(&d.path, ReadCursor(0));
    assert_eq!(out.events[0].model.as_deref(), Some("deepseek-flash-v-wb"));
    assert_eq!(out.events[1].model, None, "an empty modelID stays unknown rather than guessed");
}

/// Verbatim from this machine's `~/.local/share/crow5/opencode-powerformer-v1.18.1.db`.
/// `cache.read` is 36x `input` here, which is the dialect working as designed: the
/// stages are mutually exclusive and `total` is their sum, so nothing is subtracted
/// from `input` and there is nothing to "fix".
const CROW5_ROW: &str = r#"{"parentID":"msg_fd58ef17d001A5UlAsi56aQmnL","role":"assistant","mode":"build","agent":"build","path":{"cwd":"/Users/me/Library/Application Support/Open Design/namespaces/release-stable/data/projects/af392a8c","root":"/"},"cost":0,"tokens":{"total":111783,"input":3006,"output":206,"reasoning":27,"cache":{"write":0,"read":108544}},"modelID":"deepseek-v4-flash-ga-260731","providerID":"open-design-byok","time":{"created":1785994802824,"completed":1785994816263},"finish":"tool-calls"}"#;

/// Verbatim from this machine's `~/.local/share/mimocode/mimocode.db`.
const MIMOCODE_ROW: &str = r#"{"parentID":"msg_f65239a24001Ahn2rXO0PvmsWb","role":"assistant","mode":"build","agent":"build","path":{"cwd":"/Users/me/workspace/macmini","root":"/"},"cost":0,"tokens":{"total":33785,"input":2429,"output":92,"reasoning":160,"cache":{"write":0,"read":31104}},"modelID":"mimo-auto","providerID":"mimo","time":{"created":1784108268372,"completed":1784108273415},"finish":"tool-calls"}"#;

/// The siblings share this parser, so the one thing that must differ per product
/// is the id stamped on the events — otherwise Crow5's spend lands on OpenCode.
#[test]
fn sibling_products_reuse_the_parser_and_carry_their_own_tool_id() {
    let cases: [(Box<dyn SourceAdapter>, &str, &str); 3] = [
        (Box::new(OpenCodeAdapter), CROW5_ROW, "opencode"),
        (Box::new(Crow5Adapter), CROW5_ROW, "crow5"),
        (Box::new(MimocodeAdapter), MIMOCODE_ROW, "mimocode"),
    ];
    for (adapter, row, want_tool) in cases {
        let d = db(true);
        let conn = d.conn();
        project(&conn, "prj_1", None);
        session(&conn, "ses_1", "prj_1", "/w/crow5");
        message(&conn, "a1", "ses_1", 1785994802824, row);
        drop(conn);

        let out = adapter
            .read(&d.source(), ReadCursor(0))
            .expect("read is infallible by contract");
        assert_eq!(out.events.len(), 1, "{want_tool} row must produce one event");
        let e = &out.events[0];
        assert_eq!(e.tool, want_tool, "the product decides the tool id");
        assert_eq!(e.ts_ms, 1785994802824);
        assert_eq!(e.session, "ses_1");
        assert_eq!(e.dedupe_key.as_deref(), Some("a1"));
        assert_eq!(e.source, d.key());
        assert_eq!(e.project.as_deref(), Some("/w/crow5"), "session.directory still wins");
    }
}

/// Crow5's stage numbers, from the same row the id test above uses: reasoning
/// folds into output, cache read stays its own stage.
#[test]
fn crow5_stages_map_exactly_like_opencodes() {
    let d = db(true);
    let conn = d.conn();
    project(&conn, "prj_1", None);
    session(&conn, "ses_1", "prj_1", "/w/crow5");
    message(&conn, "a1", "ses_1", 1785994802824, CROW5_ROW);
    drop(conn);

    let out = Crow5Adapter
        .read(&d.source(), ReadCursor(0))
        .expect("read is infallible by contract");
    let c = &out.events[0].counts;
    assert_eq!(c.input, 3006.0);
    assert_eq!(c.cache_read, 108544.0);
    assert_eq!(c.cache_creation, 0.0);
    assert_eq!(c.output, 206.0 + 27.0, "reasoning folds into output, as in OpenCode");
    assert_eq!(c.reasoning, 27.0);
    assert_eq!(c.credits, 0.0, "the source's own cost is still ignored");
    assert_eq!(c.total(), 111_783.0, "== the row's own tokens.total");
    assert!(
        c.cache_read > c.input,
        "cache read dwarfs input in this dialect and that is not a bug to fix"
    );
    assert_eq!(out.events[0].model.as_deref(), Some("deepseek-v4-flash-ga-260731"));
}

#[test]
fn mimocode_stages_map_exactly_like_opencodes() {
    let d = db(true);
    let conn = d.conn();
    project(&conn, "prj_1", None);
    session(&conn, "ses_1", "prj_1", "/w/mimocode");
    message(&conn, "a1", "ses_1", 1784108268372, MIMOCODE_ROW);
    drop(conn);

    let out = MimocodeAdapter
        .read(&d.source(), ReadCursor(0))
        .expect("read is infallible by contract");
    let e = &out.events[0];
    assert_eq!(e.tool, "mimocode");
    let c = &e.counts;
    assert_eq!((c.input, c.cache_read, c.cache_creation), (2429.0, 31104.0, 0.0));
    assert_eq!(c.output, 92.0 + 160.0);
    assert_eq!(c.total(), 33_785.0, "== the row's own tokens.total");
    assert_eq!(e.project.as_deref(), Some("/w/mimocode"));
    assert_eq!(e.model.as_deref(), Some("mimo-auto"));
}

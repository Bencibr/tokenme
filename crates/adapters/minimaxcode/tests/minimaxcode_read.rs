//! End-to-end reads against a session tree built in MiniMax Code's real layout.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use usage_adapter_minimaxcode::{MiniMaxCodeAdapter, TOOL_ID};
use usage_core::{DateFilter, FileKind, Meter, ReadCursor, SourceAdapter, UsageEvent};

/// `set_var` is process-global, so these tests run one at a time.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// One assistant record, in the envelope the app writes: a billable call carrying
/// its own four stages and its own `totalTokens`.
fn call(id: &str, model: &str, input: u64, output: u64, read: u64, write: u64, ms: u64) -> String {
    format!(
        r#"{{"message_id":"{id}","turn_id":"turn-1","message":{{"role":"assistant","api":"anthropic-messages","provider":"minimax","model":"{model}","stopReason":"toolCall","timestamp":{ms},"responseId":"resp-{id}","usage":{{"input":{input},"output":{output},"cacheRead":{read},"cacheWrite":{write},"totalTokens":{}, "cost":{{"total":0}}}}}}}}"#,
        input + output + read + write
    )
}

/// The records that are never billed: the other three roles the same file holds.
fn noise() -> Vec<String> {
    vec![
        r#"{"message_id":"msg-u","message":{"role":"user","content":[{"type":"text","text":"go"}],"timestamp":1791158830000}}"#.to_string(),
        // A `toolResult` with a usage object beside it is still not a call of this
        // session: the sub-agent's own transcript bills it.
        r#"{"message_id":"msg-t","message":{"role":"toolResult","toolCallId":"t1","content":[],"usage":{"input":9,"output":9},"timestamp":1791158831000}}"#.to_string(),
        r#"{"message_id":"msg-c","message":{"role":"custom","customType":"goal-update","timestamp":1791158832000}}"#.to_string(),
        // An aborted assistant turn reports zeros.
        call("msg-abort", "MiniMax-M2.7", 0, 0, 0, 0, 1791158833000),
    ]
}

/// `<home>/v2/sessions/<Y>/<M>/<D>/<leaf>/messages.jsonl`, with the manifest that
/// names the session, exactly as `session-history-paths.ts` writes them.
fn session(root: &Path, day: &str, leaf: &str, id: &str, lines: &[String]) {
    let dir = root.join("v2").join("sessions").join(day).join(leaf);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        format!(r#"{{"schemaVersion":1,"sessionId":"{id}","layout":"v2-final-dated-session"}}"#),
    )
    .unwrap();
    std::fs::write(dir.join("messages.jsonl"), format!("{}\n", lines.join("\n"))).unwrap();
}

/// Everything the adapter bills from the tree the environment variable pins.
fn read_all() -> Vec<UsageEvent> {
    let mut out = Vec::new();
    for file in MiniMaxCodeAdapter.discover(&DateFilter::default()) {
        let outcome = MiniMaxCodeAdapter.read(&file, ReadCursor(0)).unwrap();
        assert_eq!(outcome.cursor, ReadCursor(0), "a Tree source has no cursor to advance");
        out.extend(outcome.events);
    }
    out
}

/// Two sessions — one of them a sub-agent's own transcript — and every role the
/// format carries: four billed calls, one event each, none of them attributed to
/// the wrong session, and the noise dropped.
#[test]
fn a_real_session_tree_bills_every_call_once() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MINIMAX_DATA_DIR", dir.path());
    let root = dir.path();
    let mut main = noise();
    main.push(call("msg-a1", "MiniMax-M3.1-Flash-Preview", 21029, 256, 1924, 0, 1791158837144));
    main.push(call("msg-a2", "MiniMax-M3.1-Flash-Preview", 500, 120, 12_000, 40, 1791158847292));
    session(root, "2026/10/05", "00-07-16-076-session_MAIN", "mvs_main", &main);
    session(
        root,
        "2026/10/05",
        "00-09-00-000-session_SUB",
        "mvs_sub",
        &[call("msg-s1", "MiniMax-M2.7", 7, 3, 0, 0, 1791158940000)],
    );

    let mut events = read_all();
    events.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms));
    assert_eq!(events.len(), 3, "user, toolResult, custom and an aborted call bill nothing");
    assert!(events.iter().all(|e| e.tool == TOOL_ID), "every event is this tool's");
    assert!(events.iter().all(|e| e.meter == Meter::Tokens));

    assert_eq!(events[0].session, "mvs_main");
    assert_eq!(events[0].model.as_deref(), Some("MiniMax-M3.1-Flash-Preview"));
    // Stages land where the vendor's own total says they belong.
    assert_eq!(
        (events[0].counts.input, events[0].counts.cache_read, events[0].counts.output),
        (21029.0, 1924.0, 256.0)
    );
    assert_eq!(events[0].counts.total(), 23_209.0, "our total is the record's own totalTokens");
    assert_eq!(events[0].dedupe_key.as_deref(), Some("minimaxcode#msg-a1"));
    // A sub-agent's transcript is its own session row, not folded into the parent:
    // the vendor's own projection keys every row by the session that served it.
    assert_eq!(events[2].session, "mvs_sub");
    assert_eq!(events[2].counts.total(), 10.0);
    // Each event names the file it was read from, and the two sessions are two
    // files: the indexer's per-source cursor bookkeeping depends on it.
    let sources: std::collections::HashSet<&str> =
        events.iter().map(|e| e.source.as_str()).collect();
    assert_eq!(sources.len(), 2, "one source per transcript");
    assert!(
        events.iter().all(|e| e.source.ends_with("messages.jsonl")),
        "the file is the source: {:?}",
        events[0].source
    );
    std::env::remove_var("MINIMAX_DATA_DIR");
}

/// A materialised rewrite — the rewind and compaction paths replace the whole
/// file — must not double-count what survived and must drop what was removed.
#[test]
fn a_rewritten_history_replays_without_double_counting() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MINIMAX_DATA_DIR", dir.path());
    let root = dir.path();
    let leaf = "00-07-16-076-session_MAIN";
    session(
        root,
        "2026/10/05",
        leaf,
        "mvs_main",
        &[
            call("msg-a1", "MiniMax-M2.7", 100, 10, 0, 0, 1791158837144),
            call("msg-a2", "MiniMax-M2.7", 200, 20, 0, 0, 1791158847292),
            call("msg-a3", "MiniMax-M2.7", 300, 30, 0, 0, 1791158857292),
        ],
    );
    let before: Vec<(Option<String>, f64)> =
        read_all().into_iter().map(|e| (e.dedupe_key, e.counts.total())).collect();
    assert_eq!(before.len(), 3);

    // What `canonical-history-materializer.ts` does: a new file, renamed over the
    // old one, with the rewound tail gone, a surviving call *restated* with its
    // final counts, and one new call appended.
    let transcript = root
        .join("v2/sessions/2026/10/05")
        .join(leaf)
        .join("messages.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}\n",
            [
                call("msg-a1", "MiniMax-M2.7", 150, 15, 0, 0, 1791158837144),
                call("msg-a4", "MiniMax-M2.7", 400, 40, 0, 0, 1791158867292),
            ]
            .join("\n")
        ),
    )
    .unwrap();

    let after = read_all();
    let keys: Vec<&str> = after.iter().filter_map(|e| e.dedupe_key.as_deref()).collect();
    assert_eq!(keys, vec!["minimaxcode#msg-a1", "minimaxcode#msg-a4"]);
    assert_eq!(
        after[0].counts.total(),
        165.0,
        "the restated call carries its new counts under the same identity, so the index's monotone repair sees the growth"
    );
    // The identity of a surviving record did not move with the rewrite: same key,
    // so a replay is absorbed rather than added.
    assert_eq!(before[0].0, after[0].dedupe_key);
    std::env::remove_var("MINIMAX_DATA_DIR");
}

/// The project label is the workspace the vendor recorded for that session id;
/// a session it does not know about still bills, it just goes unlabelled.
#[test]
fn the_workspace_label_comes_from_the_runtime_store() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MINIMAX_DATA_DIR", dir.path());
    let root = dir.path();
    session(
        root,
        "2026/10/05",
        "00-07-16-076-session_MAIN",
        "mvs_main",
        &[call("msg-a1", "MiniMax-M2.7", 10, 1, 0, 0, 1791158837144)],
    );
    session(
        root,
        "2026/10/05",
        "00-08-00-000-session_ORPHAN",
        "mvs_orphan",
        &[call("msg-o1", "MiniMax-M2.7", 5, 1, 0, 0, 1791158838144)],
    );

    let db = root.join("v2/sqlite");
    std::fs::create_dir_all(&db).unwrap();
    let conn = Connection::open(db.join("runtime-state.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE local_runtime_sessions (session_id TEXT PRIMARY KEY, workspace_dir TEXT);
         INSERT INTO local_runtime_sessions VALUES ('mvs_main', '/Users/demo/projA'), ('mvs_orphan', NULL);",
    )
    .unwrap();
    drop(conn);

    let mut events = read_all();
    events.sort_by(|a, b| a.session.cmp(&b.session));
    assert_eq!(events.len(), 2, "a missing label never costs an event");
    assert_eq!(events[0].project.as_deref(), Some("/Users/demo/projA"));
    assert_eq!(events[1].project, None, "no workspace_dir is no label, not a wrong one");
    std::env::remove_var("MINIMAX_DATA_DIR");
}

/// A store that cannot be opened costs the label only: the transcript is still
/// read, and a torn tail is still left for the next pass.
#[test]
fn a_broken_store_and_a_torn_tail_lose_nothing_readable() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MINIMAX_DATA_DIR", dir.path());
    let root = dir.path();
    session(
        root,
        "2026/10/05",
        "00-07-16-076-session_MAIN",
        "mvs_main",
        &[call("msg-a1", "MiniMax-M2.7", 10, 1, 0, 0, 1791158837144)],
    );
    // A file where a database should be: the ladder fails on all three rungs.
    std::fs::create_dir_all(root.join("v2/sqlite")).unwrap();
    std::fs::write(root.join("v2/sqlite/runtime-state.sqlite"), "not a database\n").unwrap();

    let transcript = root.join("v2/sessions/2026/10/05/00-07-16-076-session_MAIN/messages.jsonl");
    let whole = std::fs::read_to_string(&transcript).unwrap();
    // The record the app is mid-write through: no newline, and a truncated body.
    std::fs::write(&transcript, format!("{}\"{{\"message_id\":\"msg-half\"", whole)).unwrap();

    let events = read_all();
    assert_eq!(events.len(), 1, "the complete line bills, the torn one waits");
    assert_eq!(events[0].dedupe_key.as_deref(), Some("minimaxcode#msg-a1"));
    assert_eq!(events[0].project, None, "an unreadable store costs the label only");

    // An empty transcript and a manifest-less directory are both harmless.
    std::fs::write(&transcript, "").unwrap();
    assert!(read_all().is_empty());
    std::env::remove_var("MINIMAX_DATA_DIR");
}

/// A record with no id and no stamp still bills, positioned by where it sits; and
/// a record with no id but a stamp is identified by its turn, model and stages.
#[test]
fn anonymous_records_get_an_identity_that_survives_a_replay() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MINIMAX_DATA_DIR", dir.path());
    let stamped = r#"{"message":{"role":"assistant","model":"MiniMax-M2.7","timestamp":1791158837144,"usage":{"input":4,"output":6,"cacheRead":0,"cacheWrite":0}}}"#;
    let undated = r#"{"message":{"role":"assistant","model":"MiniMax-M2.7","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0}}}"#;
    session(
        dir.path(),
        "2026/10/05",
        "00-07-16-076-session_ANON",
        "mvs_anon",
        &[stamped.to_string(), undated.to_string()],
    );

    let mut files = MiniMaxCodeAdapter.discover(&DateFilter::default());
    assert_eq!(files.len(), 1);
    // This layout is read whole, every pass, because the vendor rewrites it.
    assert_eq!(files[0].kind, FileKind::Tree);
    let file = files.remove(0);
    let files_mtime = file.mtime_ms;
    let events = MiniMaxCodeAdapter.read(&file, ReadCursor(0)).unwrap().events;
    assert_eq!(events.len(), 2);
    let stamped_key = format!(
        "minimaxcode#mvs_anon#-#{}#MiniMax-M2.7#4#0#0#6",
        1_791_158_837_144_i64
    );
    assert_eq!(events[0].dedupe_key.as_deref(), Some(stamped_key.as_str()));
    // Undated: filed under the file's last write rather than a 1970 bucket, and
    // identified by position in the file because content cannot tell twins apart.
    assert_eq!(events[1].ts_ms, files_mtime, "the file's own stamp stands in: {}", events[1].ts_ms);
    assert!(
        events[1].dedupe_key.as_deref().unwrap().ends_with("#1"),
        "the second line's position is the last resort: {:?}",
        events[1].dedupe_key
    );
    std::env::remove_var("MINIMAX_DATA_DIR");
}

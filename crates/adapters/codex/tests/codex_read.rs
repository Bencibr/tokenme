//! Reading a real-shaped Codex rollout: exact numbers, model attribution,
//! quota, and byte-cursor resume.

use std::path::{Path, PathBuf};

use usage_adapter_codex::CodexAdapter;
use usage_core::{FileKind, ReadCursor, SourceAdapter, SourceFile, UsageEvent};

const FIXTURE: &str =
    "tests/fixtures/sessions/2026-09-23/rollout-2026-09-23T11-32-11-01a0ce03-33ce-7172-848c-7199f1589e2e.jsonl";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

fn source_file(path: &Path) -> SourceFile {
    let meta = std::fs::metadata(path).expect("fixture exists");
    SourceFile {
        path: path.to_path_buf(),
        kind: FileKind::Jsonl,
        size: meta.len(),
        mtime_ms: meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64,
    }
}

fn read(path: &Path, cursor: ReadCursor) -> (Vec<UsageEvent>, ReadCursor) {
    let a = CodexAdapter;
    let out = a.read(&source_file(path), cursor).expect("read never errors for a readable file");
    (out.events, out.cursor)
}

fn write_tmp(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// The three numeric traps asserted together on the verified real record.
#[test]
fn fixture_maps_the_verified_sample_exactly() {
    let (events, cursor) = read(&fixture_path(), ReadCursor(0));
    assert_eq!(events.len(), 2, "one per non-zero token_count; the malformed, echo and partial lines drop out");
    let e = &events[0];
    assert_eq!(e.tool, "codex");
    assert_eq!(e.session, "01a0ce03-33ce-7172-848c-7199f1589e2e");
    assert_eq!(e.project.as_deref(), Some("/Users/me/workspace/bug-hunter"));
    assert_eq!(e.model.as_deref(), Some("gpt-5.6-luna"), "from the turn_context before it");
    assert_eq!(e.counts.input, 15632.0 - 3072.0, "cached split out of input");
    assert_eq!(e.counts.cache_read, 3072.0);
    assert_eq!(e.counts.cache_creation, 0.0);
    assert_eq!(e.counts.output, 55.0);
    assert_eq!(e.counts.reasoning, 37.0, "informational sub-split of output");
    // total() == the source's own billable total, reasoning not added on top.
    assert_eq!(e.counts.total(), 15687.0);
    assert_eq!(usage_core::parse_ts_ms("2026-09-23T11:32:31.944Z").unwrap(), e.ts_ms);
    assert_eq!(e.source, source_file(&fixture_path()).key());
    assert!(e.calls.is_empty());
    // dedupe_key: None on purpose — Codex records carry no per-call id, so
    // idempotency comes from the byte cursor instead.
    assert_eq!(e.dedupe_key, None);
    assert!(cursor.0 > 0 && cursor.0 <= source_file(&fixture_path()).size);
    assert_eq!(events[1].model.as_deref(), Some("gpt-5.5-codex"), "later turn_context wins");
    assert_eq!(events[1].counts.total(), 8402.0);
    assert_eq!(events[1].counts.cached_pct(), 6144.0 / 8192.0 * 100.0);
}

#[test]
fn quota_sample_is_taken_from_the_same_payload() {
    let (events, _) = read(&fixture_path(), ReadCursor(0));
    let q = events[0].quota.as_ref().expect("primary present");
    assert_eq!(q.used_percent, 45.0);
    assert_eq!(q.window_minutes, 300);
    assert_eq!(q.resets_at_ms, 1_790_170_235_000, "resets_at is unix seconds, scaled once");
    assert_eq!(events[1].quota, None, "a token_count without rate_limits stays quota-free");
}

#[test]
fn second_read_is_empty_and_the_cursor_does_not_move() {
    let (first, cursor) = read(&fixture_path(), ReadCursor(0));
    assert_eq!(first.len(), 2);
    let (again, cursor2) = read(&fixture_path(), cursor);
    assert!(again.is_empty(), "the cursor is the whole story: nothing is re-ingested");
    assert_eq!(cursor2, cursor, "the unconsumed partial line must not move the cursor");
}

/// The fixture's last line has no newline (the app was mid-write). Once the rest
/// of the line lands, the next pass must pick it up and only that record.
#[test]
fn partial_trailing_line_is_resumed_not_eaten() {
    let dir = tempfile::tempdir().unwrap();
    let body = std::fs::read_to_string(fixture_path()).unwrap();
    assert!(body.ends_with("token_co"), "fixture ends mid-record");
    let path = write_tmp(dir.path(), "rollout-2026-09-23T11-32-11-01a0ce03-33ce-7172-848c-7199f1589e2e.jsonl", &body);
    let (events, cursor) = read(&path, ReadCursor(0));
    assert_eq!(events.len(), 2);
    let last_newline = body.rfind('\n').unwrap() as u64 + 1;
    assert_eq!(cursor.0, last_newline, "consumes up to the last newline only");

    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(
            r#"unt","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":25,"reasoning_output_tokens":0,"total_tokens":1025},"model_context_window":258400}}}"#
                .as_bytes(),
        )
        .unwrap();
        f.write_all(b"\n").unwrap();
    }
    let (late, cursor2) = read(&path, cursor);
    assert_eq!(late.len(), 1, "exactly the record that was mid-write");
    assert_eq!(late[0].counts.total(), 1025.0);
    assert_eq!(late[0].model.as_deref(), Some("gpt-5.5-codex"), "turn context survives across passes");
    assert_eq!(cursor2.0, std::fs::metadata(&path).unwrap().len(), "now at EOF");
}

/// `token_count` has no model field, so ordering decides: nearest preceding
/// `turn_context` wins, `session_meta` is the fallback, `None` beats a guess.
#[test]
fn model_attribution_follows_turn_then_meta_then_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let usage = r#"{"timestamp":"2026-09-23T12:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":40,"cached_input_tokens":0,"output_tokens":5,"total_tokens":45}}}}"#;

    // Only session_meta: the fallback applies.
    let meta_only = format!(
        r#"{{"timestamp":"2026-09-23T11:00:00.000Z","type":"session_meta","payload":{{"session_id":"s1","cwd":"/w","model":"gpt-5.1"}}}}
{usage}
"#
    );
    let p = write_tmp(dir.path(), "rollout-2026-09-23T11-00-00-00000000-0000-0000-0000-000000000001.jsonl", &meta_only);
    let (events, _) = read(&p, ReadCursor(0));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].model.as_deref(), Some("gpt-5.1"));
    assert_eq!(events[0].project.as_deref(), Some("/w"));

    // A turn_context model always beats the meta one, and the newest wins.
    let both = format!(
        r#"{{"timestamp":"2026-09-23T11:00:00.000Z","type":"session_meta","payload":{{"session_id":"s2","model":"gpt-5.1"}}}}
{{"timestamp":"2026-09-23T11:00:01.000Z","type":"turn_context","payload":{{"turn_id":"a","cwd":"/x","model":"gpt-5.6-luna"}}}}
{usage}
{{"timestamp":"2026-09-23T12:00:00.000Z","type":"turn_context","payload":{{"turn_id":"b","model":"gpt-5.5-codex"}}}}
{usage}
"#
    );
    let p = write_tmp(dir.path(), "rollout-2026-09-23T11-00-00-00000000-0000-0000-0000-000000000002.jsonl", &both);
    let (events, _) = read(&p, ReadCursor(0));
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(events[1].model.as_deref(), Some("gpt-5.5-codex"));
    // turn_context cwd only fills the project when session_meta had none.
    assert_eq!(events[1].project.as_deref(), Some("/x"));

    // No model anywhere: reported unknown rather than inherited from a sibling file.
    let bare = format!("{usage}\n");
    let p = write_tmp(dir.path(), "rollout-2026-09-23T11-00-00-00000000-0000-0000-0000-000000000003.jsonl", &bare);
    let (events, _) = read(&p, ReadCursor(0));
    assert_eq!(events[0].model, None);
    assert_eq!(
        events[0].session, "00000000-0000-0000-0000-000000000003",
        "session id falls back to the uuid in the file name"
    );
}

#[test]
fn unreadable_and_empty_files_degrade_to_an_empty_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.jsonl");
    let a = CodexAdapter;
    let out = a.read(&source_file_like(&missing), ReadCursor(0)).expect("never Err");
    assert!(out.events.is_empty());
    assert_eq!(out.cursor, ReadCursor(0));

    let empty = write_tmp(dir.path(), "rollout-2026-09-23T11-00-00-00000000-0000-0000-0000-000000000004.jsonl", "");
    let out = a.read(&source_file(&empty), ReadCursor(0)).unwrap();
    assert!(out.events.is_empty());
    assert_eq!(out.cursor, ReadCursor(0));

    // A truncated/rotated log whose cursor is past EOF is re-read from the top;
    // the indexer purges by `source` key, which every event carries.
    let body = std::fs::read_to_string(fixture_path()).unwrap();
    let p = write_tmp(dir.path(), "rollout-2026-09-23T11-00-00-00000000-0000-0000-0000-000000000005.jsonl", &body[..200]);
    let out = a.read(&source_file(&p), ReadCursor(9_000)).unwrap();
    assert!(out.cursor.0 <= 200, "cursor rewound to the truncated end: {}", out.cursor.0);
}

fn source_file_like(path: &Path) -> SourceFile {
    SourceFile { path: path.to_path_buf(), kind: FileKind::Jsonl, size: 0, mtime_ms: 0 }
}

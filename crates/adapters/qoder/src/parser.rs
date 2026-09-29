//! `<root>/projects/**/*.jsonl` → normalised [`usage_core::UsageEvent`]s.
//!
//! The records are Claude-Code shaped, but the payload is not: across 5 555
//! real usage-bearing records every one of `input_tokens`, `output_tokens`,
//! `cache_creation_input_tokens` and `cache_read_input_tokens` is `0`, while
//! `message.usage.credits` carries the actual charge. This adapter therefore maps
//! credits and nothing else, and emits [`Meter::Credits`] — the real per-call token
//! numbers live in the IDE's own store, which [`crate::cache_db`] reads.
//!
//! A record's identity is `message.usage.request_id`, measured on this machine over
//! two passes of the live tree (7 372 and 7 605 usage-bearing records): every record
//! in both carries it as a non-empty string, all are distinct, and none maps to more
//! than one `message.id`. That is the same column [`crate::cache_db`] keys its events
//! with, which is what lets one call present in both stores be indexed once;
//! `message.id` stays the fallback.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};

use serde_json::Value;
use usage_core::{
    parse_ts_ms, Error, Meter, ReadCursor, ReadOutcome, SourceFile, TokenCounts, UsageEvent,
};

use crate::{DEDUPE_PREFIX, TOOL_ID};

/// Cheap pre-filter: only a line carrying a `credits` key can move the meter, so
/// the ~12 k streamed blocks, `active-leaf` and `user` records per session never
/// reach the JSON parser.
const MARK: &str = "\"credits\"";

/// Read everything appended after `cursor`, stopping before a line that is still
/// being written.
pub(crate) fn read_file(file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
    let Some(tail) = read_tail(file, cursor.0)? else {
        return Ok(untouched(cursor));
    };
    let Some(last) = tail.iter().rposition(|b| *b == b'\n') else {
        // No complete line in the window: a record still being flushed.
        return Ok(untouched(cursor));
    };
    let processed = last + 1;
    let mut turns = Turns::default();
    for raw in tail[..processed].split(|b| *b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        // Invalid UTF-8 means a torn write, same class as a JSON parse failure.
        let Ok(text) = std::str::from_utf8(raw) else {
            continue;
        };
        let text = text.strip_suffix('\r').unwrap_or(text);
        if !text.contains(MARK) {
            continue;
        }
        // Qoder interleaves non-record lines (and, under `--include-partial-messages`,
        // half-flushed records): skipping them is the whole error policy, since the
        // next append re-delivers a torn tail from the cursor we did not advance.
        let Ok(line) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        turns.consume(&line);
    }
    Ok(ReadOutcome {
        events: turns.finish(file),
        cursor: ReadCursor(cursor.0 + processed as u64),
    })
}

fn untouched(cursor: ReadCursor) -> ReadOutcome {
    ReadOutcome {
        events: Vec::new(),
        cursor,
    }
}

/// `Ok(None)` covers every "this file is not readable right now" case: the
/// indexer logs it and moves on, so it must not abort the whole pass.
fn read_tail(file: &SourceFile, start: u64) -> Result<Option<Vec<u8>>, Error> {
    let Ok(mut handle) = File::open(&file.path) else {
        return Ok(None);
    };
    let Ok(len) = handle.metadata().map(|m| m.len()) else {
        return Ok(None);
    };
    if start > len {
        // The log was truncated or rotated underneath our cursor. Only the
        // indexer, which owns the manifest, may decide to purge and re-read.
        return Err(Error::Cursor {
            path: file.path.clone(),
            cursor: start,
        });
    }
    if start == len {
        return Ok(Some(Vec::new()));
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if handle.seek(SeekFrom::Start(start)).is_err() || handle.read_to_end(&mut buf).is_err() {
        // A short read would hand us a torn record; drop the window whole.
        return Ok(None);
    }
    Ok(Some(buf))
}

/// One assistant message inside a read window. Fields are owned so a parsed line
/// can be dropped at once: keeping every `Value` of a 20 MB transcript alive
/// until the end of the window would cost more than the transcript itself.
#[derive(Default)]
struct Turn {
    id: String,
    /// `message.usage.request_id`: the vendor's identity for this one call, and the
    /// only value the transcript and the IDE cache db both write.
    request: Option<String>,
    ts_ms: Option<i64>,
    session: Option<String>,
    project: Option<String>,
    model: Option<String>,
    counts: TokenCounts,
}

/// Assistant messages seen in one read window, in file order.
#[derive(Default)]
struct Turns {
    turns: Vec<Turn>,
    index: HashMap<String, usize>,
}

impl Turns {
    fn consume(&mut self, line: &Value) {
        // Only assistant records carry `message.usage`; `user`, `active-leaf`,
        // `attachment`, `runtime-config` and friends are conversation plumbing.
        if line.get("type").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let Some(message) = line.get("message") else {
            return;
        };
        // Without a message id there is nothing for `dedupe_key` to repeat on, so
        // a later pass could not tell a re-write from a new turn.
        let Some(id) = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let slot = match self.index.get(id) {
            Some(&seen) => seen,
            None => {
                self.turns.push(Turn {
                    id: id.to_string(),
                    ..Turn::default()
                });
                self.index.insert(id.to_string(), self.turns.len() - 1);
                self.turns.len() - 1
            }
        };
        let bucket = &mut self.turns[slot];
        if bucket.ts_ms.is_none() {
            bucket.ts_ms = text(line, "timestamp").and_then(parse_ts_ms);
        }
        if bucket.session.is_none() {
            bucket.session = text(line, "sessionId")
                .or_else(|| text(line, "session_id"))
                .map(str::to_string);
        }
        if bucket.project.is_none() {
            bucket.project = text(line, "cwd").map(str::to_string);
        }
        // Internal aliases (`qfmodel`, `gfmodel`, `qmodel_38max`) stay verbatim:
        // renaming one to a public model would attach a price Qoder never charged.
        if bucket.model.is_none() {
            bucket.model = text(message, "model").map(str::to_string);
        }
        if let Some(usage) = message.get("usage").filter(|u| u.is_object()) {
            // Streamed content blocks carry no `usage` at all, so the id can arrive
            // on any line of the turn: first one seen wins, like every other field.
            if bucket.request.is_none() {
                bucket.request = text(usage, "request_id")
                    .or_else(|| text(usage, "requestId"))
                    .map(str::to_string);
            }
            let counts = counts_from(usage);
            // Streaming logs one line per content block and a retried flush can
            // repeat the finished usage. Keep the largest snapshot: the charge is
            // per message, so summing the repeats would multiply it.
            if counts.credits > bucket.counts.credits {
                bucket.counts = counts;
            }
        }
    }

    fn finish(self, file: &SourceFile) -> Vec<UsageEvent> {
        let mut events = Vec::new();
        let mut claimed: HashSet<String> = HashSet::new();
        for bucket in self.turns {
            // A zero-credit record is not a billable request: the free/aborted
            // turns (and every streamed content block, which has no `usage` at
            // all) report 0 on the only meter this source has, and `billable`
            // cannot stand in for it — 5 400 of 5 555 real records carry
            // `billable: false` next to a non-zero charge.
            if bucket.counts.is_zero() {
                continue;
            }
            let session = bucket.session.unwrap_or_else(|| session_fallback(file));
            let mut event = UsageEvent::new(
                TOOL_ID,
                // A record with no timestamp is filed under the file's mtime
                // rather than epoch 0, which would invent a 1970 bucket.
                bucket.ts_ms.unwrap_or(file.mtime_ms),
                session.clone(),
            );
            event.project = bucket.project.or_else(|| project_fallback(file));
            event.model = bucket.model;
            event.counts = bucket.counts;
            event.meter = Meter::Credits;
            event.dedupe_key = Some(dedupe_key(&bucket.request, &session, &bucket.id, &mut claimed));
            event.source = file.key();
            events.push(event);
        }
        events
    }
}

/// The vendor's own per-call id when there is one, so a call the IDE also wrote to
/// its cache db lands on the same key and is indexed once.
///
/// Otherwise the pre-existing `<session>#<message.id>`: the indexer's dedupe index
/// is global (`event_dedupe` has no tool or session column), so a bare
/// `chatcmpl-…` id could collide with another session's and silently drop this
/// event, and `request_id` is only *measured* unique here — 7 372 of 7 372 records.
/// A second turn in one window claiming an id already used is pushed back onto the
/// session-scoped key instead of being folded away by the unique index.
fn dedupe_key(
    request: &Option<String>,
    session: &str,
    message: &str,
    claimed: &mut HashSet<String>,
) -> String {
    if let Some(request) = request {
        let key = format!("{DEDUPE_PREFIX}{request}");
        if claimed.insert(key.clone()) {
            return key;
        }
    }
    format!("{session}#{message}")
}

fn text<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn session_fallback(file: &SourceFile) -> String {
    // `<root>/projects/<workspace>/<session>.jsonl` already names the session.
    file.path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_string()
}

fn project_fallback(file: &SourceFile) -> Option<String> {
    let seg = crate::paths::project_segment(&file.path).and_then(OsStr::to_str)?;
    // The dir encodes the workspace with `/` and `.` both collapsed to `-`, which
    // is lossy for names containing dashes; enough for a label. `cwd` is present
    // on every real Qoder record, so this only runs on hand-written logs.
    Some(if seg.starts_with('-') {
        seg.replace("--", "/.").replace('-', "/")
    } else {
        seg.to_string()
    })
}

/// Credits only. Qoder's gateway does the costing server-side and writes zeros
/// into every token field, so there is nothing to pass through: a token number
/// here would either be 0 or invented from `context_usage_ratio`, and once it sat
/// in `TokenCounts` the pricing layer would multiply it by a USD rate.
fn counts_from(usage: &Value) -> TokenCounts {
    TokenCounts {
        credits: number(usage.get("credits")),
        ..TokenCounts::default()
    }
}

/// A value we cannot read (stringified, negative, NaN) is a zero, not a reason to
/// throw away the turn.
fn number(value: Option<&Value>) -> f64 {
    value
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        })
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn only_credits_survives_the_normalisation() {
        // A record that does claim tokens is still credit-metered: the numbers
        // Qoder never billed must not reach the token meter.
        let usage = json(
            r#"{"input_tokens":9000,"output_tokens":400,"cache_read_input_tokens":40000,
                "cache_creation_input_tokens":1200,"credits":0.2774696428571428,
                "original_credits":0.55,"billable":false,"context_usage_ratio":0.16}"#,
        );
        let counts = counts_from(&usage);
        assert_eq!(counts.credits, 0.2774696428571428);
        assert_eq!(counts.total(), 0.0);
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
    }

    #[test]
    fn unreadable_credits_values_are_zero_not_fatal() {
        assert!(counts_from(&json(r#"{"credits":-3}"#)).is_zero());
        assert_eq!(counts_from(&json(r#"{"credits":"0.4"}"#)).credits, 0.4);
        assert!(counts_from(&json(r#"{"credits":"n/a"}"#)).is_zero());
        assert!(counts_from(&json("{}")).is_zero());
    }

    #[test]
    fn a_zero_credit_record_produces_no_event() {
        let file = source_file("/h/.qoder/projects/-Users-demo-proj/sess-1.jsonl");
        let mut turns = Turns::default();
        // Streamed content blocks carry no `usage` object at all.
        turns.consume(&json(r#"{"type":"assistant","sessionId":"sess-1","message":{"id":"chatcmpl-a","model":"qfmodel","content":[{"type":"text","text":"hi"}]}}"#));
        turns.consume(&json(r#"{"type":"assistant","sessionId":"sess-1","message":{"id":"chatcmpl-b","model":"qfmodel","usage":{"credits":0,"input_tokens":0,"billable":false}}}"#));
        assert!(turns.finish(&file).is_empty());
    }

    #[test]
    fn streamed_repeats_merge_into_one_credits_snapshot() {
        let file = source_file("/h/.qoder/projects/-Users-demo-proj/sess-1.jsonl");
        let mut turns = Turns::default();
        for credits in [r#"0.1"#, r#"0.9"#, r#"0.9"#] {
            turns.consume(&json(&format!(
                r#"{{"type":"assistant","sessionId":"sess-1","message":{{"id":"chatcmpl-a","usage":{{"credits":{credits}}}}}}}"#
            )));
        }
        let events = turns.finish(&file);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].counts.credits, 0.9);
        // No `request_id` anywhere, so the pre-existing session-scoped key stands.
        assert_eq!(events[0].dedupe_key.as_deref(), Some("sess-1#chatcmpl-a"));
    }

    #[test]
    fn a_request_id_is_the_key_so_a_call_in_both_stores_indexes_once() {
        let file = source_file("/h/.qoder/projects/-Users-demo-proj/sess-1.jsonl");
        let mut turns = Turns::default();
        // Three lines, one call: the streamed block has no `usage`, so the id has to
        // come from a record that does, and the merge key is still the message id.
        for line in [
            r#"{"type":"assistant","sessionId":"sess-1","message":{"id":"chatcmpl-a","content":[{"type":"text"}]}}"#,
            r#"{"type":"assistant","sessionId":"sess-1","message":{"id":"chatcmpl-a","usage":{"credits":0.4,"request_id":"9a9b1c2d-0000-4000-8000-000000000001"}}}"#,
            r#"{"type":"assistant","sessionId":"sess-1","message":{"id":"chatcmpl-a","usage":{"credits":0.4,"request_id":"9a9b1c2d-0000-4000-8000-000000000001"}}}"#,
        ] {
            turns.consume(&json(line));
        }
        let events = turns.finish(&file);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].dedupe_key.as_deref(),
            Some("qoder#9a9b1c2d-0000-4000-8000-000000000001"),
            "the same string cache_db writes for the same call"
        );
        assert_eq!(events[0].counts.credits, 0.4);
    }

    #[test]
    fn a_reused_request_id_falls_back_rather_than_folding_two_turns() {
        let file = source_file("/h/.qoder/projects/-Users-demo-proj/sess-1.jsonl");
        let mut turns = Turns::default();
        // Two different messages claiming one request_id: never seen in 7 372 real
        // records, and the unique index would silently drop the second.
        for (id, credits) in [("chatcmpl-a", 0.4), ("chatcmpl-b", 0.6)] {
            turns.consume(&json(&format!(
                r#"{{"type":"assistant","sessionId":"sess-1","message":{{"id":"{id}","usage":{{"credits":{credits},"request_id":"req-x"}}}}}}"#
            )));
        }
        let events = turns.finish(&file);
        assert_eq!(events.len(), 2, "{:?}", events.iter().map(|e| &e.dedupe_key).collect::<Vec<_>>());
        assert_eq!(events[0].dedupe_key.as_deref(), Some("qoder#req-x"));
        assert_eq!(events[1].dedupe_key.as_deref(), Some("sess-1#chatcmpl-b"));
    }

    fn source_file(path: &str) -> SourceFile {
        SourceFile {
            path: std::path::PathBuf::from(path),
            kind: usage_core::FileKind::Jsonl,
            size: 1,
            mtime_ms: 1,
        }
    }
}

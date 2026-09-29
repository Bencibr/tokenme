//! `<sessions>/<mangled-cwd>/<ISO>_<uuid>.jsonl` → normalised [`usage_core::UsageEvent`]s.
//!
//! Record shapes on this machine (117 files, 35 363 records):
//! - `session` `{version: 3, id, timestamp, cwd}` — line 1, in all 117 files.
//! - `message` `{id, parentId, timestamp, message:{role, content, api, provider,
//!   model, usage, stopReason, ...}}`.
//! - `model_change` / `thinking_level_change` / `compaction` — no billable usage.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

use serde_json::Value;
use usage_core::{
    parse_ts_ms, Error, Meter, ReadCursor, ReadOutcome, SourceFile, TokenCounts, UsageEvent,
};

use crate::paths;
use crate::paths::Product;

/// Cheap pre-filter: the only lines that can matter are the header and the
/// messages carrying usage, and both are recognisable without building a DOM.
const MARKS: [&str; 2] = ["\"usage\"", "\"session\""];

/// Read everything appended after `cursor`, stopping before a line that is
/// still being written.
///
/// The record decoding is the same for every product that writes this shape;
/// `product` only decides the tool id stamped onto the events, so a sibling's
/// spend is never billed to Pi.
pub(crate) fn read_file(file: &SourceFile, cursor: ReadCursor, product: &Product) -> Result<ReadOutcome, Error> {
    let Some(tail) = read_tail(file, cursor.0)? else {
        return Ok(untouched(cursor));
    };
    let Some(last) = tail.iter().rposition(|b| *b == b'\n') else {
        // No complete line in the window: a record still being flushed.
        return Ok(untouched(cursor));
    };
    let processed = last + 1;
    let mut window = Window::default();
    for raw in tail[..processed].split(|b| *b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        // Invalid UTF-8 means a torn write, same class as a JSON parse failure.
        let Ok(text) = std::str::from_utf8(raw) else {
            continue;
        };
        let text = text.strip_suffix('\r').unwrap_or(text);
        if !MARKS.iter().any(|mark| text.contains(mark)) {
            continue;
        }
        let Ok(line) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        window.consume(&line);
    }
    // The cursor moves by the bytes *processed*, whether or not they yielded an
    // event: a window of user and tool records has still been ingested.
    Ok(ReadOutcome {
        events: window.finish(file, product),
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

/// The `session` header, i.e. the labels every event in one file shares.
#[derive(Default)]
struct Header {
    session: Option<String>,
    cwd: Option<String>,
    version: Option<String>,
    ts_ms: Option<i64>,
}

/// The header is line 1, so by the time an append is read it usually sits behind
/// the cursor. Re-reading a few hundred bytes keeps `session` and `project`
/// identical for every event in a file instead of letting later turns fall back
/// to the lossy directory label.
fn read_header(path: &Path) -> Header {
    let mut header = Header::default();
    let Ok(mut handle) = File::open(path) else {
        return header;
    };
    let mut buf = Vec::with_capacity(4096);
    if handle.by_ref().take(4096).read_to_end(&mut buf).is_err() {
        return header;
    }
    for line in lines(&buf) {
        if line.get("type").and_then(Value::as_str) != Some("session") {
            continue;
        }
        header.session = owned(text_of(&line, "id"));
        header.cwd = owned(text_of(&line, "cwd"));
        header.ts_ms = ts_of(&line, "timestamp");
        header.version = line.get("version").map(|v| format!("session format v{v}"));
        break;
    }
    header
}

/// Parse each complete line of a buffer, skipping the torn tail and anything
/// that is not JSON.
fn lines(buf: &[u8]) -> Vec<Value> {
    let Some(last) = buf.iter().rposition(|b| *b == b'\n') else {
        return Vec::new();
    };
    buf[..=last]
        .split(|b| *b == b'\n')
        .filter_map(|raw| std::str::from_utf8(raw).ok())
        .filter_map(|text| text.strip_suffix('\r').or(Some(text)))
        .filter(|text| !text.is_empty())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .collect()
}

/// One assistant message, keyed by its record id. Fields are owned so a parsed
/// line can be dropped at once: holding every `Value` of a 14 MB log alive until
/// the end of the window would cost more than the log itself.
#[derive(Default)]
struct Bucket {
    record_id: Option<String>,
    ts_ms: Option<i64>,
    model: Option<String>,
    counts: TokenCounts,
}

#[derive(Default)]
struct Window {
    /// Keys in file order, so events leave in the order the log recorded them.
    order: Vec<String>,
    turns: HashMap<String, Bucket>,
    header: Header,
}

impl Window {
    fn consume(&mut self, line: &Value) {
        match line.get("type").and_then(Value::as_str) {
            Some("session") => {
                if self.header.session.is_none() {
                    self.header.session = owned(text_of(line, "id"));
                }
                if self.header.cwd.is_none() {
                    self.header.cwd = owned(text_of(line, "cwd"));
                }
                if self.header.ts_ms.is_none() {
                    self.header.ts_ms = ts_of(line, "timestamp");
                }
                if self.header.version.is_none() {
                    self.header.version =
                        line.get("version").map(|v| format!("session format v{v}"));
                }
            }
            Some("message") => self.assistant(line),
            // model_change, thinking_level_change, compaction: Pi bills the
            // compaction summary as an ordinary assistant message, so these
            // records carry no usage of their own.
            _ => {}
        }
    }

    fn assistant(&mut self, line: &Value) {
        let Some(message) = line.get("message").filter(|m| m.is_object()) else {
            return;
        };
        // Only assistant messages are billed. A `toolResult` record can carry a
        // nested `usage` in Pi's schema (0 of 17 590 here), but the sub-agent's
        // own session file bills those calls, so counting them twice would
        // invent spend.
        if text_of(message, "role") != Some("assistant") {
            return;
        }
        let record_id = text_of(line, "id").map(str::to_string);
        // Record ids are unique inside a file and 8 hex chars wide, so they can
        // collide across sessions; the `<session>#<id>` prefix is what keeps the
        // indexer's global UNIQUE index from silently dropping a turn.
        let key = record_id
            .clone()
            .unwrap_or_else(|| format!("\0anon{}", self.order.len()));
        let fresh = !self.turns.contains_key(&key);
        if fresh {
            let ts_ms = ts_of(line, "timestamp").or_else(|| ts_of(message, "timestamp"));
            self.order.push(key.clone());
            self.turns.insert(
                key.clone(),
                Bucket {
                    record_id,
                    ts_ms,
                    ..Bucket::default()
                },
            );
        }
        let Some(bucket) = self.turns.get_mut(&key) else {
            return;
        };
        if bucket.model.is_none() {
            // Router aliases (`9dev`, `ox`, `deepseek-v4-pro:0813`) stay verbatim:
            // an unknown id surfaces as unpriced, while substituting a plausible
            // vendor model would bill spend the source never ran.
            bucket.model = owned(text_of(message, "model"));
        }
        let Some(usage) = message.get("usage").filter(|u| u.is_object()) else {
            return;
        };
        let counts = counts_from(usage);
        // Pi persists one terminal assistant message per API call, so a repeated
        // id does not occur in the data. If a re-write ever lands twice it is the
        // same call growing, hence largest snapshot wins and a sum never does.
        if counts.total() > bucket.counts.total() {
            bucket.counts = counts;
        }
    }

    fn finish(mut self, file: &SourceFile, product: &Product) -> Vec<UsageEvent> {
        if self.turns.is_empty() {
            return Vec::new();
        }
        if self.header.session.is_none() || self.header.cwd.is_none() {
            let head = read_header(&file.path);
            let Header {
                session,
                cwd,
                version: _,
                ts_ms,
            } = head;
            self.header.session = self.header.session.or(session);
            self.header.cwd = self.header.cwd.or(cwd);
            self.header.ts_ms = self.header.ts_ms.or(ts_ms);
        }
        let session = self
            .header
            .session
            .clone()
            .unwrap_or_else(|| paths::session_from_path(&file.path));
        let project = self
            .header
            .cwd
            .clone()
            .or_else(|| paths::project_from_path(&file.path));
        let mut events = Vec::with_capacity(self.order.len());
        for key in std::mem::take(&mut self.order) {
            // `key` is the record id, or the anon sentinel for a record without
            // one; both were inserted, so the remove always hits.
            let Some(bucket) = self.turns.remove(&key) else {
                continue;
            };
            // A turn that reported zeros (aborted request, error stub) has
            // nothing to bill and would only inflate the totals.
            if bucket.counts.is_zero() {
                continue;
            }
            let mut event = UsageEvent::new(
                product.id,
                // Never 0: an undated record falls back to the session header,
                // then to the file's mtime, so it cannot invent a 1970 bucket.
                bucket.ts_ms.or(self.header.ts_ms).unwrap_or(file.mtime_ms),
                session.clone(),
            );
            event.project = project.clone();
            event.model = bucket.model;
            event.counts = bucket.counts;
            event.meter = Meter::Tokens;
            // A record without an id cannot be deduped by key; the byte cursor
            // plus the indexer's purge-on-shrink keep such a file idempotent.
            event.dedupe_key = bucket.record_id.map(|id| format!("{session}#{id}"));
            // Pi's tool calls are all built-ins (`bash`, `read`, `edit`,
            // `write`, …): no MCP or Skill extension to attribute.
            event.calls = Vec::new();
            event.source = file.key();
            events.push(event);
        }
        events
    }
}

/// `probe`'s hint: the `version` of the session file it short-circuited on.
pub(crate) fn head_version(path: &Path) -> Option<String> {
    read_header(path).version
}

fn owned(value: Option<&str>) -> Option<String> {
    Some(value?.to_string())
}

fn text_of<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Pi's dialects: a record's `timestamp` is RFC 3339 with `Z`, while the nested
/// `message.timestamp` is a unix number in seconds or milliseconds.
fn ts_of(value: &Value, key: &str) -> Option<i64> {
    let raw = value.get(key)?;
    if let Some(text) = raw.as_str() {
        return parse_ts_ms(text);
    }
    let n = raw.as_f64()?.floor();
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let n = n as i64;
    // 10 digits are seconds, 13 are milliseconds — same rule as `parse_ts_ms`.
    Some(if n < 10_000_000_000 { n * 1000 } else { n })
}

/// Pi names its stages differently from the Anthropic API and splits cache
/// writes into a 1-hour tier, so this mapping is the whole value of the adapter.
fn counts_from(usage: &Value) -> TokenCounts {
    let num = |row: &Value, key: &str| row.get(key).map(number).unwrap_or(0.0);
    TokenCounts {
        // `input` is the *uncached* remainder: `totalTokens == input + output +
        // cacheRead + cacheWrite` holds for all 17 009 usage records here, which
        // is only possible if `cacheRead` is excluded. Subtracting it the way the
        // Anthropic shape needs would drop most of every prompt.
        input: num(usage, "input"),
        // `cacheWrite1h` is the 1-hour tier *inside* `cacheWrite`, not extra work:
        // it never exceeds `cacheWrite` (3501/3501 records carrying both), it is
        // absent from the `totalTokens` identity, and Pi's own `cost` object has
        // no `cacheWrite1h` line. Adding it would double-count 1-hour writes.
        cache_creation: num(usage, "cacheWrite"),
        cache_read: num(usage, "cacheRead"),
        output: num(usage, "output"),
        // Thinking tokens are a sub-breakdown of `output` (`reasoning` exceeded it
        // in 0 of 12 687 records), never added on top.
        reasoning: num(usage, "reasoning"),
        // Pi's self-reported `cost.*` is deliberately dropped: money is computed
        // centrally from the shared price table so every tool stays comparable,
        // and a router alias like `9dev` prices itself at exactly 0 here.
        credits: 0.0,
    }
}

/// Third-party proxies stringify numbers now and then; a value we cannot read is
/// a zero, not a reason to throw away the turn.
fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
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
    fn cache_write_is_counted_once_and_pi_cost_is_ignored() {
        // Shape taken from a real 9router record, with the cacheWrite numbers the
        // local logs never exercised filled in.
        let counts = counts_from(&json(
            r#"{"input":18955,"output":683,"cacheRead":25088,"cacheWrite":5000,
                "cacheWrite1h":5000,"totalTokens":49726,
                "cost":{"input":1.2,"output":0.3,"cacheRead":0.04,"cacheWrite":0.5,"total":2.04}}"#,
        ));
        assert_eq!(
            (
                counts.input,
                counts.cache_creation,
                counts.cache_read,
                counts.output
            ),
            (18955.0, 5000.0, 25088.0, 683.0)
        );
        // The decomposition is faithful: our total is Pi's own `totalTokens`.
        assert_eq!(counts.total(), 49_726.0);
        assert_eq!(counts.reasoning, 0.0);
        assert_eq!(counts.credits, 0.0, "Pi's self-reported cost must not leak");
    }

    #[test]
    fn reasoning_is_reported_but_never_added_to_output() {
        let counts = counts_from(&json(
            r#"{"input":7616,"output":262,"cacheRead":1152,"cacheWrite":0,"reasoning":153,
                "totalTokens":9030,"cost":{}}"#,
        ));
        assert_eq!(counts.reasoning, 153.0);
        assert_eq!(counts.total(), 9030.0);
    }

    #[test]
    fn unreadable_usage_values_do_not_lose_the_turn() {
        assert_eq!(
            counts_from(&json(r#"{"input":"7","output":-3}"#)).total(),
            7.0
        );
        assert!(counts_from(&json("{}")).is_zero());
    }

    #[test]
    fn both_timestamp_dialects_resolve() {
        assert_eq!(
            ts_of(
                &json(r#"{"timestamp":"2026-09-22T14:16:39.708Z"}"#),
                "timestamp"
            ),
            parse_ts_ms("2026-09-22T14:16:39.708Z")
        );
        assert_eq!(
            ts_of(&json(r#"{"timestamp":1758550615123}"#), "timestamp"),
            Some(1758550615123)
        );
        assert_eq!(
            ts_of(&json(r#"{"timestamp":1758550615}"#), "timestamp"),
            Some(1758550615000)
        );
        assert_eq!(ts_of(&json(r#"{"timestamp":0}"#), "timestamp"), None);
        assert_eq!(ts_of(&json(r#"{"timestamp":"nope"}"#), "timestamp"), None);
    }

    #[test]
    fn a_record_without_any_timestamp_never_lands_in_1970() {
        let mut window = Window::default();
        window.consume(&json(
            r#"{"type":"message","id":"a1","message":{"role":"assistant","model":"9dev",
                "usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":11,"cost":{}}}}"#,
        ));
        let file = source_file(
            "/h/.pi/agent/sessions/--Users-demo-proj--/2026-01-01T00-00-00-000Z_uuid-1.jsonl",
        );
        let events = window.finish(&file, &paths::PI);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ts_ms, file.mtime_ms);
        assert_eq!(events[0].session, "uuid-1");
        assert_eq!(events[0].project.as_deref(), Some("/Users/demo/proj"));
        assert_eq!(events[0].dedupe_key.as_deref(), Some("uuid-1#a1"));
    }

    #[test]
    fn zero_usage_turns_are_dropped_and_an_id_less_turn_keeps_counting() {
        let mut window = Window::default();
        window.consume(&json(
            r#"{"type":"message","id":"aborted","timestamp":"2026-09-22T14:16:39.708Z",
                "message":{"role":"assistant","model":"9dev","stopReason":"aborted",
                "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{}}}}"#,
        ));
        window.consume(&json(
            r#"{"type":"message","timestamp":"2026-09-22T14:16:40.708Z",
                "message":{"role":"assistant","model":"9dev",
                "usage":{"input":3,"output":4,"cacheRead":0,"cacheWrite":0,"totalTokens":7,"cost":{}}}}"#,
        ));
        let file = source_file("/h/.pi/agent/sessions/--Users-demo-proj--/x_uuid-2.jsonl");
        let events = window.finish(&file, &paths::PI);
        assert_eq!(events.len(), 1, "the aborted turn has nothing to bill");
        assert_eq!(
            events[0].dedupe_key, None,
            "a record with no id cannot dedupe by key"
        );
    }

    #[test]
    fn non_billable_record_shapes_are_ignored() {
        let mut window = Window::default();
        for line in [
            r#"{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-09-22T14:16:39.708Z","provider":"9router","modelId":"9dev"}"#,
            r#"{"type":"thinking_level_change","id":"t1","thinkingLevel":"high"}"#,
            r#"{"type":"compaction","id":"c1","tokensBefore":50000,"summary":"..."}"#,
            r#"{"type":"message","id":"u1","message":{"role":"user","content":[{"type":"text","text":"/exit"}]}}"#,
            r#"{"type":"message","id":"r1","message":{"role":"toolResult","toolName":"bash","content":[],"isError":false}}"#,
        ] {
            window.consume(&json(line));
        }
        let file = source_file("/h/.pi/agent/sessions/--Users-demo-proj--/x_uuid-3.jsonl");
        assert!(window.finish(&file, &paths::PI).is_empty());
    }

    #[test]
    fn lines_skips_a_torn_tail_and_garbage() {
        let buf = b"{\"type\":\"session\"}\nnot json\n{\"type\":\"message\"}\n{partial";
        assert_eq!(lines(buf).len(), 2);
        assert!(lines(b"no newline at all").is_empty());
    }

    #[test]
    fn head_version_reads_the_session_record_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("2026-09-22T14-16-39-708Z_uuid.jsonl");
        assert_eq!(head_version(&log), None, "an absent file has no version");
        std::fs::write(
            &log,
            "{\"type\":\"session\",\"version\":3,\"id\":\"uuid\",\"cwd\":\"/x\"}\n{\"type\":\"message\"}\n",
        )
        .unwrap();
        assert_eq!(head_version(&log).as_deref(), Some("session format v3"));
    }

    fn source_file(path: &str) -> SourceFile {
        SourceFile {
            path: std::path::PathBuf::from(path),
            kind: usage_core::FileKind::Jsonl,
            size: 1,
            mtime_ms: 1_758_550_615_000,
        }
    }
}

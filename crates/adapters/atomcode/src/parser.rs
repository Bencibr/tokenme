//! `$ATOMCODE_HOME/sessions/<project_hash>/<id>.jsonl` → normalised
//! [`usage_core::UsageEvent`]s.
//!
//! Verified against the real files on this machine (20 transcripts in 9 project
//! buckets, 64 usage-bearing records) **and** against the writer, AtomCode
//! 5.1.0's MIT source (`atomgit_atomcode/atomcode` @ e4215f7, cloned to
//! `/tmp/acsrc/atomcode` while this adapter was written). Every claim below
//! cites one of the two.
//!
//! ## Record shape (transcript.rs:25-59, `TurnRecord`)
//!
//! One line per *completed turn*, flushed exactly once at the turn's terminal:
//! "appends ONE raw record per completed turn to `<id>.jsonl`, the
//! never-compacted ground truth" (`crates/atomcode-capabilities/src/session/
//! transcript.rs:1-8`). Fields: `v` (schema, `1` in 64/64), `ts`, `iso`,
//! `session_id` (present in 64/64, equal to the file stem), `turn_id`, `undone`
//! (`false` in 64/64), `user`, `assistant`, `reasoning` (prose, 60/64),
//! `tools[]` and a **top-level** `usage` — not nested under `message` the way
//! Claude's is.
//!
//! ## (a) `cached` is a SUBSET of `prompt`, so `prompt` is an inclusive total
//!
//! Settled by the provider adapters that fill it, not by guessing:
//! * `crates/atomcode-capabilities/src/provider/openai_compat.rs:1786-1802`,
//!   whose own comment reads "`prompt` is total input and `cached` is the
//!   cache-read subset": `prompt: u.prompt_tokens.unwrap_or_else(...)`,
//!   `cached` scraped from `prompt_cache_hit_tokens` / `cached_tokens` /
//!   `prompt_tokens_details.cached_tokens`.
//! * `crates/atomcode-capabilities/src/provider/anthropic.rs:879-886`:
//!   `prompt: self.input_tokens + self.cache_read + self.cache_creation`,
//!   `cached: self.cache_read`. The cache write is folded *into* `prompt` and
//!   no field survives to say so.
//!
//! The local rows agree: measured `prompt` 7 088 400, `completion` 372 734,
//! `cached` 6 919 936. `cached` is a multiple of 128 in 64/64 rows (a
//! block-aligned cache-hit counter) while `prompt` is a multiple of 128 in 0/64
//! (a real token count), and `prompt - cached` behaves exactly like net input:
//! positive for 63/64 rows, summing to 168 464 raw — 170 999 once the single
//! overrun below is clamped — i.e. a 97.6 % hit rate over a 7.09 M-token prompt
//! side, which is what a 200 k-window re-sent prefix looks like. An *additive*
//! `cached` would instead put 14 008 336 prompt tokens
//! through a window the source itself records as 200 000 (`<id>.meta`
//! `turn_stats[].ctx_window`) — and its `used_tokens` equals this row's
//! `prompt` in 8/8 turns where both exist, so `prompt` *is* the occupancy.
//!
//! Hence `input = prompt - cached` (clamped at 0, the Codex convention),
//! `cache_read = cached`, `output = completion`.
//!
//! Two consequences of that mapping, both from `transcript.rs:251-255` (the
//! turn buffer takes `prompt` from the LAST round but `cached` as the MAX across
//! rounds, and *sums* `completion`): `cache_creation` is necessarily 0 (nothing
//! to put it in), and one row (`b7954235…` turn 2) reports `cached` 102 400 >
//! `prompt` 99 865 — the max-round cache against the final-round prompt, over by
//! 2 535. The clamp keeps that row out of `input`; AtomCode's own web UI clamps
//! the same ratio with `Math.min(100, cached/prompt*100)`.
//!
//! ## (b) `ts` is epoch MILLISECONDS; `iso` is its RFC 3339 mirror
//!
//! `transcript.rs:33-38`: "epoch MILLISECONDS, UTC — stamped by L1 at flush" and
//! "Human-readable RFC-3339 mirror of `ts`". Local: `ts` is 13 digits in 64/64
//! rows and re-derives `iso` (`2026-07-18T08:01:17.893+00:00`, `+00:00` in
//! 64/64) within 1 ms in 64/64. Both dialects are accepted, `ts` first; a bare
//! 10-digit value is still read as seconds, and an undated record falls back to
//! the file's mtime rather than landing in 1970. `started_at` (added after v1,
//! `transcript.rs:29-32`) is absent from all 64 rows and names the prompt's
//! arrival rather than the record's flush, so it is not read.
//!
//! ## (c) `turn_id` is per-session and NOT unique per billed call
//!
//! `transcript.rs:39-41`: "`Kernel TurnCtx.turn_id` (monotonic within the
//! session)". Locally there are only 33 distinct `turn_id` values across 64
//! rows (a bare `turn_id` repeats up to 20 times across sessions), so the
//! session prefix is mandatory. Even *inside* one session it collides: `<id>.
//! jsonl` for `b7954235…` holds `turn_id` 4 twice, 62 s apart, with different
//! `user` prompts, 13 vs 5 tool calls and `prompt` 129 996 vs 124 747. Those are
//! two genuinely distinct turns, not a streaming snapshot growing (a snapshot
//! would never *shrink* `prompt`), and both were billed. So the key is
//! `session_id#turn_id#ts` — unique for 64/64 rows — and same-key repeats still
//! merge by keeping the largest snapshot, which makes a duplicated line a no-op.
//!
//! ## Why `FileKind::Jsonl` (append-only) rather than `Tree`
//!
//! `transcript.rs:44-48` documents `undone` as a *reserved* flag precisely
//! because marking a rewound turn "needs a side index or a rewrite pass" and the
//! jsonl is "append-only"; the rewrite is deferred, and `undone` is `false` in
//! 64/64 rows here. The files behave the same way: rows leave in `ts` order in
//! 20/20 files, and each file's mtime minus its last row's `ts` is 0 or 1 ms in
//! 20/20, i.e. nothing touches a transcript after the turn lands. The whole-state
//! rewrite lives in the sibling `<id>.snapshot`, which this adapter never reads.
//!
//! ## Model: the transcript names none
//!
//! No `model` key on any of the 64 records (`TurnRecord` has no such field,
//! `transcript.rs:25-59`); AtomCode keeps `provider_model` on the *snapshot's*
//! per-message `meta` instead, and a session's model comes from the config that
//! was active at flush time (`~/.atomcode/config.toml`, `default_model`). That
//! join is rejected — the snapshot is a rewritten whole-state file whose messages
//! a rewind can drop, and a machine-level default would bill models the session
//! never ran — so `event.model` stays `None` and [`crate::AtomCodeAdapter`]
//! reports `ModelAttr::Unknown`. An additive `model`/`provider_model` key on the
//! record itself would be picked up unchanged if a future `v` ever adds one.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

use serde_json::Value;
use usage_core::{
    parse_ts_ms, Call, CallKind, Error, Meter, ReadCursor, ReadOutcome, SourceFile, TokenCounts,
    UsageEvent,
};

use crate::paths;
use crate::TOOL_ID;

/// Cheap pre-filter: `usage` is a required field of every record worth a line,
/// and no other line in the file carries the substring.
const MARKS: [&str; 1] = ["\"usage\""];

/// Read everything appended after `cursor`, stopping before a line that is
/// still being written.
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
        if !MARKS.iter().any(|mark| text.contains(mark)) {
            continue;
        }
        let Ok(line) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        turns.consume(&line);
    }
    // The cursor moves by the bytes *processed*, whether or not they yielded an
    // event: a turn that reported zeros has still been ingested.
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

/// One turn's `TurnRecord`, in file order. Fields are owned so a parsed line can
/// be dropped at once: a 200 k-token turn carries its full raw prompt and reply
/// text, and holding every `Value` of an 857 KB transcript until the end of the
/// window would cost more than the transcript itself.
#[derive(Default)]
struct Bucket {
    /// `session_id#turn_id#ts`, or `None` for a record too incomplete to name.
    id: Option<String>,
    session: Option<String>,
    ts_ms: Option<i64>,
    model: Option<String>,
    counts: TokenCounts,
    calls: Vec<Call>,
}

#[derive(Default)]
struct Turns {
    /// Keys in file order, so events leave in the order the log recorded them.
    order: Vec<String>,
    turns: HashMap<String, Bucket>,
}

impl Turns {
    fn consume(&mut self, line: &Value) {
        // A `TurnRecord`'s `usage` is a required field of the struct, so a line
        // without one is not a transcript record this adapter understands.
        let Some(usage) = line.get("usage").filter(|u| u.is_object()) else {
            return;
        };
        let session = text(line, "session_id").map(str::to_string);
        // `ts` first (required, ms), then `iso`, its RFC 3339 mirror: a record
        // from a build that only ever wrote one of them still dates correctly.
        let ts_ms = ts_of(line, "ts").or_else(|| ts_of(line, "iso"));
        let turn_id = line.get("turn_id").and_then(Value::as_u64);
        // `turn_id` alone is not an identity — it is per-session and re-used
        // (see the module doc) — so the flush time is what makes the key unique.
        let named = match (&session, turn_id, ts_ms) {
            (Some(sid), Some(turn), Some(ts)) => Some(format!("{sid}#{turn}#{ts}")),
            _ => None,
        };
        let key = named
            .clone()
            .unwrap_or_else(|| format!("\0anon{}", self.order.len()));
        let fresh = !self.turns.contains_key(&key);
        if fresh {
            self.order.push(key.clone());
            self.turns.insert(
                key.clone(),
                Bucket {
                    id: named,
                    session,
                    ts_ms,
                    ..Bucket::default()
                },
            );
        }
        let Some(bucket) = self.turns.get_mut(&key) else {
            return;
        };
        // Absent in every record today; kept so a future additive `v` needs no
        // parser change to start labelling its own turns.
        if bucket.model.is_none() {
            bucket.model = owned(text(line, "model").or_else(|| text(line, "provider_model")));
        }
        let counts = counts_from(usage);
        // The same key twice means one flush written twice, so the later line can
        // only be a growing snapshot of the same turn: keep the largest, never
        // sum, which would bill the tail of the turn a second time.
        if counts.total() > bucket.counts.total() {
            bucket.counts = counts;
        }
        tool_calls(line, &mut bucket.calls);
    }

    fn finish(self, file: &SourceFile) -> Vec<UsageEvent> {
        if self.turns.is_empty() {
            return Vec::new();
        }
        // The workspace label is not on the record at all: `working_dir` of the
        // sibling `<id>.meta`, read once per window. The project bucket directory
        // is a one-way hash and names nothing.
        let project = paths::working_dir(&file.path);
        let mut events = Vec::with_capacity(self.order.len());
        for key in self.order {
            // `key` is the composed id or the anon sentinel; both were inserted.
            let Some(bucket) = self.turns.get(&key) else {
                continue;
            };
            // A turn that reported zeros (error stub, cancelled before the first
            // token) has nothing to bill and would only inflate the totals.
            if bucket.counts.is_zero() {
                continue;
            }
            let session = bucket
                .session
                .clone()
                .unwrap_or_else(|| paths::session_from_path(&file.path));
            let mut event = UsageEvent::new(
                TOOL_ID,
                // Never 0: an undated record falls back to the file's mtime, so it
                // cannot invent a 1970 bucket.
                bucket.ts_ms.unwrap_or(file.mtime_ms),
                session,
            );
            event.project = project.clone();
            event.model = bucket.model.clone();
            event.counts = bucket.counts;
            event.meter = Meter::Tokens;
            // A record with no `session_id`/`turn_id`/`ts` cannot be named; the
            // byte cursor plus the indexer's purge-on-shrink keep such a file
            // idempotent instead.
            event.dedupe_key = bucket.id.clone();
            event.calls = bucket.calls.clone();
            event.source = file.key();
            events.push(event);
        }
        events
    }
}

/// `probe`'s hint: the `v` of the `.jsonl` record it short-circuited on.
pub(crate) fn head_version(path: &Path) -> Option<String> {
    let mut handle = File::open(path).ok()?;
    let mut buf = Vec::with_capacity(4096);
    if handle.by_ref().take(4096).read_to_end(&mut buf).is_err() {
        return None;
    }
    if let Some(version) = lines(&buf)
        .into_iter()
        .find(|line| line.get("usage").is_some())
        .and_then(|record| record.get("v").and_then(Value::as_u64))
    {
        return Some(format!("turn record v{version}"));
    }
    // No complete line in the window: a real turn's raw prompt and reply run to
    // tens of kilobytes. `TurnRecord` serialises `v` first, so the digits at the
    // head of the file still answer without reading the record whole.
    let text = String::from_utf8_lossy(&buf);
    let (_, rest) = text.split_once("\"v\":")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!digits.is_empty()).then(|| format!("turn record v{digits}"))
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

fn owned(value: Option<&str>) -> Option<String> {
    Some(value?.to_string())
}

fn text<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// AtomCode's two dialects for the same instant: `ts` is an integer of epoch
/// milliseconds and `iso` is an RFC 3339 mirror of it. A bare number below ten
/// digits is read as seconds, matching `parse_ts_ms`.
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
    Some(if n < 10_000_000_000 { n * 1000 } else { n })
}

/// The stage mapping, i.e. the whole value of this adapter.
///
/// `prompt` is the *inclusive* prompt total and `cached` the subset of it that
/// came out of the provider cache (`openai_compat.rs:1786-1802`,
/// `anthropic.rs:879-886`), so `input` has to be netted down or every cached
/// token is billed twice. `cache_creation` has no field of its own — Anthropic's
/// cache write is folded into `prompt` before the record is written — and
/// `reasoning` on the record is prose, not a token count, so both stay 0.
fn counts_from(usage: &Value) -> TokenCounts {
    let num = |row: &Value, key: &str| row.get(key).map(number).unwrap_or(0.0);
    let prompt = num(usage, "prompt");
    let cached = num(usage, "cached");
    TokenCounts {
        input: (prompt - cached).max(0.0),
        cache_creation: 0.0,
        cache_read: cached,
        output: num(usage, "completion"),
        reasoning: 0.0,
        // AtomCode's own cost math stays out of the pipeline: money is computed
        // centrally from the shared price table so every tool stays comparable.
        credits: 0.0,
    }
}

/// The turn's tool calls. AtomCode's built-ins (`bash`, `read_file`, `edit_file`,
/// `write_file`, `grep`, `glob`, `todowrite`, `web_search`, …) are dropped the
/// way Claude's are: they would swamp the breakdown with activity that is not an
/// extension. 13 distinct names appear across the 64 local records, 11 of them
/// built-ins.
fn tool_calls(line: &Value, out: &mut Vec<Call>) {
    let Some(tools) = line.get("tools").and_then(Value::as_array) else {
        return;
    };
    for tool in tools {
        let Some(name) = text(tool, "name") else {
            continue;
        };
        // MCP tools register as kernel tools named `mcp__{server}__{tool}`
        // (`crates/atomcode-capabilities/src/mcp/mod.rs:3`), same shape as
        // Claude Code's, so the server segment is the attribution.
        let call = if let Some(rest) = name.strip_prefix("mcp__") {
            let Some((server, _)) = rest.split_once("__") else {
                continue;
            };
            Call {
                kind: CallKind::Mcp,
                name: server.to_string(),
            }
        // A skill runs through the `use_skill` tool with its name in the
        // `args` JSON (`crates/atomcode-tuix/src/event_loop/mod.rs:30063`,
        // `"use_skill" => get_str("name")`).
        } else if name == "use_skill" {
            let Some(skill) = text(tool, "args")
                .and_then(|args| serde_json::from_str::<Value>(args).ok())
                .and_then(|args| text(&args, "name").map(str::to_string))
            else {
                continue;
            };
            Call {
                kind: CallKind::Skill,
                name: skill,
            }
        } else {
            continue;
        };
        if !out.contains(&call) {
            out.push(call);
        }
    }
}

/// A value we cannot read as a count is a zero, not a reason to lose the turn.
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
    fn cached_is_split_out_of_an_inclusive_prompt() {
        // Real row: `{prompt: 12061, completion: 235, cached: 10368}`.
        let counts = counts_from(&json(r#"{"prompt":12061,"completion":235,"cached":10368}"#));
        assert_eq!(
            (counts.input, counts.cache_read, counts.output),
            (1693.0, 10368.0, 235.0)
        );
        // Our decomposition still adds up to the prompt the provider charged, plus
        // the completion: AtomCode's own `prompt + completion`.
        assert_eq!(counts.total(), 12_296.0);
        assert_eq!(counts.cache_creation, 0.0, "Anthropic writes cache_creation into prompt");
        assert_eq!(counts.reasoning, 0.0, "the `reasoning` field is prose");
        assert_eq!(counts.credits, 0.0);
    }

    #[test]
    fn a_cache_hit_larger_than_the_prompt_cannot_make_input_negative() {
        // `b7954235…` turn 2: `cached` is the max across the turn's rounds while
        // `prompt` is the last round's, so cached legitimately overruns by 2535.
        let counts = counts_from(&json(r#"{"prompt":99865,"completion":23706,"cached":102400}"#));
        assert_eq!(counts.input, 0.0);
        assert_eq!(counts.cache_read, 102_400.0);
        assert_eq!(counts.total(), 126_106.0);
    }

    #[test]
    fn unreadable_usage_values_do_not_lose_the_turn() {
        assert_eq!(
            counts_from(&json(r#"{"prompt":"7","completion":-3}"#)).total(),
            7.0
        );
        assert!(counts_from(&json("{}")).is_zero());
    }

    #[test]
    fn both_timestamp_dialects_resolve() {
        assert_eq!(
            ts_of(&json(r#"{"ts":1784361677893}"#), "ts"),
            Some(1784361677893)
        );
        assert_eq!(
            ts_of(&json(r#"{"ts":"2026-07-18T08:01:17.893+00:00"}"#), "ts"),
            parse_ts_ms("2026-07-18T08:01:17.893+00:00")
        );
        // A 10-digit `ts` is seconds, not milliseconds: same rule as `parse_ts_ms`.
        assert_eq!(ts_of(&json(r#"{"ts":1784361677}"#), "ts"), Some(1784361677000));
        assert_eq!(ts_of(&json(r#"{"ts":0}"#), "ts"), None);
        assert_eq!(ts_of(&json(r#"{"iso":"nope"}"#), "iso"), None);
    }

    #[test]
    fn the_key_needs_session_turn_and_flush_time() {
        let mut turns = Turns::default();
        turns.consume(&json(
            r#"{"v":1,"ts":1784361677893,"iso":"2026-07-18T08:01:17.893+00:00",
                "session_id":"s-1","turn_id":4,"undone":false,"user":"a","assistant":"b",
                "usage":{"prompt":100,"completion":5,"cached":64}}"#,
        ));
        let file = source_file("/h/.atomcode/sessions/45d7/s-1.jsonl");
        let events = turns.finish(&file);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].dedupe_key.as_deref(), Some("s-1#4#1784361677893"));
        assert_eq!(events[0].session, "s-1");
        assert_eq!(events[0].ts_ms, 1784361677893);
        assert_eq!(events[0].model, None, "the transcript names no model");

        // Same turn id, later flush: a second billed call, not a snapshot.
        let mut turns = Turns::default();
        for ts in [1784356785786_i64, 1784356847788] {
            turns.consume(&json(&format!(
                r#"{{"ts":{ts},"session_id":"s-2","turn_id":4,
                    "usage":{{"prompt":100,"completion":5,"cached":64}}}}"#
            )));
        }
        let events = turns.finish(&file);
        assert_eq!(events.len(), 2, "both turn-4 calls survive");
        assert_eq!(events.iter().map(|e| e.counts.total()).sum::<f64>(), 210.0);
    }

    #[test]
    fn a_repeated_key_keeps_the_largest_snapshot() {
        let mut turns = Turns::default();
        let growing = r#"{"ts":1784361677893,"session_id":"s-3","turn_id":1,
            "usage":{"prompt":1200,"completion":40,"cached":900}}"#;
        let finalised = r#"{"ts":1784361677893,"session_id":"s-3","turn_id":1,
            "usage":{"prompt":9200,"completion":480,"cached":9000}}"#;
        turns.consume(&json(growing));
        turns.consume(&json(finalised));
        turns.consume(&json(growing));
        let events = turns.finish(&source_file("/h/.atomcode/sessions/45d7/s-3.jsonl"));
        assert_eq!(events.len(), 1);
        // 1240 + 9680 would be the answer if the snapshots were summed.
        assert_eq!(events[0].counts.total(), 9_680.0);
    }

    #[test]
    fn zero_usage_and_nameless_records_are_treated_apart() {
        let mut turns = Turns::default();
        // Flushed at an error terminal with nothing billed.
        turns.consume(&json(
            r#"{"ts":1784361677893,"session_id":"s-4","turn_id":1,
                "usage":{"prompt":0,"completion":0,"cached":0}}"#,
        ));
        // Real usage, but no `turn_id` to build an identity from.
        turns.consume(&json(
            r#"{"session_id":"s-4","usage":{"prompt":30,"completion":4,"cached":0}}"#,
        ));
        let events = turns.finish(&source_file("/h/.atomcode/sessions/45d7/s-4.jsonl"));
        assert_eq!(events.len(), 1, "the zero turn has nothing to bill");
        assert_eq!(events[0].dedupe_key, None);
        // Undated: the file's mtime stands in, so nothing lands in 1970.
        assert_eq!(events[0].ts_ms, 1_784_361_677_000);
        assert_eq!(events[0].session, "s-4", "the file stem labels it instead");
    }

    #[test]
    fn only_extensions_are_recorded_as_calls() {
        let mut turns = Turns::default();
        turns.consume(&json(
            r#"{"ts":1784361677893,"session_id":"s-5","turn_id":1,
                "tools":[
                    {"name":"bash","args":"{}","result":"","is_error":false},
                    {"name":"mcp__bugx__run","args":"{}","result":"","is_error":false},
                    {"name":"use_skill","args":"{\"name\":\"atomcode:ask\",\"arguments\":\"q\"}","result":"","is_error":false},
                    {"name":"use_skill","args":"not json","result":"","is_error":true},
                    {"name":"mcp__broken","args":"{}","result":"","is_error":false}],
                "usage":{"prompt":10,"completion":1,"cached":0}}"#,
        ));
        let events = turns.finish(&source_file("/h/.atomcode/sessions/45d7/s-5.jsonl"));
        assert_eq!(events[0].calls.len(), 2, "built-ins and malformed args stay out");
        assert_eq!(events[0].calls[0].name, "bugx");
        assert_eq!(events[0].calls[1].name, "atomcode:ask");
        assert_eq!(events[0].calls[1].kind, CallKind::Skill);
    }

    #[test]
    fn a_foreign_shape_without_usage_produces_nothing() {
        let mut turns = Turns::default();
        for line in [
            r#"{"role":"System","text":"You are AtomCode"}"#,
            r#"{"v":1,"ts":1784361677893,"session_id":"s-6","turn_id":1}"#,
            r#"{"usage":"a string, not an object"}"#,
        ] {
            turns.consume(&json(line));
        }
        assert!(turns.finish(&source_file("/h/.atomcode/sessions/45d7/s-6.jsonl")).is_empty());
    }

    #[test]
    fn lines_skips_a_torn_tail_and_garbage() {
        let buf = b"{\"usage\":{}}\nnot json\n{\"usage\":{}}\n{partial";
        assert_eq!(lines(buf).len(), 2);
        assert!(lines(b"no newline at all").is_empty());
    }

    #[test]
    fn head_version_reads_the_record_schema_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("s-7.jsonl");
        assert_eq!(head_version(&log), None, "an absent file has no version");
        std::fs::write(
            &log,
            "{\"v\":1,\"session_id\":\"s-7\",\"usage\":{\"prompt\":1,\"completion\":1,\"cached\":0}}\n",
        )
        .unwrap();
        assert_eq!(head_version(&log).as_deref(), Some("turn record v1"));

        // A real turn's raw prompt and reply run to tens of kilobytes, so the
        // first line can sit whole *past* the head window. `v` is serialised
        // first, so `probe` still gets a hint out of it.
        let long = dir.path().join("s-8.jsonl");
        std::fs::write(
            &long,
            format!(
                "{{\"v\":2,\"session_id\":\"s-8\",\"usage\":{{\"prompt\":1,\"completion\":1,\"cached\":0}},\"assistant\":\"{}\"",
                "x".repeat(9000)
            ),
        )
        .unwrap();
        assert_eq!(head_version(&long).as_deref(), Some("turn record v2"));
    }

    fn source_file(path: &str) -> SourceFile {
        SourceFile {
            path: std::path::PathBuf::from(path),
            kind: usage_core::FileKind::Jsonl,
            size: 1,
            mtime_ms: 1_784_361_677_000,
        }
    }
}

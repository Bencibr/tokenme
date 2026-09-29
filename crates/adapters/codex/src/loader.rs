//! Sequential JSONL reader: byte cursor, tolerant parsing, and Codex's model
//! attribution.
//!
//! `ReadCursor.0` is a byte offset. Only whole lines are consumed, so a file the
//! app is still writing never yields a half record; the trailing partial line is
//! picked up on the next pass.

use std::io::{BufRead, BufReader, Read, Seek as _, SeekFrom};
use std::path::Path;

use usage_core::{Meter, QuotaSample, ReadCursor, ReadOutcome, TokenCounts, UsageEvent};

use crate::parser::{self, Record};
use crate::paths;
use crate::TOOL_ID;

/// Per-file attribution state, seeded by `session_meta` and updated per turn.
#[derive(Debug, Default)]
struct Attribution {
    session: String,
    project: Option<String>,
    meta_model: Option<String>,
    turn_model: Option<String>,
    /// Identity of the previous `token_count`: the session accumulator plus the
    /// exact usage. Codex re-reports some calls verbatim (measured 2.03% of all
    /// tokens on this machine), and only a byte-identical repeat is one — 294
    /// real records leave the accumulator flat while reporting a different call,
    /// so a looser test would silently drop them.
    previous_usage: Option<UsageSignature>,
}

type UsageSignature = (f64, TokenCounts, f64);

/// The billable payload of one `token_count`.
struct Usage {
    ts_ms: i64,
    counts: TokenCounts,
    quota: Option<QuotaSample>,
}

impl Attribution {
    fn new(path: &Path) -> Self {
        Self {
            session: paths::uuid_from_file_name(path).unwrap_or_else(|| path.to_string_lossy().into_owned()),
            ..Self::default()
        }
    }

    /// `token_count` carries no model, so the most recent `turn_context.model`
    /// wins; if none precedes it (sub-agent rollouts, a session resumed mid-file)
    /// fall back to any model on `session_meta`, else `None`.
    fn model(&self) -> Option<String> {
        self.turn_model.clone().or_else(|| self.meta_model.clone())
    }

    /// Applies one line's record and reports the usage it carried, if any.
    fn apply(&mut self, raw: &str) -> Option<Usage> {
        // Pre-gate on substrings: most bytes of a rollout are `response_item`
        // payloads, and parsing them just to learn they carry no usage is the one
        // thing a multi-GB log cannot survive.
        if !parser::worth_parsing(raw) {
            return None;
        }
        match parser::parse_record(raw)? {
            Record::SessionMeta { session, cwd, model } => {
                // A second `session_meta` is a new session boundary inside the same
                // file (a resumed or forked rollout), so a stale turn model must not
                // leak across it.
                self.turn_model = None;
                self.previous_usage = None;
                if let Some(s) = session {
                    self.session = s;
                }
                if cwd.is_some() {
                    self.project = cwd;
                }
                if model.is_some() {
                    self.meta_model = model;
                }
                None
            }
            Record::TurnContext { model, cwd } => {
                if model.is_some() {
                    self.turn_model = model;
                }
                if self.project.is_none() {
                    self.project = cwd;
                }
                None
            }
            Record::Usage { ts_ms, counts, reported_total, quota, accumulator } => {
                if counts.is_zero() {
                    // Context-size echo: emitting it would inflate the request count
                    // for a turn that billed nothing.
                    return None;
                }
                let signature = (accumulator, counts, reported_total);
                if accumulator > 0.0 && self.previous_usage == Some(signature) {
                    return None;
                }
                self.previous_usage = Some(signature);
                Some(Usage { ts_ms, counts, quota })
            }
        }
    }

    fn event(&self, usage: Usage, source_key: &str) -> UsageEvent {
        let mut ev = UsageEvent::new(TOOL_ID, usage.ts_ms, self.session.clone());
        ev.project = self.project.clone();
        ev.model = self.model();
        ev.meter = Meter::Tokens;
        ev.counts = usage.counts;
        ev.quota = usage.quota;
        // Codex records carry no per-call id, so idempotency comes from the byte
        // cursor rather than `dedupe_key`.
        ev.dedupe_key = None;
        ev.source = source_key.to_string();
        ev
    }
}

/// Reads `[cursor, last_complete_line)` from `path`.
pub(crate) fn read_rollout(path: &Path, cursor: ReadCursor, source_key: &str) -> ReadOutcome {
    try_read(path, cursor, source_key).unwrap_or_else(|_| ReadOutcome {
        // Unreadable file: hand back the position we were given. Resetting to 0
        // would make the next pass re-ingest the whole file, and this adapter has
        // no `dedupe_key` to absorb the duplicates.
        events: Vec::new(),
        cursor,
    })
}

fn try_read(path: &Path, cursor: ReadCursor, source_key: &str) -> std::io::Result<ReadOutcome> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    // Cursor past EOF means the log was rotated or truncated: re-read from the top
    // and let the indexer purge this `source` key's stale events.
    let start = if cursor.0 > len { 0 } else { cursor.0 };
    if start == len {
        // Nothing appended: skip even the attribution probe below.
        return Ok(ReadOutcome { events: Vec::new(), cursor: ReadCursor(start) });
    }
    let mut state = Attribution::new(path);
    if start > 0 {
        // A resumed pass has not read the record that names the current turn's
        // model, so recover it from the bytes just behind the cursor.
        let (model, cwd) = recover_turn_context(&mut file, start).unwrap_or((None, None));
        state.turn_model = model;
        state.project = cwd;
    }
    let mut events = Vec::new();
    let mut pos = start;
    if start < len {
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::with_capacity(128 * 1024, (&mut file).take(len - start));
        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        loop {
            buf.clear();
            let n = match reader.read_until(b'\n', &mut buf) {
                Ok(n) => n,
                // Torn chunk: stop here and resume at the last good line.
                Err(_) => break,
            };
            if n == 0 || !buf.ends_with(b"\n") {
                // EOF, or a mid-write partial line left for the next pass.
                break;
            }
            pos = pos.saturating_add(n as u64);
            if let Ok(line) = std::str::from_utf8(&buf) {
                if let Some(usage) = state.apply(line) {
                    events.push(state.event(usage, source_key));
                }
            }
        }
    }
    Ok(ReadOutcome { events, cursor: ReadCursor(pos) })
}

/// Bytes read per step, and the total budget, of the backwards attribution scan.
const SEED_CHUNK: u64 = 256 * 1024;
/// A turn whose records exceed the budget loses its model label rather than
/// making every pass re-read megabytes.
const SEED_CAP: u64 = 8 * 1024 * 1024;
const MARKER: &[u8] = b"turn_context";

/// The newest `turn_context` line before `cursor`, as (model, cwd).
///
/// Reads backwards in chunks and stops at the first hit, so a steady-state pass
/// costs one chunk of I/O (on real logs that distance is ~90 KiB median) instead
/// of a re-read of the file. Scan failures are not fatal: an unlabelled model is
/// a pricing gap, whereas dropping the events would lose usage outright.
fn recover_turn_context(file: &mut std::fs::File, cursor: u64) -> Option<(Option<String>, Option<String>)> {
    let mut base = cursor;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let start = base.saturating_sub(SEED_CHUNK);
        let mut chunk = vec![0u8; (base - start) as usize];
        if file.seek(SeekFrom::Start(start)).is_err() || file.read_exact(&mut chunk).is_err() {
            return None;
        }
        chunk.extend_from_slice(&buf);
        buf = chunk;
        base = start;
        match scan_for_turn_context(&buf, base == 0) {
            Scan::Found(hit) => return Some(hit),
            Scan::Truncated | Scan::NotFound if base == 0 || cursor - base >= SEED_CAP => return None,
            Scan::Truncated | Scan::NotFound => {}
        }
    }
}

enum Scan {
    Found((Option<String>, Option<String>)),
    /// No usable record in view; more bytes might hold one.
    NotFound,
    /// The leftmost candidate's line starts before the buffer: read more bytes.
    Truncated,
}

fn scan_for_turn_context(buf: &[u8], at_bof: bool) -> Scan {
    let mut limit = buf.len();
    loop {
        let Some(rel) = rfind_marker(&buf[..limit]) else { return Scan::NotFound };
        let line_start = match buf[..rel].iter().rposition(|b| *b == b'\n') {
            Some(i) => i + 1,
            None if !at_bof => return Scan::Truncated,
            None => 0,
        };
        let Some(nl) = buf[rel..].iter().position(|b| *b == b'\n') else {
            // The candidate runs past the cursor: that is the mid-write tail, so
            // discard it and keep looking further left.
            limit = line_start;
            continue;
        };
        let line_end = rel + nl;
        if let Ok(line) = std::str::from_utf8(&buf[line_start..line_end]) {
            if let Some(Record::TurnContext { model, cwd }) = parser::parse_record(line) {
                return Scan::Found((model, cwd));
            }
        }
        // The marker appeared inside some other record's text: step left of it.
        limit = line_start;
    }
}

fn rfind_marker(hay: &[u8]) -> Option<usize> {
    if hay.len() < MARKER.len() {
        return None;
    }
    (0..=hay.len() - MARKER.len()).rev().find(|&i| &hay[i..i + MARKER.len()] == MARKER)
}



#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn token_count(total: f64, last_total: f64) -> String {
        json!({
            "timestamp": "2026-09-23T10:00:00.000Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": last_total, "cached_input_tokens": 0.0,
                        "output_tokens": 0.0, "total_tokens": last_total
                    },
                    "total_token_usage": { "total_tokens": total }
                }
            }
        })
        .to_string()
    }

    fn meta(session: &str) -> String {
        json!({ "type": "session_meta", "payload": { "session_id": session } }).to_string()
    }

    fn new_state() -> Attribution {
        Attribution::new(&PathBuf::from("/tmp/rollout-x.jsonl"))
    }

    #[test]
    fn a_verbatim_re_report_is_dropped_once() {
        let mut a = new_state();
        assert!(a.apply(&meta("s1")).is_none());
        assert!(a.apply(&token_count(1000.0, 1000.0)).is_some(), "first call bills");
        assert!(a.apply(&token_count(1000.0, 1000.0)).is_none(), "identical repeat re-bills");
        assert!(a.apply(&token_count(2000.0, 1000.0)).is_some(), "accumulator moved, so a new call");
    }

    #[test]
    fn a_flat_accumulator_with_different_usage_is_still_a_call() {
        // Real shape from the fixtures: `total_token_usage` parked at one value
        // while three distinct calls report different usage.
        let mut a = new_state();
        assert!(a.apply(&token_count(930_000.0, 15_687.0)).is_some());
        assert!(a.apply(&token_count(930_000.0, 19_847.0)).is_some());
        assert!(a.apply(&token_count(930_000.0, 8_402.0)).is_some());
    }

    #[test]
    fn a_new_session_boundary_clears_the_repeat_memory() {
        let mut a = new_state();
        assert!(a.apply(&token_count(500.0, 500.0)).is_some());
        a.apply(&meta("s2"));
        assert!(a.apply(&token_count(500.0, 500.0)).is_some());
    }

    #[test]
    fn a_record_without_an_accumulator_is_never_dropped() {
        let mut a = new_state();
        let bare = json!({
            "timestamp": "2026-09-23T10:00:00.000Z", "type": "event_msg",
            "payload": { "type": "token_count", "info": { "last_token_usage": {
                "input_tokens": 75.0, "cached_input_tokens": 0.0,
                "output_tokens": 0.0, "total_tokens": 75.0 } } }
        })
        .to_string();
        assert!(a.apply(&bare).is_some());
        assert!(a.apply(&bare).is_some(), "no accumulator to compare against");
    }
}

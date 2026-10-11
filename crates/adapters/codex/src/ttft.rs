//! Independent, best-effort extraction of turn-level TTFT from Codex rollouts.
//!
//! This is deliberately separate from billable `UsageEvent`s. Codex currently
//! persists the measurement on the terminal `event_msg.task_complete` record.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Per-file resume state. Keep one cursor per rollout path; it contains the
/// model attribution and completed turn ids needed to interpret later appends.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexTtftCursor {
    offset: u64,
    models: HashMap<String, String>,
    completed_turns: HashSet<String>,
}

impl CodexTtftCursor {
    /// Start reading at a known byte offset without scanning earlier history.
    /// Model attribution starts empty and is available after a matching
    /// `turn_context` record is observed.
    pub fn at_offset(offset: u64) -> Self {
        Self { offset, ..Self::default() }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }
}

/// One Codex turn-level TTFT observation, in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexTtftSample {
    pub turn_id: String,
    pub model: Option<String>,
    pub ttft_ms: u64,
    /// Timestamp on the terminal rollout record, when present and parseable.
    pub completed_at_ms: Option<i64>,
}

/// Samples parsed after the supplied cursor, plus the cursor to persist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexTtftRead {
    pub samples: Vec<CodexTtftSample>,
    pub cursor: CodexTtftCursor,
}

/// Read complete JSONL records appended after `cursor` and extract turn TTFT.
///
/// A trailing partial line is left unread for the next call. If the file is
/// truncated below the saved offset, parsing restarts at byte zero and clears
/// attribution/deduplication state. As with the usage reader, replacement by a
/// same-size-or-larger file cannot be detected from a byte offset alone.
pub fn read_ttft(path: &Path, mut cursor: CodexTtftCursor) -> std::io::Result<CodexTtftRead> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if cursor.offset > len {
        cursor = CodexTtftCursor::default();
    }
    if cursor.offset == len {
        return Ok(CodexTtftRead { samples: Vec::new(), cursor });
    }

    let start = cursor.offset;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(64 * 1024, (&mut file).take(len - start));
    let mut buf = Vec::with_capacity(16 * 1024);
    let mut samples = Vec::new();
    let mut pos = start;
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 || !buf.ends_with(b"\n") {
            break;
        }
        pos = pos.saturating_add(n as u64);
        let Ok(line) = std::str::from_utf8(&buf) else { continue };
        if !line.contains("turn_context") && !line.contains("task_complete") && !line.contains("turn_complete") {
            continue;
        }
        if let Some((turn_id, model)) = turn_model(line) {
            cursor.models.insert(turn_id, model);
            continue;
        }
        if let Some(sample) = complete_sample(line, &cursor) {
            if cursor.completed_turns.insert(sample.turn_id.clone()) {
                samples.push(sample);
            }
        }
    }
    cursor.offset = pos;
    Ok(CodexTtftRead { samples, cursor })
}

fn record_parts(line: &str) -> Option<(Value, Value)> {
    let record: Value = serde_json::from_str(line).ok()?;
    let payload = record.get("payload")?.clone();
    Some((record, payload))
}

fn nonempty_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
        .map(str::to_string)
}

fn turn_model(line: &str) -> Option<(String, String)> {
    let (record, payload) = record_parts(line)?;
    let root_kind = record.get("type").and_then(Value::as_str);
    let payload_kind = payload.get("type").and_then(Value::as_str);
    if root_kind != Some("turn_context") && !(root_kind == Some("event_msg") && payload_kind == Some("turn_context")) {
        return None;
    }
    let model = nonempty_string(&payload, "model")?;
    if model.len() > 128 { return None; }
    Some((nonempty_string(&payload, "turn_id")?, model))
}

fn complete_sample(line: &str, cursor: &CodexTtftCursor) -> Option<CodexTtftSample> {
    let (record, payload) = record_parts(line)?;
    if record.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let kind = payload.get("type").and_then(Value::as_str)?;
    if kind != "task_complete" && kind != "turn_complete" {
        return None;
    }
    let turn_id = nonempty_string(&payload, "turn_id")?;
    let ttft_ms = u64::try_from(payload.get("time_to_first_token_ms")?.as_i64()?).ok()?;
    let completed_at_ms = record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(usage_core::parse_ts_ms);
    Some(CodexTtftSample {
        model: cursor.models.get(&turn_id).cloned(),
        turn_id,
        ttft_ms,
        completed_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn record(kind: &str, payload: Value) -> String {
        serde_json::json!({"timestamp":"2026-09-23T11:32:31.944Z","type":kind,"payload":payload}).to_string() + "\n"
    }

    #[test]
    fn reads_terminal_ttft_and_correlates_model_by_turn_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(record("turn_context", serde_json::json!({"turn_id":"t1","model":"gpt-5.6-luna"})).as_bytes()).unwrap();
        file.write_all(record("event_msg", serde_json::json!({"type":"task_complete","turn_id":"t1","time_to_first_token_ms":1234})).as_bytes()).unwrap();

        let out = read_ttft(&path, CodexTtftCursor::default()).unwrap();
        assert_eq!(out.samples.len(), 1);
        assert_eq!(out.samples[0].turn_id, "t1");
        assert_eq!(out.samples[0].model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(out.samples[0].ttft_ms, 1234);
        assert_eq!(out.samples[0].completed_at_ms, Some(1_790_163_151_944));
        assert_eq!(out.cursor.offset(), std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn offset_constructor_starts_with_empty_attribution_state() {
        let cursor = CodexTtftCursor::at_offset(42);
        assert_eq!(cursor.offset(), 42);
        assert!(cursor.models.is_empty());
        assert!(cursor.completed_turns.is_empty());
    }

    #[test]
    fn cursor_preserves_model_context_and_deduplicates_across_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let context = record("turn_context", serde_json::json!({"turn_id":"t1","model":"gpt-5.6-luna"}));
        std::fs::write(&path, context).unwrap();
        let first = read_ttft(&path, CodexTtftCursor::default()).unwrap();
        assert!(first.samples.is_empty());

        let complete = record("event_msg", serde_json::json!({"type":"task_complete","turn_id":"t1","time_to_first_token_ms":0}));
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(complete.as_bytes()).unwrap();
        let second = read_ttft(&path, first.cursor).unwrap();
        assert_eq!(second.samples.len(), 1);
        assert_eq!(second.samples[0].ttft_ms, 0, "zero is a valid measured value");
        assert_eq!(second.samples[0].model.as_deref(), Some("gpt-5.6-luna"));

        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(complete.as_bytes()).unwrap();
        let third = read_ttft(&path, second.cursor).unwrap();
        assert!(third.samples.is_empty(), "duplicate terminal record does not emit twice");
    }

    #[test]
    fn leaves_partial_line_for_next_read_and_rejects_unsafe_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let good = record("event_msg", serde_json::json!({"type":"task_complete","turn_id":"ok","time_to_first_token_ms":7}));
        let invalid = record("event_msg", serde_json::json!({"type":"task_complete","turn_id":"negative","time_to_first_token_ms":-1}));
        std::fs::write(&path, format!("{good}{invalid}{{\"type\":" )).unwrap();
        let first = read_ttft(&path, CodexTtftCursor::default()).unwrap();
        assert_eq!(first.samples.len(), 1);
        assert_eq!(first.samples[0].turn_id, "ok");
        assert!(first.cursor.offset() < std::fs::metadata(&path).unwrap().len());

        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"\"event_msg\"}\n").unwrap();
        let second = read_ttft(&path, first.cursor).unwrap();
        assert!(second.samples.is_empty(), "malformed completion is ignored safely");
        assert_eq!(second.cursor.offset(), std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn restarts_when_rollout_is_truncated_below_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let old = record("event_msg", serde_json::json!({"type":"task_complete","turn_id":"old-turn-with-long-id","time_to_first_token_ms":9}));
        std::fs::write(&path, &old).unwrap();
        let first = read_ttft(&path, CodexTtftCursor::default()).unwrap();
        assert_eq!(first.samples.len(), 1);

        let new = record("event_msg", serde_json::json!({"type":"task_complete","turn_id":"new","time_to_first_token_ms":10}));
        std::fs::write(&path, &new).unwrap();
        let second = read_ttft(&path, first.cursor).unwrap();
        assert_eq!(second.samples.len(), 1);
        assert_eq!(second.samples[0].turn_id, "new");
    }
}

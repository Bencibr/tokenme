//! Parses one Codely chat snapshot.
//!
//! ## Measured shape
//!
//! `conversationRecord.messages[]` holds one entry per turn. Assistant entries
//! are `type: "gemini"` today and carry the call's own usage:
//!
//! ```json
//! {"id":"…","timestamp":"2026-09-14T13:38:13.150Z","type":"gemini",
//!  "model":"codely-core",
//!  "tokens":{"input":56806,"output":315,"cached":11008,
//!            "thoughts":0,"tool":0,"total":57121}}
//! ```
//!
//! `input` is the **whole prompt**, cached prefix included: on all 1,226 usage
//! rows measured on this machine `total == input + output`, `cached <= input`,
//! and `cached` is the part of the prompt served from cache. The four stages in
//! [`TokenCounts`] are mutually exclusive, so the adapter nets the cached part
//! out of `input` (otherwise the cached prefix would be billed twice) and keeps
//! it as `cache_read`.
//!
//! `thoughts` and `tool` are sub-breakdowns of `output`, not extra tokens — the
//! same `total` identity proves it. `thoughts` maps to `reasoning` (which
//! `TokenCounts::total` deliberately does not add on top), `tool` needs no
//! separate stage because it is already inside `output`. Both were 0 on every
//! row measured, so neither mapping invents a number today; it is here so a
//! later build that starts reporting either is not silently ignored.
//!
//! ## Identity
//!
//! A session file is rewritten whole on every save, so `read` re-parses it and
//! the dedupe key is the only thing that makes replay idempotent. `id` is a
//! stable UUID per message; the session id scopes it because the index's dedupe
//! space is global. A message with no id falls back to its position, which is
//! stable across rewrites.

use usage_core::{parse_ts_ms, Meter, TokenCounts, UsageEvent};

use crate::{paths, TOOL_ID};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    /// Messages that carried a `tokens` object.
    pub messages_with_tokens: usize,
    /// Messages that became events (a row can be dropped for an unparseable stamp).
    pub events: usize,
    /// Σ `input` as written (whole prompt, cached prefix included).
    pub wire_input: u64,
    /// Σ `cached`.
    pub wire_cache_read: u64,
    /// Σ `output`.
    pub wire_output: u64,
    /// Σ `thoughts`.
    pub wire_thoughts: u64,
    /// Rows where `cached > input`, i.e. the inclusive convention broke down.
    pub cache_split_violations: usize,
}

/// JSON text → the events it bills. Unparseable JSON is an empty result, never
/// an error: the CLI rewrites the file, so the next pass may already be good.
pub fn parse_transcript(text: &str, source: &str) -> (Vec<UsageEvent>, Stats) {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return (Vec::new(), Stats::default());
    };
    let mut stats = Stats::default();
    let mut events = Vec::new();
    let Some(record) = root.get("conversationRecord").and_then(serde_json::Value::as_object) else {
        return (events, stats);
    };
    let Some(messages) = record.get("messages").and_then(serde_json::Value::as_array) else {
        return (events, stats);
    };
    let session = paths::session_id(&root, std::path::Path::new(source));
    let project = paths::project_from(&root);
    events.reserve(messages.len());

    for (index, message) in messages.iter().enumerate() {
        let Some(tokens) = message.get("tokens").and_then(serde_json::Value::as_object) else {
            continue;
        };
        stats.messages_with_tokens += 1;

        let input = word(tokens, "input");
        let cached = word(tokens, "cached");
        let output = word(tokens, "output");
        let thoughts = word(tokens, "thoughts");
        if cached > input {
            stats.cache_split_violations += 1;
        }
        stats.wire_input += input;
        stats.wire_cache_read += cached;
        stats.wire_output += output;
        stats.wire_thoughts += thoughts;

        let counts = TokenCounts {
            input: input.saturating_sub(cached) as f64,
            cache_creation: 0.0,
            cache_read: cached as f64,
            output: output as f64,
            // A sub-breakdown of `output`, never an extra stage.
            reasoning: thoughts as f64,
            credits: 0.0,
        };
        let Some(ts_ms) = timestamp_ms(message) else { continue };
        stats.events += 1;

        let mut event = UsageEvent::new(TOOL_ID, ts_ms, session.clone()).with(counts);
        event.meter = Meter::Tokens;
        event.project = project.clone();
        event.model = message
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string);
        event.dedupe_key = Some(match message.get("id").and_then(serde_json::Value::as_str) {
            Some(id) if !id.is_empty() => format!("{TOOL_ID}#{session}#{id}"),
            _ => format!("{TOOL_ID}#{session}#idx{index}"),
        });
        event.source = source.to_string();
        events.push(event);
    }
    (events, stats)
}

fn word(tokens: &serde_json::Map<String, serde_json::Value>, key: &str) -> u64 {
    let Some(value) = tokens.get(key) else { return 0 };
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|n| n.max(0.0) as u64))
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()).map(|n| n.max(0.0) as u64))
        .unwrap_or(0)
}

/// `timestamp` is the RFC 3339 stamp the CLI writes for every turn. A numeric
/// stamp (seconds or milliseconds) is accepted as well, through the shared
/// `< 1e10 ⇒ seconds` rule.
fn timestamp_ms(message: &serde_json::Value) -> Option<i64> {
    let value = message.get("timestamp")?;
    if let Some(text) = value.as_str() {
        return parse_ts_ms(text);
    }
    value.as_u64().and_then(|n| parse_ts_ms(&n.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/session.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn the_cached_prefix_is_not_billed_twice() {
        let (events, stats) = parse_transcript(&fixture(), "/tmp/h/chat-sess-1.json");
        assert_eq!(stats.messages_with_tokens, 2);
        assert_eq!(stats.events, 2);
        assert_eq!(stats.wire_input, 1500);
        assert_eq!(stats.wire_cache_read, 400);
        assert_eq!(stats.wire_output, 70);
        assert_eq!(stats.wire_thoughts, 5);
        assert_eq!(stats.cache_split_violations, 0);

        let first = &events[0];
        assert_eq!(first.session, "sess-1");
        assert_eq!(first.project.as_deref(), Some("demo"));
        assert_eq!(first.model.as_deref(), Some("codely-core"));
        assert_eq!(first.dedupe_key.as_deref(), Some("codely#sess-1#m1"));
        assert_eq!(first.ts_ms, parse_ts_ms("2026-09-14T13:00:05.000Z").unwrap());
        assert_eq!(
            first.counts,
            TokenCounts { input: 600.0, cache_creation: 0.0, cache_read: 400.0, output: 50.0, reasoning: 0.0, credits: 0.0 }
        );
        assert_eq!(first.counts.total(), 1050.0, "the wire total, not the cached prompt twice");

        let second = &events[1];
        assert_eq!(second.counts.reasoning, 5.0, "thoughts are a sub-breakdown of output");
        assert_eq!(second.counts.total(), 520.0);
        assert_eq!(second.counts.output, 20.0, "reasoning never adds to output");
    }

    #[test]
    fn a_row_without_a_stamp_is_dropped_but_the_rest_survive() {
        let text = r#"{"conversationRecord":{"sessionId":"s","messages":[
            {"id":"bad","type":"gemini","tokens":{"input":1,"output":1}},
            {"id":"good","timestamp":"2026-09-14T13:00:05Z","tokens":{"input":2,"output":3,"cached":0}}
        ]}}"#;
        let (events, stats) = parse_transcript(text, "/tmp/chat-x.json");
        assert_eq!(stats.messages_with_tokens, 2);
        assert_eq!(stats.events, 1);
        assert_eq!(events[0].dedupe_key.as_deref(), Some("codely#s#good"));
    }

    #[test]
    fn an_unparseable_object_yields_nothing() {
        let (events, stats) = parse_transcript("{not json", "/tmp/chat-x.json");
        assert!(events.is_empty());
        assert_eq!(stats, Stats::default());
    }
}

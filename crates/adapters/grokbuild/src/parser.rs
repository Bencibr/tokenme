//! Parses one Grok Build `usage.json`.
//!
//! ## Measured shape (Grok Build 2026-11)
//!
//! ```json
//! {
//!   "sessionId": "01a1…",
//!   "updatedAt": "2026-11-03T16:53:50.025683300+00:00",
//!   "session": { "inputTokens": 22477, "outputTokens": 61,
//!                "cachedReadTokens": 1664, "cacheCreationTokens": 0,
//!                "reasoningTokens": 38, "totalTokens": 22538,
//!                "primaryModelId": "grok-4.7-build", … },
//!   "turns": [{
//!     "turnNumber": 1,
//!     "endedAt": "2026-11-03T16:53:50.025683300+00:00",
//!     "inputTokens": 22477, "outputTokens": 61, …
//!     "primaryModelId": "grok-4.7-build"
//!   }]
//! }
//! ```
//!
//! `totalTokens == inputTokens + outputTokens` on the measured rows, so
//! `cachedReadTokens` / `cacheCreationTokens` are **inside** the prompt stage
//! (same inclusive convention Codely uses). The four [`TokenCounts`] stages
//! are mutually exclusive, so the adapter nets both cache figures out of
//! `input`. `reasoningTokens` is a sub-breakdown of the completion and is
//! kept as `reasoning` (which [`TokenCounts::total`] deliberately does not
//! add on top).
//!
//! ## Identity
//!
//! `usage.json` is rewritten whole as turns land (`FileKind::Tree`), so the
//! dedupe key is the only thing that makes replay idempotent. A turn's
//! `turnNumber` is stable inside a session.

use usage_core::{parse_ts_ms, Meter, TokenCounts, UsageEvent};

use crate::{paths, TOOL_ID};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    pub turns_with_tokens: usize,
    pub events: usize,
    pub wire_input: u64,
    pub wire_cache_read: u64,
    pub wire_cache_creation: u64,
    pub wire_output: u64,
    pub wire_reasoning: u64,
    pub cache_split_violations: usize,
}

/// JSON text → the events it bills. Unparseable JSON is an empty result: the
/// CLI rewrites the file, so the next pass may already be good.
pub fn parse_usage(text: &str, source: &str, usage_path: &std::path::Path) -> (Vec<UsageEvent>, Stats) {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return (Vec::new(), Stats::default());
    };
    let mut stats = Stats::default();
    let session = root
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| paths::session_id(usage_path));
    let project = paths::project_for(usage_path);
    // Bill only `turns[]`. The sibling `session` object is a cumulative
    // projection of the same turns — emitting it (even as `#turn0`) would
    // stick in the index after real turns land, because Tree ingest never
    // deletes orphaned dedupe keys. Empty / missing turns → wait for the
    // next rewrite; do not invent a provisional row.
    let Some(turns) = root.get("turns").and_then(|v| v.as_array()) else {
        return (Vec::new(), stats);
    };

    let mut events = Vec::with_capacity(turns.len());
    for turn in turns {
        let Some(obj) = turn.as_object() else { continue };
        let turn_number = obj
            .get("turnNumber")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if let Some(event) = turn_event(
            obj,
            &session,
            project.as_deref(),
            source,
            turn_number,
            obj.get("endedAt").or_else(|| root.get("updatedAt")),
            &mut stats,
        ) {
            events.push(event);
        }
    }
    (events, stats)
}

fn turn_event(
    obj: &serde_json::Map<String, serde_json::Value>,
    session: &str,
    project: Option<&str>,
    source: &str,
    turn_number: u64,
    stamp: Option<&serde_json::Value>,
    stats: &mut Stats,
) -> Option<UsageEvent> {
    let input = word(obj, "inputTokens");
    let output = word(obj, "outputTokens");
    let cache_read = word(obj, "cachedReadTokens");
    let cache_creation = word(obj, "cacheCreationTokens");
    let reasoning = word(obj, "reasoningTokens");
    if input == 0 && output == 0 && cache_read == 0 && cache_creation == 0 {
        return None;
    }
    stats.turns_with_tokens += 1;
    if cache_read.saturating_add(cache_creation) > input {
        stats.cache_split_violations += 1;
    }
    stats.wire_input += input;
    stats.wire_cache_read += cache_read;
    stats.wire_cache_creation += cache_creation;
    stats.wire_output += output;
    stats.wire_reasoning += reasoning;

    let ts_ms = stamp.and_then(stamp_ms)?;
    stats.events += 1;

    let net_cache = cache_read.saturating_add(cache_creation);
    let counts = TokenCounts {
        input: input.saturating_sub(net_cache) as f64,
        cache_creation: cache_creation as f64,
        cache_read: cache_read as f64,
        output: output as f64,
        reasoning: reasoning as f64,
        credits: 0.0,
    };

    let mut event = UsageEvent::new(TOOL_ID, ts_ms, session.to_string()).with(counts);
    event.meter = Meter::Tokens;
    event.project = project.map(str::to_string);
    event.model = obj
        .get("primaryModelId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    event.dedupe_key = Some(format!("{TOOL_ID}#{session}#turn{turn_number}"));
    event.source = source.to_string();
    Some(event)
}

fn word(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> u64 {
    let Some(value) = obj.get(key) else {
        return 0;
    };
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|n| n.max(0.0) as u64))
        .or_else(|| {
            value
                .as_str()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .map(|n| n.max(0.0) as u64)
        })
        .unwrap_or(0)
}

fn stamp_ms(value: &serde_json::Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return parse_ts_ms(text);
    }
    value.as_u64().and_then(|n| parse_ts_ms(&n.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const FIXTURE: &str = r#"{
      "sessionId": "01a1a252-5d17-7683-a4dc-704c86ae04c1",
      "updatedAt": "2026-11-03T16:53:50.025683300+00:00",
      "session": {
        "inputTokens": 22477,
        "outputTokens": 61,
        "cachedReadTokens": 1664,
        "cacheCreationTokens": 0,
        "reasoningTokens": 38,
        "totalTokens": 22538,
        "primaryModelId": "grok-4.7-build"
      },
      "turns": [
        {
          "turnNumber": 1,
          "endedAt": "2026-11-03T16:53:50.025683300+00:00",
          "inputTokens": 22477,
          "outputTokens": 61,
          "cachedReadTokens": 1664,
          "cacheCreationTokens": 0,
          "reasoningTokens": 38,
          "totalTokens": 22538,
          "primaryModelId": "grok-4.7-build"
        }
      ]
    }"#;

    #[test]
    fn turn_nets_cache_out_of_input_and_keeps_reasoning_aside() {
        let path = Path::new("/tmp/sessions/cwd/01a1a252-5d17-7683-a4dc-704c86ae04c1/usage.json");
        let (events, stats) = parse_usage(FIXTURE, &path.display().to_string(), path);
        assert_eq!(events.len(), 1);
        assert_eq!(stats.events, 1);
        assert_eq!(stats.cache_split_violations, 0);
        let e = &events[0];
        assert_eq!(e.counts.input, 22477.0 - 1664.0);
        assert_eq!(e.counts.cache_read, 1664.0);
        assert_eq!(e.counts.cache_creation, 0.0);
        assert_eq!(e.counts.output, 61.0);
        assert_eq!(e.counts.reasoning, 38.0);
        assert_eq!(e.counts.total(), 22477.0 - 1664.0 + 1664.0 + 61.0);
        assert_eq!(e.model.as_deref(), Some("grok-4.7-build"));
        assert_eq!(
            e.dedupe_key.as_deref(),
            Some("grokbuild#01a1a252-5d17-7683-a4dc-704c86ae04c1#turn1")
        );
        assert!(e.ts_ms > 0);
    }

    #[test]
    fn session_aggregate_alone_is_not_billed() {
        // `session` mirrors the sum of turns; indexing it would double-count
        // once turns appear (Tree ingest keeps orphaned keys).
        let text = r#"{
          "sessionId": "sess-only",
          "updatedAt": "2026-11-03T16:53:50Z",
          "session": {
            "inputTokens": 100,
            "outputTokens": 10,
            "cachedReadTokens": 20,
            "cacheCreationTokens": 0,
            "reasoningTokens": 0,
            "primaryModelId": "grok-4.7"
          }
        }"#;
        let path = Path::new("/tmp/sessions/cwd/sess-only/usage.json");
        assert!(parse_usage(text, "src", path).0.is_empty());
        let empty_turns = r#"{
          "sessionId": "sess-empty",
          "updatedAt": "2026-11-03T16:53:50Z",
          "session": {"inputTokens": 100, "outputTokens": 10, "cachedReadTokens": 0, "cacheCreationTokens": 0},
          "turns": []
        }"#;
        assert!(parse_usage(empty_turns, "src", path).0.is_empty());
    }

    #[test]
    fn junk_yields_nothing() {
        let path = Path::new("/tmp/x/usage.json");
        assert!(parse_usage("not json", "s", path).0.is_empty());
        assert!(parse_usage("{}", "s", path).0.is_empty());
    }
}

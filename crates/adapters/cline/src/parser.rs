//! Parses Cline's rewritten-in-place transcript object `*.messages.json`.
//!
//! ## Measured shape (all 5 transcripts on this machine)
//! `{version, updated_at, agent, sessionId, origin, system_prompt, messages:[…]}`.
//! The file is pretty-printed and **rewritten whole**, never appended, so a torn
//! write is possible. Only assistant rows carry usage:
//!
//! ```json
//! {"id":"msg_JSe0dOQg","role":"assistant","ts":1788497041736,
//!  "modelInfo":{"id":"deepseek/deepseek-v4-flash","provider":"cline"},
//!  "metrics":{"inputTokens":6222,"outputTokens":122,"cacheReadTokens":0,"cacheWriteTokens":0},
//!  "content":[…]}
//! ```
//! 273/273 assistant rows in the current files carry `metrics`, `ts` is epoch
//! **milliseconds** (13 digits — never run through the seconds heuristic), and
//! `modelInfo.id` keeps its provider prefix, which we pass through verbatim.
//!
//! ## `inputTokens` INCLUDES `cacheReadTokens` — the 2× trap, measured
//! Two independent proofs from the real files (`mapped_numbers_match_the_prompt`):
//! 1. containment — `inputTokens >= cacheReadTokens` holds for **273/273** rows.
//!    Anthropic-style "prompt excludes cache" sources show the opposite once a
//!    context is warm (fresh part of a few hundred tokens against a 100k cached
//!    prefix); it never happens here.
//! 2. prefix-chain — for consecutive calls, `cacheRead(t+1) ≈ input(t)` in 87/90
//!    pairs of the big session (`≈ input(t) − cacheRead(t)` in only 6/90). The
//!    next request's cached prefix is the *whole* previous prompt, so `inputTokens`
//!    must already be the whole prompt.
//!
//! Hence `input = inputTokens − cacheReadTokens` and `cache_read = cacheReadTokens`
//! — the four `TokenCounts` stages are documented as mutually exclusive so that
//! `total()` never double-counts, and mapping `input` verbatim would bill the
//! cached prefix twice (22.9M prompt tokens instead of 11.8M for session
//! `1787490736874_djat1`, a 1.9× overcharge).
//!
//! `cacheWriteTokens` is 0 in all 273 rows: DeepSeek-style implicit caching
//! charges no write tier, and the uncached part is already in `input`. So we map
//! it verbatim instead of reconstructing a context-occupancy delta.
//!
//! ## Session meta is label-only
//! `<ts>_<id>.json` next to the transcript holds `metadata.usage` AND
//! `metadata.aggregateUsage`, which are two copies of one cumulative
//! accumulator that already equals the per-message sums (proved by
//! `emitted_tokens_equal_the_session_accumulator`) — reading either as events
//! would double the session. It is parsed for `cwd`/`workspace_root` only, and
//! **no event is ever emitted from that file**.

use usage_core::{Meter, TokenCounts, UsageEvent};

use crate::TOOL_ID;

/// Money fields we deliberately never read: cost is computed centrally from the
/// shared price table so every tool stays comparable.
#[cfg(test)]
const COST_KEYS: &[&str] = &["totalCost", "aggregatedAgentsCost"];

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    /// Rows visited that had `role:"assistant"` and a `metrics` object.
    pub assistant_with_metrics: usize,
    /// Rows that became events (a row can be dropped for an unparseable `ts`).
    pub events: usize,
    /// Σ `inputTokens` as written (whole prompt, cached prefix included).
    pub wire_input: u64,
    /// Σ `cacheReadTokens`.
    pub wire_cache_read: u64,
    /// Σ `cacheWriteTokens` — 0 on every row measured.
    pub wire_cache_write: u64,
    /// Σ `outputTokens`.
    pub wire_output: u64,
    /// Rows where `cacheReadTokens > inputTokens`, i.e. the inclusive convention
    /// the stage split assumes broke down. 0 on every row measured here.
    pub cache_split_violations: usize,
}

impl Stats {
    /// The four numbers exactly as Cline writes them (`inputTokens` still holds
    /// the cached prefix).
    #[cfg(test)]
    pub fn wire_counts(&self) -> TokenCounts {
        TokenCounts {
            input: self.wire_input as f64,
            cache_creation: self.wire_cache_write as f64,
            cache_read: self.wire_cache_read as f64,
            output: self.wire_output as f64,
            reasoning: 0.0,
            credits: 0.0,
        }
    }

    /// The same totals through the adapter's stage split — what the events add up
    /// to when no row was dropped for an unparseable `ts`.
    #[cfg(test)]
    pub fn mapped_counts(&self) -> TokenCounts {
        TokenCounts {
            input: self.wire_input.saturating_sub(self.wire_cache_read) as f64,
            cache_creation: self.wire_cache_write as f64,
            cache_read: self.wire_cache_read as f64,
            output: self.wire_output as f64,
            reasoning: 0.0,
            credits: 0.0,
        }
    }
}

/// One transcript object → events. `fallback_session`/`project`/`source` come from
/// the caller (file name, sibling meta, manifest key), which keeps this function
/// filesystem-free and testable. Unparseable JSON yields an empty result, never an
/// error: the file is rewritten wholesale, so the next pass may already be good.
pub fn parse_transcript(text: &str, fallback_session: &str, project: Option<&str>, source: &str) -> (Vec<UsageEvent>, Stats) {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return (Vec::new(), Stats::default());
    };
    parse_value(&root, fallback_session, project, source)
}

fn parse_value(root: &serde_json::Value, fallback_session: &str, project: Option<&str>, source: &str) -> (Vec<UsageEvent>, Stats) {
    let mut stats = Stats::default();
    let mut events = Vec::new();
    let Some(object) = root.as_object() else {
        // Some builds hand us a bare array of messages; nothing billable is lost
        // by ignoring a shape we have never seen.
        return (events, stats);
    };
    let session = object.get("sessionId").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty()).unwrap_or(fallback_session);
    let Some(messages) = object.get("messages").and_then(serde_json::Value::as_array) else {
        return (events, stats);
    };
    events.reserve(messages.len());

    for (idx, message) in messages.iter().enumerate() {
        let Some(metrics) = assistant_metrics(message) else { continue };
        stats.assistant_with_metrics += 1;

        let input = word(metrics, "inputTokens");
        let cache_read = word(metrics, "cacheReadTokens");
        let cache_write = word(metrics, "cacheWriteTokens");
        let output = word(metrics, "outputTokens");
        if input < cache_read {
            stats.cache_split_violations += 1;
        }
        stats.wire_input += input;
        stats.wire_cache_read += cache_read;
        stats.wire_cache_write += cache_write;
        stats.wire_output += output;

        // Proven inclusive convention: the cached prefix is part of `inputTokens`.
        // `saturating_sub` also keeps a hypothetical exclusive-shaped row from
        // underflowing while the smoke test checks the containment invariant.
        let counts = TokenCounts {
            input: input.saturating_sub(cache_read) as f64,
            cache_creation: cache_write as f64,
            cache_read: cache_read as f64,
            output: output as f64,
            reasoning: 0.0,
            credits: 0.0,
        };
        let Some(ts_ms) = timestamp_ms(message) else { continue };
        stats.events += 1;
        let mut event = UsageEvent::new(TOOL_ID, ts_ms, session).with(counts);
        event.meter = Meter::Tokens;
        event.project = project.map(str::to_string);
        event.model = model_id(message);
        // The indexer's dedupe index is GLOBAL, so a bare `msg_JSe0dOQg` would
        // collide across sessions and silently drop events; a rewritten
        // transcript re-emits every row on each pass, so this key is the only
        // thing keeping `Tree` idempotent. Positional fallback for a row with no
        // `id` keeps that property (message order is stable across rewrites).
        event.dedupe_key = Some(match message.get("id").and_then(serde_json::Value::as_str) {
            Some(id) if !id.is_empty() => format!("{session}#{id}"),
            _ => format!("{session}#idx{idx}"),
        });
        event.source = source.to_string();
        events.push(event);
    }
    (events, stats)
}

/// `Some(metrics map)` for an assistant row that reports usage, `None` otherwise.
fn assistant_metrics(message: &serde_json::Value) -> Option<&serde_json::Map<String, serde_json::Value>> {
    if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
        return None;
    }
    message.get("metrics").and_then(serde_json::Value::as_object)
}

/// `modelInfo.id` (`deepseek/deepseek-v4-flash`, provider-prefixed) with the
/// older `modelId` (`cline-deepseek-v4-flash`) as fallback — verbatim either way,
/// pricing normalises the prefix.
fn model_id(message: &serde_json::Value) -> Option<String> {
    ["modelInfo", "model"]
        .iter()
        .filter_map(|key| message.get(*key))
        .filter_map(|v| v.as_str().map(str::to_string).or_else(|| v.get("id").and_then(serde_json::Value::as_str).map(str::to_string)))
        .chain(message.get("modelId").and_then(serde_json::Value::as_str).map(str::to_string))
        .find(|s| !s.is_empty())
}

/// `ts` in ms (`number` or digit `string`), else `timestamp`/`created_at`, which
/// older builds wrote as an ISO-8601 string.
fn timestamp_ms(message: &serde_json::Value) -> Option<i64> {
    for key in ["ts", "timestamp", "created_at"] {
        let Some(raw) = message.get(key) else { continue };
        if let Some(ms) = raw.as_str().and_then(usage_core::parse_ts_ms) {
            return Some(ms);
        }
        if let Some(n) = raw.as_u64() {
            // 13-digit ms written as a JSON number; `parse_ts_ms` uses the same
            // <1e10 ⇒ seconds rule, so route through it rather than duplicating.
            if let Some(ms) = usage_core::parse_ts_ms(&n.to_string()) {
                return Some(ms);
            }
        }
    }
    None
}

fn word(metrics: &serde_json::Map<String, serde_json::Value>, key: &str) -> u64 {
    metrics.get(key).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn assistant(i: u64, cr: u64, cw: u64, out: u64, ts: serde_json::Value, id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "role": "assistant", "ts": ts,
            "modelInfo": {"id": "deepseek/deepseek-v4-flash", "provider": "cline"},
            "metrics": {"inputTokens": i, "outputTokens": out, "cacheReadTokens": cr, "cacheWriteTokens": cw},
            "content": [{"type": "text", "text": "ok"}]
        })
    }

    #[test]
    fn maps_the_three_real_consecutive_calls() {
        // Verbatim first three assistant rows of 1787490736874_djat1.
        let (events, stats) = parse_transcript(&fixture("cline-djat1.messages.json"), "1787490736874_djat1", Some("apppty"), "src");
        assert_eq!(stats.events, 96);
        assert_eq!(events.len(), 96);
        let c = |i: usize| events[i].counts;
        assert_eq!(c(0), TokenCounts { input: 6133.0, cache_creation: 0.0, cache_read: 0.0, output: 145.0, reasoning: 0.0, credits: 0.0 });
        // 10 253 whole prompt of which 6 144 came from the cache ⇒ 4 109 new.
        assert_eq!(c(1), TokenCounts { input: 4109.0, cache_creation: 0.0, cache_read: 6144.0, output: 269.0, reasoning: 0.0, credits: 0.0 });
        assert_eq!(c(2), TokenCounts { input: 5490.0, cache_creation: 0.0, cache_read: 10240.0, output: 475.0, reasoning: 0.0, credits: 0.0 });
        // total() stays the honest sum of four disjoint stages.
        assert_eq!(c(1).total(), 10253.0 + 269.0);
        assert_eq!(events[1].ts_ms, 1_787_490_794_017, "ts is epoch milliseconds, not seconds");
        assert_eq!(events[1].model.as_deref(), Some("deepseek/deepseek-v4-flash"), "id stays verbatim, provider prefix included");
        assert_eq!(events[1].session, "1787490736874_djat1");
        assert_eq!(events[1].project.as_deref(), Some("apppty"));
        assert_eq!(events[1].meter, Meter::Tokens);
        assert!(events.iter().all(|e| e.quota.is_none() && e.calls.is_empty() && e.dedupe_key.is_some()));
        assert!(events.iter().all(|e| e.source == "src"), "the manifest key rides along");
        // Wire totals are what the session accumulator reports.
        assert_eq!((stats.wire_input, stats.wire_cache_read, stats.wire_cache_write, stats.wire_output), (11_816_157, 11_093_248, 0, 56_286));
    }

    #[test]
    fn input_tokens_contains_cache_read_tokens() {
        // Proof #1: containment over every row of every real transcript.
        let (text, dir) = (fixture("cline-djat1.messages.json"), "1787490736874_djat1");
        let root: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(root["messages"].is_array(), "fixture is a transcript");
        let (events, probe_stats) = parse_transcript(&text, dir, None, "src");
        assert_eq!(probe_stats.cache_split_violations, 0, "inputTokens >= cacheReadTokens on every row");
        let rows: Vec<(u64, u64)> = root["messages"].as_array().unwrap().iter().filter_map(assistant_metrics).map(|m| (word(m, "inputTokens"), word(m, "cacheReadTokens"))).collect();
        assert!(rows.iter().all(|(i, cr)| i >= cr));
        // Proof #2: the cached prefix of call t+1 is the WHOLE prompt of call t.
        let (mut whole, mut fresh) = (0, 0);
        for ((i, cr), (ni, ncr)) in rows.iter().zip(rows.iter().skip(1)) {
            if *ncr == 0 || *ni == 0 {
                continue;
            }
            if (*ncr as f64 - *i as f64).abs() <= 0.1 * (*i as f64) {
                whole += 1;
            }
            // The competing reading: only the uncached part of call t got cached.
            if (*ncr as f64 - (i - cr.min(i)) as f64).abs() <= 0.1 * (*i as f64) {
                fresh += 1;
            }
        }
        assert!(whole > 80, "cacheRead(t+1) should track input(t), got {whole} vs {fresh}");
        assert!(whole > 3 * fresh, "decisively: {whole} whole-prompt matches vs {fresh} uncached-only");
        // So mapping `inputTokens` verbatim would bill the cached prefix twice.
        let billed: f64 = events.iter().map(|e| e.counts.input + e.counts.cache_read).sum();
        assert_eq!(billed as u64, probe_stats.wire_input, "prompt stages add back to the wire prompt exactly once");
        assert_eq!(probe_stats.wire_input, 11_816_157);
        assert_eq!(probe_stats.mapped_counts().input, billed - probe_stats.wire_cache_read as f64, "only the uncached rest is billed as input");
    }

    #[test]
    fn emitted_tokens_equal_the_session_accumulator() {
        // metadata.usage == metadata.aggregateUsage == our wire sums, which is
        // why the meta file must never produce events.
        let meta: serde_json::Value = serde_json::from_str(&fixture("cline-djat1.json")).unwrap();
        let (events, stats) = parse_transcript(&fixture("cline-djat1.messages.json"), "1787490736874_djat1", None, "src");
        for key in ["usage", "aggregateUsage"] {
            let u = &meta["metadata"][key];
            assert_eq!(u["inputTokens"].as_u64(), Some(stats.wire_input), "{key}");
            assert_eq!(u["cacheReadTokens"].as_u64(), Some(stats.wire_cache_read), "{key}");
            assert_eq!(u["cacheWriteTokens"].as_u64(), Some(stats.wire_cache_write), "{key}");
            assert_eq!(u["outputTokens"].as_u64(), Some(stats.wire_output), "{key}");
        }
        assert_eq!(meta["metadata"]["usage"], meta["metadata"]["aggregateUsage"], "two copies of one accumulator");
        assert_eq!(events.len(), stats.assistant_with_metrics);
    }

    #[test]
    fn dedupe_key_is_scoped_to_the_session() {
        let build = |session: &str, ts: u64| {
            format!(
                "{{\"sessionId\":\"{session}\",\"messages\":[{},{}]}}",
                assistant(10, 0, 0, 1, serde_json::json!(ts), "msg_dup"),
                assistant(20, 15, 0, 2, serde_json::json!(ts + 1000), "msg_dup")
            )
        };
        let (a, _) = parse_transcript(&build("1_ts_a", 1_788_497_041_736), "1_ts_a", None, "src");
        let (b, _) = parse_transcript(&build("2_ts_b", 1_788_497_041_736), "2_ts_b", None, "src");
        assert_eq!(a[0].dedupe_key.as_deref(), Some("1_ts_a#msg_dup"));
        assert_eq!(b[0].dedupe_key.as_deref(), Some("2_ts_b#msg_dup"));
        assert_eq!(a[1].dedupe_key.as_deref(), Some("1_ts_a#msg_dup"), "…but a repeated id inside one session must collapse");
        assert_ne!(a[0].dedupe_key, b[0].dedupe_key, "a global unique index must not drop either session");
    }

    #[test]
    fn timestamp_and_model_dialects() {
        let text = format!(
            "{{\"messages\":[{iso},{numeric_string},{no_ts}]}}",
            iso = {
                let mut m = assistant(10, 0, 0, 1, serde_json::json!(1_788_497_041_736_u64), "m1");
                m.as_object_mut().unwrap().remove("ts");
                m["timestamp"] = serde_json::json!("2026-09-04T04:43:44.442Z");
                m
            },
            numeric_string = {
                let mut m = assistant(11, 0, 0, 1, serde_json::json!(1_788_497_041_737_u64), "m2");
                m["ts"] = serde_json::json!("1788497041737");
                m["modelId"] = serde_json::json!("cline-deepseek-v4-flash");
                m.as_object_mut().unwrap().remove("modelInfo");
                m
            },
            no_ts = assistant(12, 0, 0, 1, serde_json::Value::Null, "m3")
        );
        let (events, stats) = parse_transcript(&text, "s", None, "src");
        assert_eq!(events.len(), 2, "a row we cannot date is not billable: {events:?}");
        assert_eq!(stats.assistant_with_metrics, 3, "it is still counted");
        assert_eq!(events[0].ts_ms, 1_788_497_024_442, "ISO falls back through parse_ts_ms (UTC)");
        assert_eq!(events[1].ts_ms, 1_788_497_041_737, "13-digit string stays milliseconds");
        assert_eq!(events[1].model.as_deref(), Some("cline-deepseek-v4-flash"), "legacy modelId still resolves, verbatim");
    }

    #[test]
    fn payload_session_id_wins_over_the_file_name() {
        let text = format!("{{\"sessionId\":\"1788497024441_2hl7c\",\"messages\":[{}]}}", assistant(10, 0, 0, 1, serde_json::json!(1_788_497_041_736_u64), "msg_A"));
        let (events, _) = parse_transcript(&text, "some_renamed_dir", None, "src");
        assert_eq!(events[0].session, "1788497024441_2hl7c");
        assert_eq!(events[0].dedupe_key.as_deref(), Some("1788497024441_2hl7c#msg_A"), "renaming the dir must not mint new keys");
        let text = text.replace("1788497024441_2hl7c", "");
        let (events, _) = parse_transcript(&text, "1_ts_a", None, "src");
        assert_eq!(events[0].session, "1_ts_a", "empty payload sessionId falls back to the file");
    }

    #[test]
    fn non_usage_rows_and_garbage_are_skipped_not_panic() {
        let text = format!(
            "{{\"messages\":[{user},{tool},{no_metrics},{nulls},{bad_numbers}]}}",
            user = serde_json::json!({"id":"user_0","role":"user","ts":1_788_497_041_700_u64,"text":"hi"}),
            tool = serde_json::json!({"id":"tc_0","role":"tool_call","ts":1_788_497_041_710_u64}),
            no_metrics = serde_json::json!({"id":"msg_nm","role":"assistant","ts":1_788_497_041_720_u64}),
            nulls = serde_json::json!({"id":"msg_null","role":"assistant","ts":1_788_497_041_730_u64,"metrics":{"inputTokens":null,"cacheReadTokens":"x","cacheWriteTokens":5,"outputTokens":1.0001}}),
            bad_numbers = assistant(1, u64::from(u32::MAX), 0, 0, serde_json::json!(1_788_497_041_740_u64), "msg_big")
        );
        let (events, stats) = parse_transcript(&text, "s", None, "src");
        assert_eq!(events.len(), 2, "user / tool_call / metric-less rows never bill");
        assert_eq!(stats.assistant_with_metrics, 2);
        // Nulls and non-numbers read as zero, 1.0001 truncates towards the wire number.
        assert_eq!(events[0].counts, TokenCounts { input: 0.0, cache_creation: 5.0, cache_read: 0.0, output: 1.0, reasoning: 0.0, credits: 0.0 });
        assert_eq!(events[0].dedupe_key.as_deref(), Some("s#msg_null"));
        // An exclusive-shaped row would underflow: saturating_sub keeps input at 0
        // and flags the split instead of silently billing a negative prompt.
        assert_eq!(events[1].counts.input, 0.0);
        assert_eq!(events[1].counts.cache_read, 4_294_967_295.0);
        assert_eq!(stats.cache_split_violations, 1);
        for body in ["", "{", "null", "[]", "{\"messages\":\"nope\"}", "{\"messages\":[1,\"x\",null]}", &fixture("malformed.messages.json")] {
            let (e, s) = parse_transcript(body, "s", None, "src");
            assert!(e.is_empty() && s == Stats::default(), "{body:?}");
        }
    }

    #[test]
    fn money_is_never_taken_from_the_source() {
        let text = format!(
            "{{\"messages\":[{}]}}",
            {
                let mut m = assistant(100, 40, 0, 7, serde_json::json!(1_788_497_041_736_u64), "msg_cost");
                m["metrics"]["totalCost"] = serde_json::json!(0.4241);
                m["cost"] = serde_json::json!(9.9);
                m
            }
        );
        let (events, _) = parse_transcript(&text, "s", None, "src");
        assert_eq!(events[0].counts.credits, 0.0);
        assert_eq!(events[0].counts.total(), 60.0 + 40.0 + 7.0);
        assert!(COST_KEYS.iter().all(|k| fixture("cline-djat1.json").contains(k)), "the session meta publishes cost and we ignore all of it");
    }
}

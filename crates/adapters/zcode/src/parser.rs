//! Fallback reader: one `rollout/model-io-*.jsonl` line → one [`UsageEvent`].
//!
//! Only used when `db/db.sqlite` is missing (see [`crate::paths`] for why the db
//! is the primary source). The record is ZCode's request/response log; the token
//! numbers are taken **verbatim from the provider's own payload** at
//! `response.providerMetadata.<provider>.usage`, where the provider key varies, so
//! the map is iterated and the first entry holding an object `usage` wins.
//!
//! ## Stage mapping (measured, Anthropic-shaped)
//! `{input_tokens: 702, output_tokens: 43, cache_read_input_tokens: 381_888}` —
//! `input_tokens` already *excludes* the cached prefix (it is below
//! `cache_read_input_tokens` in 151 of 151 records), which is exactly how
//! [`TokenCounts`] defines its stages, so the four fields map 1:1 with no
//! subtraction. The db's normalised columns use the opposite convention and are
//! netted in [`crate::db`]; both paths end up billing the same request once
//! (proved by `db_and_rollout_agree_on_the_same_call`).
//!
//! `cacheCreationInputTokens` sits as a **sibling** of `usage` (not inside it) and
//! is null in every record measured; it is folded in only when non-null, and the
//! nested `cache_creation_input_tokens` wins if a provider ever sends both.
//!
//! `totalCost`/`cost`/`price` are ignored by design: money is computed centrally
//! from the shared price table so all tools stay comparable.

use usage_core::{Meter, TokenCounts, UsageEvent};

use crate::TOOL_ID;

/// A line that parses as a billable `model_io` call. `None` covers every
/// non-usage record type (`session_initialized`, `tool_result`, …), a malformed
/// line, and a call that reported no tokens (error / cancelled attempts).
pub fn event_from_line(line: &str, fallback_session: Option<&str>, source_key: &str) -> Option<UsageEvent> {
    // Pre-gate: records average 200 KB, and only usage lines carry this key.
    if !line.contains("\"providerMetadata\"") {
        return None;
    }
    let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
        return None;
    };
    event_from_record(&rec, fallback_session, source_key)
}

/// Same, from an already-parsed record (shared with the db reader's tests).
pub fn event_from_record(rec: &serde_json::Value, fallback_session: Option<&str>, source_key: &str) -> Option<UsageEvent> {
    if rec.get("type").and_then(serde_json::Value::as_str) != Some("model_io") {
        return None;
    }
    let (provider, body) = provider_usage(rec)?;
    let counts = stages(body, provider);
    let session = rec.get("sessionId").and_then(serde_json::Value::as_str).or(fallback_session).unwrap_or("zcode-unknown").to_string();
    let request_id = rec.get("requestId").and_then(serde_json::Value::as_str);
    // `attempt` is a real billed retry, so it is part of the identity: two rows
    // with the same `requestId` and different attempts are two requests.
    let attempt = rec.get("attempt").and_then(serde_json::Value::as_i64).unwrap_or(0);
    let ts_ms = timestamp(rec.get("completedAt")).or_else(|| timestamp(rec.get("startedAt")))?;
    let model = rec.get("model").and_then(|m| m.get("modelId")).and_then(serde_json::Value::as_str).or_else(|| rec.get("response").and_then(|r| r.get("modelId")).and_then(serde_json::Value::as_str));
    build_event(Call {
        session: &session,
        request_id,
        attempt,
        ts_ms,
        model,
        project: None,
        counts,
        source_key,
    })
}

/// The provider's verbatim usage object: iterate `providerMetadata` and take the
/// first entry that carries an object `usage`.
fn provider_usage(rec: &serde_json::Value) -> Option<(&str, &serde_json::Map<String, serde_json::Value>)> {
    let pm = rec.get("response")?.get("providerMetadata")?.as_object()?;
    pm.iter().find_map(|(key, body)| {
        let body = body.as_object()?;
        body.get("usage").filter(|u| u.is_object()).map(|_| (key.as_str(), body))
    })
}

/// Provider payload → the four mutually exclusive stages.
fn stages(body: &serde_json::Map<String, serde_json::Value>, provider: &str) -> TokenCounts {
    let usage_value = body.get("usage").expect("checked by the caller");
    let usage = usage_value.as_object().expect("checked by the caller");
    fn number(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> f64 {
        map.get(key).and_then(as_count).unwrap_or(0.0)
    }
    fn dig<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
        path.iter().try_fold(value, |cur, key| cur.get(*key))
    }
    // Anthropic names first, OpenAI names second; `prompt_tokens` includes the
    // cached part, so it is netted to keep the stages disjoint.
    let cached = number(usage, "cache_read_input_tokens").max(dig(usage_value, &["prompt_tokens_details", "cached_tokens"]).and_then(as_count).unwrap_or(0.0));
    let prompt = if usage.contains_key("input_tokens") { number(usage, "input_tokens") } else { number(usage, "prompt_tokens") - cached };
    let output = if usage.contains_key("output_tokens") { number(usage, "output_tokens") } else { number(usage, "completion_tokens") };
    let reasoning = number(usage, "reasoning_tokens").max(dig(usage_value, &["output_tokens_details", "reasoning_tokens"]).and_then(as_count).unwrap_or(0.0));
    let cache_creation = number(usage, "cache_creation_input_tokens")
        .max(dig(usage_value, &["cache_creation"]).and_then(as_count).unwrap_or(0.0))
        // The sibling of `usage`, null in every record measured so far.
        .max(body.get("cacheCreationInputTokens").and_then(as_count).unwrap_or(0.0));
    debug_assert!(!provider.is_empty());
    TokenCounts {
        input: prompt.max(0.0),
        cache_creation: cache_creation.max(0.0),
        cache_read: cached.max(0.0),
        output: output.max(0.0),
        // `reasoning` is a sub-breakdown of `output` and never added on top.
        reasoning: reasoning.min(output),
        credits: 0.0,
    }
}

fn as_count(value: &serde_json::Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_u64().map(|n| n as f64))
}

/// `completedAt`/`startedAt` are ISO-8601 with milliseconds in the current build
/// (`"2026-09-23T00:17:50.359Z"`) and 13-digit epoch ms in older ones; both go
/// through [`usage_core::parse_ts_ms`], whose <1e10 cutoff keeps seconds honest
/// and leaves milliseconds untouched.
fn timestamp(value: Option<&serde_json::Value>) -> Option<i64> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return usage_core::parse_ts_ms(text);
    }
    value.as_u64().and_then(|n| usage_core::parse_ts_ms(&n.to_string()))
}

/// One billed call, as both readers see it.
pub(crate) struct Call<'a> {
    pub session: &'a str,
    pub request_id: Option<&'a str>,
    pub attempt: i64,
    pub ts_ms: i64,
    pub model: Option<&'a str>,
    pub project: Option<String>,
    pub counts: TokenCounts,
    pub source_key: &'a str,
}

/// The single constructor both readers use, so the dedupe format and the
/// zero-usage rule cannot drift apart.
pub(crate) fn build_event(call: Call<'_>) -> Option<UsageEvent> {
    let Call { session, request_id, attempt, ts_ms, model, project, counts, source_key } = call;
    if counts.is_zero() {
        // Error / cancelled attempts report every stage as 0.
        return None;
    }
    let mut event = UsageEvent::new(TOOL_ID, ts_ms, session).with(counts);
    event.meter = Meter::Tokens;
    event.model = model.map(str::to_string);
    event.project = project;
    // `attempt` is a separate billed request, and the id alone is not enough
    // because the index is global: `<session>#<requestId>#<attempt>`.
    event.dedupe_key = request_id.map(|rid| format!("{session}#{rid}#{attempt}"));
    event.source = source_key.to_string();
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn record(session: &str, rid: &str, attempt: u64, usage: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "model_io", "sessionId": session, "turnId": "turn_1", "traceId": "trace-1",
            "requestId": rid, "attempt": attempt,
            "startedAt": "2026-09-23T00:17:50.359Z", "completedAt": "2026-09-23T00:17:55.834Z",
            "durationMs": 5475,
            "model": {"modelId": "GLM-5.3-Flash", "providerId": "account:bigmodel-individual-coding-plan"},
            "querySource": "main_turn", "modelIOReset": null,
            "request": {"body": {"model": "GLM-5.3-Flash", "max_tokens": 128000}},
            "response": {"modelId": "GLM-5.3-Flash", "providerMetadata": {"anthropic": {"usage": usage, "cacheCreationInputTokens": null}}}
        })
    }

    #[test]
    fn maps_the_real_first_record_field_for_field() {
        // Verbatim usage of the first record of the real 2026-09-23 session.
        let (events, _) = parse_lines(&fixture("rollout-real.jsonl"), None, "src");
        assert_eq!(events.len(), 8, "ten lines: eight calls, one non-usage record, one torn: {events:?}");
        let e = &events[0];
        assert_eq!(e.counts, TokenCounts { input: 702.0, cache_creation: 0.0, cache_read: 381_888.0, output: 43.0, reasoning: 0.0, credits: 0.0 });
        assert_eq!(e.counts.total(), 702.0 + 381_888.0 + 43.0, "the cached prefix is a stage of its own");
        assert_eq!(e.tool, "zcode");
        assert_eq!(e.session, "sess_7de7ea43-96fe-4efe-9bdb-4afd85127203");
        assert_eq!(e.model.as_deref(), Some("GLM-5.3-Flash"), "model id stays verbatim, no invented mapping");
        assert_eq!(e.dedupe_key.as_deref(), Some("sess_7de7ea43-96fe-4efe-9bdb-4afd85127203#b0df2753-2722-4c18-a743-748c9c3fbdca#1"));
        assert_eq!(e.ts_ms, 1_790_122_675_834, "completedAt, ISO with millisecond precision");
        assert_eq!(e.project, None, "the rollout log knows no cwd; only the db does");
        assert_eq!(e.source, "src");
        assert_eq!(e.meter, Meter::Tokens);
        assert!(e.quota.is_none() && e.calls.is_empty());
        assert_eq!(events[2].counts.input, 388.0);
        assert_eq!(events[2].counts.reasoning, 0.0);
    }

    #[test]
    fn input_tokens_excludes_the_cached_prefix() {
        // 151/151 records on disk have input_tokens < cache_read_input_tokens,
        // which is only possible when the two are disjoint stages.
        let (events, _) = parse_lines(&fixture("rollout-real.jsonl"), None, "src");
        assert!(events.iter().all(|e| e.counts.input < e.counts.cache_read), "{:?}", events.iter().map(|e| e.counts.input).collect::<Vec<_>>());
        let input: f64 = events.iter().map(|e| e.counts.input).sum();
        let cached: f64 = events.iter().map(|e| e.counts.cache_read).sum();
        assert_eq!((input, cached), (4120.0, 3_071_232.0), "Σ input_tokens vs Σ cache_read_input_tokens of the real file");
        assert!(cached > 100.0 * input, "a warm context is billed as cache, not as input");
    }

    #[test]
    fn repeated_request_id_and_attempt_are_two_events_only_when_the_attempt_differs() {
        // Real data has 151 distinct (requestId, attempt) pairs, so the retry
        // case below is built from a real record with `attempt` bumped.
        // The three lines are the same real record: attempt 1, attempt 2 of the
        // same requestId, and a second copy of attempt 2's log line.
        let (events, _) = parse_lines(&fixture("rollout-retry.jsonl"), None, "src");
        assert_eq!(events.len(), 3);
        let keys: Vec<_> = events.iter().map(|e| e.dedupe_key.clone()).collect();
        assert_eq!(keys[0].as_deref().unwrap(), "sess_retry_probe#req_retry_probe#1");
        assert_eq!(keys[1].as_deref().unwrap(), "sess_retry_probe#req_retry_probe#2");
        assert_eq!(keys[1], keys[2], "a duplicated log line is the same billed call");
        assert_ne!(keys[0], keys[1], "a retried attempt is a separate request");
        assert_eq!(events[1].counts, events[0].counts, "both attempts were charged the same prompt");
        assert_eq!(events[1].ts_ms, events[0].ts_ms);
    }

    #[test]
    fn null_and_missing_cache_creation_and_other_provider_keys() {
        // `cacheCreationInputTokens` is a sibling of `usage` and null in all 151
        // real records; a non-null value must land in cache_creation.
        let mut rec = record("s", "r1", 1, serde_json::json!({"input_tokens": 10, "output_tokens": 3, "cache_read_input_tokens": 5}));
        assert_eq!(event_from_record(&rec, None, "src").unwrap().counts.cache_creation, 0.0);
        rec["response"]["providerMetadata"]["anthropic"]["cacheCreationInputTokens"] = serde_json::json!(4096);
        assert_eq!(event_from_record(&rec, None, "src").unwrap().counts.cache_creation, 4096.0);
        // A nested Anthropic field wins over the sibling.
        rec["response"]["providerMetadata"]["anthropic"]["usage"]["cache_creation_input_tokens"] = serde_json::json!(77);
        assert_eq!(event_from_record(&rec, None, "src").unwrap().counts.cache_creation, 4096.0, "max of the two, never the sum");
        // The provider key varies: this one is not called "anthropic".
        let other = serde_json::json!({"type":"model_io","sessionId":"s","requestId":"r2","attempt":1,"completedAt":1_789_411_075_834_u64,"model":{"modelId":"GLM-5.3-Flash"},"response":{"providerMetadata":{"openai":{"usage":{"prompt_tokens":1000,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":900},"output_tokens_details":{"reasoning_tokens":8}}}}}});
        let e = event_from_record(&other, None, "src").unwrap();
        assert_eq!(e.counts, TokenCounts { input: 100.0, cache_creation: 0.0, cache_read: 900.0, output: 20.0, reasoning: 8.0, credits: 0.0 });
        assert_eq!(e.ts_ms, 1_789_411_075_834, "13-digit epoch ms is also accepted");
        // Empty / absent usage never bills.
        for usage in [serde_json::json!({}), serde_json::json!(null), serde_json::json!("")] {
            let rec = record("s", "r3", 1, usage);
            assert!(event_from_record(&rec, None, "src").is_none());
        }
        let mut no_pm = record("s", "r4", 1, serde_json::json!({"input_tokens": 1}));
        no_pm["response"].as_object_mut().unwrap().remove("providerMetadata");
        assert!(event_from_record(&no_pm, None, "src").is_none());
    }

    #[test]
    fn malformed_and_other_record_types_never_bill() {
        let (events, _) = parse_lines(&fixture("rollout-garbage.jsonl"), None, "src");
        assert!(events.is_empty());
        // A torn final line without a trailing newline must not panic either.
        let torn = "{\"type\":\"model_io\",\"response\":{\"providerMetadata\":{\"anthropic\":{\"usage\":{\"input_tokens\":1,";
        assert!(event_from_line(torn, Some("sess_torn"), "src").is_none());
        assert!(event_from_line("", None, "src").is_none());
        assert!(event_from_line("\u{1f600}", None, "src").is_none());
        let non_usage = serde_json::json!({"type":"session_initialized","sessionId":"s","response":{"providerMetadata":{"anthropic":{"usage":{"input_tokens":5,"output_tokens":1}}}}});
        assert!(event_from_record(&non_usage, None, "src").is_none(), "only model_io rows are calls");
    }

    #[test]
    fn fallback_session_and_cost_ignorance() {
        let line = serde_json::to_string(&record("s", "r5", 1, serde_json::json!({"input_tokens": 4, "output_tokens": 2, "cache_read_input_tokens": 1, "totalCost": 9.99, "cost": 3}))).unwrap();
        let e = event_from_line(&line, Some("sess_from_file_name"), "src").unwrap();
        assert_eq!(e.session, "s", "the record's own sessionId wins");
        let line = line.replace("\"sessionId\":\"s\",", "");
        let e = event_from_line(&line, Some("sess_from_file_name"), "src").unwrap();
        assert_eq!(e.session, "sess_from_file_name");
        assert_eq!(e.dedupe_key.as_deref(), Some("sess_from_file_name#r5#1"));
        assert_eq!(e.counts.credits, 0.0, "the source's own money is never read");
        assert_eq!(e.counts.total(), 4.0 + 1.0 + 2.0, "input_tokens is already the uncached part here");
        // Missing attempt defaults to 0 so the key stays stable for the record.
        let line = line.replace("\"attempt\":1,", "");
        let e = event_from_line(&line, None, "src").unwrap();
        assert_eq!(e.dedupe_key.as_deref(), Some("zcode-unknown#r5#0"), "no sessionId and no fallback ⇒ a shared placeholder");
    }

    fn parse_lines(text: &str, fallback_session: Option<&str>, source_key: &str) -> (Vec<UsageEvent>, usize) {
        let mut visited = 0;
        let events = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .inspect(|_| visited += 1)
            .filter_map(|l| event_from_line(l, fallback_session, source_key))
            .collect();
        (events, visited)
    }
}

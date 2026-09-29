//! The v4 session projection: where format v4 actually keeps its numbers.
//!
//! Measured on Windows 2026-09-29 across three live sessions: the v4 stream
//! (`session.v4.jsonl.zstd`) stays a one-line header forever — the writer
//! keeps durable state in the session projection instead, one JSON per
//! session under `storages/session_projcache/sessions/<id>.json`:
//!
//! - `record.identity` — `formatVersion`, `createdAt`, `cwd`
//! - `rows.tokenUsage.val.totals` — the session's cumulative
//!   `{uncachedInputTokens, outputTokens, cacheReadTokens, cacheWriteTokens}`
//!   (the same exclusive convention the v3 stream used)
//! - `rows.modelSelection.val.lastUsed.model` — the model, when one ran
//! - `rows.sessionListMetadata.val.lastPromptAt` — epoch ms of the last prompt
//!
//! One cumulative event per session, stable `…#proj` dedupe key: the indexer
//! replaces on the key, so a livewriting projection re-reads clean and the
//! cursor (Tree mtime) may replay it freely. A projection with all-zero
//! totals (session opened, nothing answered) yields nothing.
//!
//! v3 sessions (macOS) keep their per-call streams; their projections are
//! never read — the filename decides which side speaks, so a session is
//! never billed twice.

use std::path::Path;

use serde_json::Value;
use usage_core::{TokenCounts, UsageEvent};

use crate::TOOL_ID;

/// Parse one projection-cache file. The session id is not in the JSON —
/// `record.identity` carries only format/created/cwd — so the loader hands it
/// in from the filename (`session-<id>.json`). `mtime_ms` is the fallback
/// event time for a projection that carries no `lastPromptAt` yet.
pub fn parse(text: &str, session: &str, mtime_ms: i64, source_key: &str) -> Option<Box<UsageEvent>> {
    let v: Value = serde_json::from_str(text).ok()?;
    let identity = v.pointer("/record/identity")?;
    let totals = v.pointer("/record/rows/tokenUsage/val/totals")?;
    let num = |k: &str| totals.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
    let counts = TokenCounts {
        input: num("uncachedInputTokens"),
        cache_creation: num("cacheWriteTokens"),
        cache_read: num("cacheReadTokens"),
        output: num("outputTokens"),
        reasoning: 0.0,
        credits: 0.0,
    };
    if counts.is_zero() {
        return None;
    }
    // `lastPromptAt` is when the session was last actually used; a projection
    // that never answered falls back to the file's own mtime.
    let ts_ms = v
        .pointer("/record/rows/sessionListMetadata/val/lastPromptAt")
        .and_then(Value::as_i64)
        .filter(|ts| *ts > 0)
        .unwrap_or(mtime_ms);
    let mut event = UsageEvent::new(TOOL_ID, ts_ms, session);
    event.counts = counts;
    event.meter = usage_core::Meter::Tokens;
    event.model = v
        .pointer("/record/rows/modelSelection/val/lastUsed/model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    event.project = identity
        .get("cwd")
        .and_then(Value::as_str)
        .and_then(|c| Path::new(c).file_name())
        .and_then(|n| n.to_str())
        .map(str::to_string);
    event.dedupe_key = Some(format!("{session}#proj"));
    event.source = source_key.to_string();
    Some(Box::new(event))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r#"{
      "version": 7,
      "record": {
        "identity": {"formatVersion": 4, "createdAt": 1790666157422, "cwd": "C:\\Users\\demo\\ai-record"},
        "rows": {
          "tokenUsage": {"ver": 2, "seq": 51, "val": {"totals": {
              "uncachedInputTokens": 28147, "outputTokens": 1219,
              "cacheReadTokens": 37120, "cacheWriteTokens": 0},
              "last": {"turn": 2, "step": 4}}},
          "modelSelection": {"ver": 2, "seq": 51, "val": {"lastUsed": {
              "provider": "deepseek-official", "model": "deepseek-flash",
              "reasoningEffort": "high"}, "pending": null}},
          "sessionListMetadata": {"ver": 1, "seq": 51, "val": {"blank": false, "lastPromptAt": 1790667070477}}
        }
      }
    }"#;

    #[test]
    fn the_projection_becomes_one_cumulative_event() {
        let event = parse(LIVE, "b70faa8e", 1790667000000, "proj#b70").expect("a used session yields an event");
        assert_eq!(event.session, "b70faa8e");
        assert_eq!(event.model.as_deref(), Some("deepseek-flash"));
        assert_eq!(event.project.as_deref(), Some("ai-record"));
        assert_eq!(event.counts.input, 28147.0);
        assert_eq!(event.counts.output, 1219.0);
        assert_eq!(event.counts.cache_read, 37120.0);
        assert_eq!(event.ts_ms, 1790667070477, "lastPromptAt wins over mtime");
        assert_eq!(event.dedupe_key.as_deref(), Some("b70faa8e#proj"));
    }

    #[test]
    fn an_unused_session_yields_nothing_and_the_timestamp_falls_back() {
        let unused = r#"{"record":{"identity":{"formatVersion":4,"createdAt":1,"cwd":"C:\\w"},"rows":{
            "tokenUsage":{"val":{"totals":{"uncachedInputTokens":0,"outputTokens":0,"cacheReadTokens":0,"cacheWriteTokens":0}}},
            "sessionListMetadata":{"val":{}}}}}"#;
        assert!(parse(unused, "s0", 42, "k").is_none(), "zero totals are not an event");

        let no_timestamp = r#"{"record":{"identity":{"formatVersion":4,"createdAt":1,"cwd":"C:\\w"},"rows":{
            "tokenUsage":{"val":{"totals":{"uncachedInputTokens":5,"outputTokens":1,"cacheReadTokens":0,"cacheWriteTokens":0}}}}}}"#;
        let event = parse(no_timestamp, "s1", 77, "k").expect("usage without a prompt timestamp still bills");
        assert_eq!(event.ts_ms, 77, "mtime is the fallback clock");
        assert!(event.model.is_none());
    }
}

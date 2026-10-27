//! Streaming parse of one decompressed session stream.
//!
//! Three lines matter; everything else (tool payloads, reasoning chunks,
//! snapshots) is noise the usage math never sees:
//!
//! - `{"type":"session", id, cwd, …}` — session identity and project path
//! - `{"type":"request/header", data.header.config.model}` — the step's model,
//!   carried forward until the next header
//! - `{"type":"assistant/chunk", seq, time, data.chunk =
//!   {"type":"usage", usage:{inputTokens, outputTokens, cacheReadTokens,
//!   reasoningTokens}}}` — one billed call, the envelope of the oldest clients
//! - `{"type":"assistant/message", seq, time, data.usage:{inputTokens,
//!   outputTokens, cacheReadTokens, cacheWriteTokens}}` — the envelope every
//!   current DSH client writes; DSH v3 writes it too (there with
//!   `reasoningTokens` as a sub-breakdown of `output` and no cache write), so the
//!   client version never decides how a call is read. `inputTokens` is uncached
//!   input in both (verified live: the per-call sums equal the projection's
//!   `uncachedInputTokens + cacheReadTokens + outputTokens` exactly, so nothing
//!   is counted twice and nothing hides)
//!
//! `seq` is the dedupe half-key, `time` the wall clock — the day a call is
//! billed to is the day it actually happened, on every envelope.
//!
//! ## `v3` / `v4` are DSH's own versions
//! The `version` a session header carries — and the same number in the file
//! name, `session.v3.jsonl.zstd` / `session.v4.jsonl.zstd` — is the DSH client
//! version that wrote the stream, not a distinction between two billing
//! formats: both versions bill through `assistant/message`. Reading it as a
//! format switch is what made DSH v3's calls look like a foreign envelope.
//!
//! ## Forked sessions replay their parent
//! The writer forks a session whenever the user resumes one: the new stream's
//! header says `"isSeeded": true` with `"parentSession": "<parent id>"`, and
//! the parent's events are re-emitted verbatim — same `seq`, same `time`, same
//! tokens — before (and, when the fork was itself resumed, between) the fork's
//! own calls. The replay closes with `session/end-seed` carrying
//! `data.inherited: true`; a stream can carry several such markers, and only the
//! LAST one ends the inherited range (measured 2026-10-27: fork `0f9b6ecd`'s
//! 19 calls after its first marker were still the parent's, all 19 present in
//! the parent's stream; `35e1d11f` had 466 of 487). Billing that range beside
//! the parent counts those tokens twice — 144.31M reported against a true 87.0M
//! on that day. The loader therefore skips the range when the parent stream is
//! on disk (it is what the replay copies, and it bills itself); a fork whose
//! parent is gone keeps the replay, since it is then the only copy.


use std::path::Path;

use serde_json::Value;
use usage_core::{TokenCounts, UsageEvent};

use crate::TOOL_ID;

/// Streaming state across one session's lines.
#[derive(Default)]
pub struct Stream {
    session: Option<String>,
    project: Option<String>,
    model: Option<String>,
}

pub enum Parsed {
    /// A usage event for the index.
    Event(Box<UsageEvent>),
    /// A line that carries no billable call (headers, chunks, snapshots…).
    Noise,
}

impl Stream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume one JSONL line. `source_key` is the file key the index manifest
    /// is keyed on; `seq` disambiguates lines within it.
    pub fn line(&mut self, line: &str, source_key: &str) -> Parsed {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return Parsed::Noise;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                self.session = v.get("id").and_then(Value::as_str).map(str::to_string);
                self.project = v
                    .get("cwd")
                    .and_then(Value::as_str)
                    .and_then(|c| Path::new(c).file_name())
                    .and_then(|n| n.to_str())
                    .map(str::to_string);
                Parsed::Noise
            }
            Some("request/header") => {
                self.model = v
                    .pointer("/data/header/config/model")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Parsed::Noise
            }
            Some("assistant/chunk") => self.usage_chunk(&v, source_key),
            // DSH v4 bills here, and so does v3: both versions write
            // `assistant/message` + `data.usage`, with `reasoningTokens` as a
            // sub-breakdown of `output` on v3 (measured 2026-10-27: all 10,421
            // DSH v3 calls carry this shape, 55,611 input / 0 cacheRead / 595
            // output / 494 reasoning on a sample step). Only the oldest clients
            // used the `assistant/chunk` envelope above, which stays for them.
            Some("assistant/message") => self.message_usage(&v, source_key),
            _ => Parsed::Noise,
        }
    }

    fn usage_chunk(&self, v: &Value, source_key: &str) -> Parsed {
        let chunk = match v.pointer("/data/chunk") {
            Some(c) if c.get("type").and_then(Value::as_str) == Some("usage") => c,
            _ => return Parsed::Noise,
        };
        let usage = match chunk.get("usage") {
            Some(u) if u.is_object() => u,
            _ => return Parsed::Noise,
        };
        let num = |k: &str| usage.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        let output = num("outputTokens");
        let counts = TokenCounts {
            input: num("inputTokens"),
            cache_creation: 0.0,
            cache_read: num("cacheReadTokens"),
            output,
            // A sub-breakdown of `output`, clamped so it never inflates a total.
            reasoning: num("reasoningTokens").min(output),
            credits: 0.0,
        };
        self.event(counts, v, source_key)
    }

    /// The billed call: usage rides `data.usage` directly, with the cache write
    /// the `assistant/chunk` envelope never carried. `totalTokens` in the wire is
    /// the sum of the others and is not read — the index sums its own stages.
    fn message_usage(&self, v: &Value, source_key: &str) -> Parsed {
        let usage = match v.pointer("/data/usage") {
            Some(u) if u.is_object() => u,
            _ => return Parsed::Noise,
        };
        let num = |k: &str| usage.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        let output = num("outputTokens");
        let counts = TokenCounts {
            input: num("inputTokens"),
            cache_creation: num("cacheWriteTokens"),
            cache_read: num("cacheReadTokens"),
            output,
            // A sub-breakdown of `output` on DSH v3, absent in v4; clamped
            // so it never inflates a total.
            reasoning: num("reasoningTokens").min(output),
            credits: 0.0,
        };
        self.event(counts, v, source_key)
    }

    /// The shared tail of both envelopes: zero-usage lines are noise, and the
    /// rest becomes one event stamped with the line's own wall clock.
    fn event(&self, counts: TokenCounts, v: &Value, source_key: &str) -> Parsed {
        if counts.is_zero() {
            return Parsed::Noise;
        }
        let seq = v.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let session = match &self.session {
            Some(s) => s.clone(),
            None => return Parsed::Noise,
        };
        let mut event = UsageEvent::new(TOOL_ID, v.get("time").and_then(Value::as_i64).unwrap_or(0), &session)
            .with(counts);
        event.meter = usage_core::Meter::Tokens;
        event.model = self.model.clone();
        event.project = self.project.clone();
        event.dedupe_key = Some(format!("{session}#{seq}"));
        event.source = format!("{source_key}#{seq}");
        Parsed::Event(Box::new(event))
    }
}

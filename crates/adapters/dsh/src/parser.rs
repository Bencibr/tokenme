//! Streaming parse of one decompressed session stream.
//!
//! Three lines matter; everything else (tool payloads, reasoning chunks,
//! snapshots) is noise the usage math never sees:
//!
//! - `{"type":"session", id, cwd, …}` — session identity and project path
//! - `{"type":"request/header", data.header.config.model}` — the step's model,
//!   carried forward until the next header
//! - format v3: `{"type":"assistant/chunk", seq, time, data.chunk =
//!   {"type":"usage", usage:{inputTokens, outputTokens, cacheReadTokens,
//!   reasoningTokens}}}` — one billed call
//! - format v4: `{"type":"assistant/message", seq, time,
//!   data.usage:{inputTokens, outputTokens, cacheReadTokens,
//!   cacheWriteTokens}}` — same role, different envelope, and `inputTokens`
//!   is uncached input there too (verified live: the per-call sums equal the
//!   projection's `uncachedInputTokens + cacheReadTokens + outputTokens`
//!   exactly, so nothing is counted twice and nothing hides)
//!
//! `seq` is the dedupe half-key, `time` the wall clock — the day a call is
//! billed to is the day it actually happened, on both formats.


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
    /// The session line's `version` decides the usage envelope: v3 bills on
    /// `assistant/chunk`, v4 on `assistant/message`. A stream that named no
    /// version is parsed as v3 — the older writer.
    v4: bool,
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
                self.v4 = v.get("version").and_then(Value::as_i64) == Some(4);
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
            Some("assistant/message") if self.v4 => self.message_usage(&v, source_key),
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

    /// The v4 billed call: usage rides `data.usage` directly, with the cache
    /// write the v3 envelope never carried. `totalTokens` in the wire is the
    /// sum of the others and is not read — the index sums its own stages.
    fn message_usage(&self, v: &Value, source_key: &str) -> Parsed {
        let usage = match v.pointer("/data/usage") {
            Some(u) if u.is_object() => u,
            _ => return Parsed::Noise,
        };
        let num = |k: &str| usage.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        let counts = TokenCounts {
            input: num("inputTokens"),
            cache_creation: num("cacheWriteTokens"),
            cache_read: num("cacheReadTokens"),
            output: num("outputTokens"),
            reasoning: 0.0,
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

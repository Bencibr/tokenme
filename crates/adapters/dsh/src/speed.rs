//! Best-effort request timing from DSH's persisted session event streams.
//!
//! Usage rows are completion records. DSH's newer `assistant/message` record
//! also carries the streamed message parts, which can provide first-content
//! timing. Older streams may have only a terminal usage chunk; those can give
//! request duration, but are deliberately not promoted to a TTFT sample.

use std::path::Path;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct DshSpeedSample {
    pub model: Option<String>,
    pub completed_at_ms: i64,
    pub ttft_ms: f64,
    pub duration_ms: f64,
    pub output_tokens: f64,
    pub tps: Option<f64>,
}

#[derive(Default)]
struct Pending {
    started_at_ms: Option<i64>,
    first_token_at_ms: Option<i64>,
    model: Option<String>,
}

/// Read measured speed samples from one DSH compressed JSONL session file.
/// Prompt and response text are never copied into the returned samples.
pub fn read_speed_samples(path: &Path) -> std::io::Result<Vec<DshSpeedSample>> {
    let bytes = std::fs::read(path)?;
    let Ok(decoded) = zstd::stream::decode_all(&bytes[..]) else { return Ok(Vec::new()) };
    Ok(parse_speed_stream(&decoded))
}

fn parse_speed_stream(bytes: &[u8]) -> Vec<DshSpeedSample> {
    let mut pending = Pending::default();
    let mut samples = Vec::new();
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let Ok(event) = serde_json::from_slice::<Value>(line) else { continue };
        let Some(kind) = event.get("type").and_then(Value::as_str) else { continue };
        let event_ms = event.get("time").and_then(Value::as_i64);
        match kind {
            "step/start" => {
                pending.started_at_ms = event_ms;
                pending.first_token_at_ms = None;
            }
            "request/header" => {
                if let Some(start) = event_ms { pending.started_at_ms = Some(start); }
                pending.first_token_at_ms = None;
                pending.model = event.pointer("/data/header/config/model")
                    .and_then(Value::as_str).map(str::to_owned);
            }
            "request/context" => {
                if let Some(model) = event.pointer("/data/model").and_then(Value::as_str) {
                    pending.model = Some(model.to_owned());
                }
            }
            "assistant/chunk" => {
                let chunk = event.pointer("/data/chunk");
                if chunk.and_then(|v| v.get("type")).and_then(Value::as_str) == Some("usage") {
                    if let Some(usage) = chunk.and_then(|v| v.get("usage")) {
                        let output = number(usage.get("outputTokens"));
                        if let Some(sample) = finish(&pending, event_ms, output) { samples.push(sample); }
                    }
                } else if pending.first_token_at_ms.is_none() {
                    pending.first_token_at_ms = event_ms;
                }
            }
            "assistant/message" => {
                let data = event.get("data");
                if let Some(usage) = data.and_then(|v| v.get("usage")) {
                    let completed = event_ms;
                    let first = data.and_then(|v| v.get("stream"))
                        .and_then(first_stream_token_ms)
                        .or_else(|| data.and_then(|v| v.get("message")).and_then(first_stream_token_ms))
                        .or(pending.first_token_at_ms);
                    if let (Some(_start), Some(completed), Some(first)) = (pending.started_at_ms, completed, first) {
                        pending.first_token_at_ms = Some(first);
                        let output = number(usage.get("outputTokens"));
                        if let Some(sample) = finish(&pending, Some(completed), output) { samples.push(sample); }
                    }
                }
            }
            _ => {}
        }
    }
    samples
}

fn finish(pending: &Pending, completed_at_ms: Option<i64>, output_tokens: f64) -> Option<DshSpeedSample> {
    let start = pending.started_at_ms?;
    let completed = completed_at_ms?;
    let first = pending.first_token_at_ms?;
    let duration_ms = (completed - start) as f64;
    let ttft_ms = (first - start) as f64;
    if duration_ms <= 0.0 || ttft_ms < 0.0 || ttft_ms > duration_ms { return None; }
    let decode_ms = duration_ms - ttft_ms;
    let tps = (decode_ms > 0.0 && output_tokens > 0.0)
        .then(|| output_tokens / (decode_ms / 1000.0))
        .filter(|value| value.is_finite() && (0.1..=2000.0).contains(value));
    Some(DshSpeedSample { model: pending.model.clone(), completed_at_ms: completed, ttft_ms, duration_ms, output_tokens, tps })
}

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0).max(0.0)
}

/// DSH format v2+ embeds typed stream chunks under the assistant message.
/// Restrict timing lookup to those known stream branches and never inspect or
/// retain text payloads.
fn first_stream_token_ms(stream: &Value) -> Option<i64> {
    let mut earliest = None;
    find_stream_branches(stream, &mut earliest, 0);
    earliest
}

fn find_stream_branches(value: &Value, earliest: &mut Option<i64>, depth: usize) {
    if depth > 10 { return; }
    if let Value::Object(fields) = value {
        if fields.get("type").and_then(Value::as_str).is_some_and(|kind| matches!(kind, "text-chunks" | "reasoning-chunks" | "tool-call-chunks")) {
            collect_chunk_time(value, earliest, 0);
            return;
        }
        for (key, child) in fields {
            if matches!(key.as_str(), "text-chunks" | "reasoning-chunks" | "tool-call-chunks") {
                collect_chunk_time(child, earliest, 0);
            } else if child.is_object() || child.is_array() {
                find_stream_branches(child, earliest, depth + 1);
            }
        }
    } else if let Value::Array(items) = value {
        for item in items { find_stream_branches(item, earliest, depth + 1); }
    }
}

fn collect_chunk_time(value: &Value, earliest: &mut Option<i64>, depth: usize) {
    if depth > 8 { return; }
    match value {
        Value::Array(items) => {
            for item in items { collect_chunk_time(item, earliest, depth + 1); }
        }
        Value::Object(fields) => {
            if fields.get("type").and_then(Value::as_str).is_some_and(|kind| matches!(kind, "text-chunks" | "reasoning-chunks" | "tool-call-chunks")) {
                let timestamp = fields.get("time0").or_else(|| fields.get("time"))
                    .or_else(|| fields.get("timestamp")).or_else(|| fields.get("ts"))
                    .and_then(Value::as_i64);
                if let Some(timestamp) = timestamp {
                    *earliest = Some((*earliest).map_or(timestamp, |old| old.min(timestamp)));
                }
            }
            for key in ["time", "timestamp", "ts"] {
                let timestamp = fields.get(key).and_then(Value::as_i64);
                if let Some(timestamp) = timestamp {
                    *earliest = Some((*earliest).map_or(timestamp, |old| old.min(timestamp)));
                }
            }
            for (key, child) in fields {
                if matches!(key.as_str(), "text-chunks" | "reasoning-chunks" | "tool-call-chunks") {
                    collect_chunk_time(child, earliest, depth + 1);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_stream_produces_ttft_duration_and_decode_tps_without_content() {
        let lines = [
            r#"{"type":"step/start","time":1000}"#,
            r#"{"type":"request/header","time":1010,"data":{"header":{"config":{"model":"deepseek-v4-flash"}}}}"#,
            r#"{"type":"assistant/message","time":6010,"data":{"stream":[{"type":"reasoning-chunks","time0":1510,"time":3000,"texts":["private"]}],"usage":{"outputTokens":450}}}"#,
        ];
        let samples = parse_speed_stream(lines.join("\n").as_bytes());
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(samples[0].ttft_ms, 500.0);
        assert_eq!(samples[0].duration_ms, 5000.0);
        assert_eq!(samples[0].tps, Some(100.0));
    }

    #[test]
    fn usage_only_old_stream_does_not_fabricate_ttft() {
        let lines = [
            r#"{"type":"request/header","time":1000,"data":{"header":{"config":{"model":"old"}}}}"#,
            r#"{"type":"assistant/chunk","time":2500,"data":{"chunk":{"type":"usage","usage":{"outputTokens":12}}}}"#,
        ];
        assert!(parse_speed_stream(lines.join("\n").as_bytes()).is_empty());
    }

    #[test]
    fn legacy_stream_uses_first_non_usage_chunk_as_first_content() {
        let lines = [
            r#"{"type":"request/header","time":1000,"data":{"header":{"config":{"model":"old"}}}}"#,
            r#"{"type":"assistant/chunk","time":1300,"data":{"chunk":{"type":"text","text":"private"}}}"#,
            r#"{"type":"assistant/chunk","time":2300,"data":{"chunk":{"type":"usage","usage":{"outputTokens":100}}}}"#,
        ];
        let samples = parse_speed_stream(lines.join("\n").as_bytes());
        assert_eq!(samples[0].ttft_ms, 300.0);
        assert_eq!(samples[0].duration_ms, 1300.0);
        assert_eq!(samples[0].tps, Some(100.0));
    }
}

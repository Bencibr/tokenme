//! One `messages.jsonl` line → the call it bills.
//!
//! MiniMax Code is built on the vendored `pi-mono` coding agent, and its session
//! transcript is the pi transcript with a different envelope. The billable line,
//! verbatim in shape from this machine's
//! `v2/sessions/2026/10/05/00-07-16-076-session_…/messages.jsonl`:
//!
//! ```text
//! { "message_id": "msg-LVy4YdC_…", "turn_id": "b2133f4e-…",
//!   "message": { "role": "assistant", "api": "anthropic-messages", "provider": "minimax",
//!     "model": "MiniMax-M3.1-Flash-Preview", "stopReason": "toolCall",
//!     "timestamp": 1791158837144, "responseId": "07121d36…",
//!     "usage": { "input": 21029, "output": 256, "cacheRead": 1924, "cacheWrite": 0,
//!                "totalTokens": 23209, "cost": { … } } } }
//! ```
//!
//! ## Only assistant records are billed
//!
//! All 40 records in that file split 2 `user` / 14 `assistant` / 23 `toolResult` /
//! 1 `custom`, and a `usage` object appears on **14 of 14 assistant records and on
//! none of the other 26** — including nested inside `content`. So `role` is the
//! whole filter, exactly as in [`crate::paths`]'s vendor precedent, and a
//! sub-agent's calls are billed once in the sub-agent's own session file rather
//! than twice here.
//!
//! ## The stages are mutually exclusive, and `input` is the uncached remainder
//!
//! `totalTokens == input + output + cacheRead + cacheWrite` holds for every record
//! in the local store, which is only possible if `cacheRead`/`cacheWrite` are not
//! inside `input`. The vendor says the same thing about the upstream wire
//! (`packages/local-runtime/src/usage/api.ts`): `usage.input` is "fresh input AFTER
//! the last cache breakpoint — already disjoint from `cache_read_input_tokens` and
//! `cache_creation_input_tokens`". So nothing is subtracted here and nothing is
//! added on top; this is [`usage_core::TokenCounts::total`] as written.
//!
//! `reasoning` is read when present and is a *breakdown of* `output` (pi's own
//! rule, measured there: it exceeded `output` in 0 of 12 687 records), never extra
//! work. The records this machine holds carry no such field at all, so thinking
//! spend stays inside `output` rather than being invented.
//!
//! The self-reported `cost` object is deliberately dropped: money is computed
//! centrally from the shared price table so every tool remains comparable, and here
//! it is all zeros anyway (the plan bills nothing per call).

use serde_json::Value;
use usage_core::TokenCounts;

/// One billable record, reduced to what the event needs.
#[derive(Debug, PartialEq)]
pub(crate) struct Parsed {
    pub model: Option<String>,
    pub counts: TokenCounts,
    /// Epoch milliseconds; `None` when the record carries no stamp at all, which
    /// the loader resolves from the file rather than inventing 1970.
    pub time_ms: Option<i64>,
    /// The vendor's per-message id — the identity a replay is absorbed by.
    pub message_id: Option<String>,
    /// Groups one user turn's calls; the fallback identity when a record has no id.
    pub turn_id: Option<String>,
}

/// `None` for a line that is not a billable call: another role, a malformed
/// payload, or a record with nothing to bill (an aborted request).
pub(crate) fn parse_line(raw: &str) -> Option<Parsed> {
    let line: Value = serde_json::from_str(raw).ok()?;
    let message = line.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let usage = message.get("usage")?;
    let counts = TokenCounts {
        input: number(usage, "input"),
        cache_creation: number(usage, "cacheWrite"),
        cache_read: number(usage, "cacheRead"),
        output: number(usage, "output"),
        reasoning: number(usage, "reasoning"),
        credits: 0.0,
    };
    if counts.is_zero() {
        return None;
    }
    Some(Parsed {
        model: text(message, "model"),
        counts,
        time_ms: stamp(message),
        message_id: text(&line, "message_id"),
        turn_id: text(&line, "turn_id"),
    })
}

/// Pi's two timestamp dialects: this product writes epoch milliseconds, and a
/// record filed in 1970 would silently leave every window it belongs to, so a
/// seconds-magnitude value is scaled rather than trusted.
fn stamp(message: &Value) -> Option<i64> {
    let raw = message.get("timestamp")?;
    if let Some(text) = raw.as_str() {
        return usage_core::parse_ts_ms(text);
    }
    let n = raw.as_f64()?.floor();
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let n = n as i64;
    Some(if n < 100_000_000_000 { n * 1000 } else { n })
}

fn text(value: &Value, key: &str) -> Option<String> {
    let text = value.get(key).and_then(Value::as_str)?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// A stage as the source writes it: a JSON number, or a numeric string from a
/// proxy that stringified it. Anything else is zero, never a dropped record.
fn number(value: &Value, key: &str) -> f64 {
    value
        .get(key)
        .and_then(|raw| {
            raw.as_f64()
                .or_else(|| raw.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        })
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim in shape from the local store, with the reasoning split the local
    /// records never exercised filled in.
    const RECORD: &str = r#"{"message_id":"msg-LVy4YdC_xJtkW","turn_id":"b2133f4e-912f",
        "message":{"role":"assistant","api":"anthropic-messages","provider":"minimax",
        "model":"MiniMax-M3.1-Flash-Preview","stopReason":"toolCall",
        "timestamp":1791158837144,"responseId":"07121d366916349e",
        "usage":{"input":21029,"output":256,"cacheRead":1924,"cacheWrite":12,
        "totalTokens":23221,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}}"#;

    #[test]
    fn an_assistant_record_maps_onto_the_four_stages() {
        let p = parse_line(RECORD).expect("billable");
        assert_eq!(p.model.as_deref(), Some("MiniMax-M3.1-Flash-Preview"));
        assert_eq!(p.message_id.as_deref(), Some("msg-LVy4YdC_xJtkW"));
        assert_eq!(p.turn_id.as_deref(), Some("b2133f4e-912f"));
        assert_eq!(p.time_ms, Some(1_791_158_837_144));
        assert_eq!(
            (p.counts.input, p.counts.cache_read, p.counts.cache_creation, p.counts.output),
            (21029.0, 1924.0, 12.0, 256.0)
        );
        // The decomposition is faithful: our total is the vendor's own totalTokens.
        assert_eq!(p.counts.total(), 23221.0);
        assert_eq!(p.counts.credits, 0.0, "self-reported cost must not leak");
    }

    #[test]
    fn only_assistant_records_are_billed() {
        for line in [
            r#"{"message_id":"m1","message":{"role":"user","content":[]}}"#,
            r#"{"message_id":"m2","message":{"role":"toolResult","toolCallId":"t","content":[],"usage":{"input":5,"output":5}}}"#,
            r#"{"message_id":"m3","message":{"role":"custom","customType":"goal"}}"#,
            r#"{"message_id":"m4","message":{"role":"assistant","content":[]}}"#,
            r#"{"message":"not an object"}"#,
            r#"{"message_id":"m5","message":{"role":"assistant","usage":{"input":0,"output":0}}}"#,
            "not json",
        ] {
            assert!(parse_line(line).is_none(), "not a billable call: {line}");
        }
    }

    #[test]
    fn reasoning_is_reported_but_never_added_to_output() {
        let line = r#"{"message_id":"m","message":{"role":"assistant","model":"MiniMax-M2.7",
            "timestamp":1791158837144,"usage":{"input":7616,"output":262,"cacheRead":1152,
            "cacheWrite":0,"reasoning":153,"totalTokens":9030}}}"#;
        let p = parse_line(line).unwrap();
        assert_eq!(p.counts.reasoning, 153.0);
        assert_eq!(p.counts.total(), 9030.0, "thinking is inside output, not beside it");
    }

    #[test]
    fn a_seconds_stamp_is_scaled_and_a_string_number_still_prices_the_call() {
        let line = r#"{"message_id":"m","message":{"role":"assistant","timestamp":1791158837,
            "usage":{"input":"7","output":"3","cacheRead":0,"cacheWrite":0}}}"#;
        let p = parse_line(line).unwrap();
        assert_eq!(p.time_ms, Some(1_791_158_837_000));
        assert_eq!(p.counts.total(), 10.0);
    }

    #[test]
    fn an_absent_or_zero_timestamp_is_none_not_1970() {
        let line = r#"{"message_id":"m","message":{"role":"assistant",
            "usage":{"input":1,"output":1}}}"#;
        assert_eq!(parse_line(line).unwrap().time_ms, None);
        let line = r#"{"message_id":"m","message":{"role":"assistant","timestamp":0,
            "usage":{"input":1,"output":1}}}"#;
        assert_eq!(parse_line(line).unwrap().time_ms, None);
    }
}

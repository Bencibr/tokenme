//! One `wire.jsonl` line → the call it bills.
//!
//! The record contract is the vendor's own
//! (`packages/agent-core-v2/src/wire/record.ts` plus `agent/usage/usageOps.ts`):
//! every durable line is `{ type, time?, …payload }`, and the billable one is
//!
//! ```text
//! { "type": "usage.record", "agentId": "main", "model": "…",
//!   "usage": { "inputOther": 9, "output": 17, "inputCacheRead": 0,
//!              "inputCacheCreation": 0 }, "usageScope": "turn", "time": 1787799862846 }
//! ```
//!
//! ## Both `usageScope` values are billed, and that is the whole argument
//!
//! `usageScope` names the *request source*, not a running total:
//! `session/usage/usageAgentModel.ts` sets it as
//! `input.source?.type === 'turn' ? 'turn' : 'session'`, and one record is
//! emitted per LLM request at `llmRequesterService.ts:477`
//! (`this.usage.record(ctx, request.modelAlias, usage ?? emptyUsage(), request.source)`).
//! Compaction, plan and title requests are therefore `session`-scoped **calls the
//! vendor served and charged for**, and the same file adds every one of them into
//! the state `/status` prints as `Session total`. Reading only `turn` rows — which
//! is what the best-known third-party reader does, on the belief that session rows
//! are cumulative — drops that spend. Following the vendor's own sum is what keeps
//! this adapter's total equal to the number Kimi Code shows.
//!
//! ## The stages are mutually exclusive
//!
//! `inputTotal(usage) = inputOther + inputCacheRead + inputCacheCreation`, and
//! `grandTotal` adds `output` — the same shape [`usage_core::TokenCounts::total`]
//! requires, so nothing is subtracted from `input` and nothing is added on top.
//! The wire carries no reasoning split at all, so `reasoning` stays 0 rather than
//! being invented out of another field.

use serde_json::Value;
use usage_core::TokenCounts;

/// The v2 billable line type.
const RECORD_TYPE: &str = "usage.record";
/// The v2 header line, which carries the protocol version and no usage.
const METADATA_TYPE: &str = "metadata";
/// The legacy line whose payload carries the counts.
const LEGACY_STATUS: &str = "StatusUpdate";
/// A routing prefix the CLI writes into `model` for its own hosted plan.
const MODEL_PREFIX: &str = "kimi-code/";

/// One billable line, reduced to what the event needs.
#[derive(Debug, PartialEq)]
pub(crate) struct Parsed {
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub counts: TokenCounts,
    /// Epoch milliseconds; `None` when the line carries no stamp at all, which
    /// the loader resolves from the file rather than inventing 1970.
    pub time_ms: Option<i64>,
    /// Only the legacy shape has one, and it is a per-message id.
    pub message_id: Option<String>,
}

/// `None` for a line that is not a billable call: another event type, a
/// malformed payload, or a record with nothing to bill.
pub(crate) fn parse_line(raw: &str) -> Option<Parsed> {
    let line: Value = serde_json::from_str(raw).ok()?;
    match line.get("type").and_then(Value::as_str) {
        Some(RECORD_TYPE) => v2_record(&line),
        Some(METADATA_TYPE) => None,
        _ => legacy_record(&line),
    }
}

fn v2_record(line: &Value) -> Option<Parsed> {
    let usage = line.get("usage")?;
    let counts = TokenCounts {
        input: number(usage, "inputOther"),
        cache_creation: number(usage, "inputCacheCreation"),
        cache_read: number(usage, "inputCacheRead"),
        output: number(usage, "output"),
        reasoning: 0.0,
        credits: 0.0,
    };
    if counts.is_zero() {
        return None;
    }
    Some(Parsed {
        agent_id: text(line, "agentId"),
        model: text(line, "model").map(strip_routing_prefix),
        counts,
        time_ms: number_opt(line, "time").map(|ms| normalise_ms(ms as i64)),
        message_id: None,
    })
}

fn legacy_record(line: &Value) -> Option<Parsed> {
    let message = line.get("message")?;
    if message.get("type").and_then(Value::as_str) != Some(LEGACY_STATUS) {
        return None;
    }
    let payload = message.get("payload")?;
    let usage = payload.get("token_usage")?;
    let counts = TokenCounts {
        input: number(usage, "input_other"),
        cache_creation: number(usage, "input_cache_creation"),
        cache_read: number(usage, "input_cache_read"),
        output: number(usage, "output"),
        reasoning: 0.0,
        credits: 0.0,
    };
    if counts.is_zero() {
        return None;
    }
    Some(Parsed {
        agent_id: text(payload, "agent_id"),
        model: text(payload, "model").map(strip_routing_prefix),
        counts,
        // The legacy stamp is epoch *seconds*, and fractional.
        time_ms: number_opt(line, "timestamp").map(|s| normalise_ms(s as i64)),
        message_id: text(payload, "message_id"),
    })
}

fn strip_routing_prefix(model: String) -> String {
    model.strip_prefix(MODEL_PREFIX).unwrap_or(&model).to_string()
}

/// Seconds or milliseconds, decided by magnitude: a record filed in 1970 is a
/// unit bug, and it would otherwise silently leave every window it belongs to.
fn normalise_ms(value: i64) -> i64 {
    if value.abs() < 100_000_000_000 {
        value * 1000
    } else {
        value
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    let text = value.get(key).and_then(Value::as_str)?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// A stage as the source writes it: a JSON number, or a numeric string from a
/// proxy that stringified it. Anything else is zero, never a dropped record.
fn number(value: &Value, key: &str) -> f64 {
    number_opt(value, key).unwrap_or(0.0)
}

fn number_opt(value: &Value, key: &str) -> Option<f64> {
    let raw = value.get(key)?;
    raw.as_f64()
        .or_else(|| raw.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|n| n.is_finite() && *n > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim in shape from `packages/agent-core-v2/test/app/config/config.test.ts`,
    /// which prints the wire the vendor itself writes.
    const TURN: &str = r#"{"type":"usage.record","agentId":"main","model":"mock-model","usage":{"inputOther":9,"output":17,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1787799862846}"#;

    #[test]
    fn a_turn_record_maps_onto_the_four_stages() {
        let p = parse_line(TURN).expect("billable");
        assert_eq!(p.agent_id.as_deref(), Some("main"));
        assert_eq!(p.model.as_deref(), Some("mock-model"));
        assert_eq!(p.time_ms, Some(1_787_799_862_846));
        assert_eq!(
            (p.counts.input, p.counts.cache_read, p.counts.cache_creation, p.counts.output),
            (9.0, 0.0, 0.0, 17.0)
        );
        assert_eq!(p.counts.total(), 26.0);
        assert_eq!(p.counts.reasoning, 0.0, "the wire carries no reasoning split");
    }

    /// The `session` scope is a background *request* — compaction, plan, title —
    /// not a cumulative total, so it is billed exactly like a turn row. Dropping
    /// it is how a reader ends up below what Kimi Code itself shows.
    #[test]
    fn a_session_scoped_record_is_a_real_call_not_a_running_total() {
        let line = r#"{"type":"usage.record","agentId":"main","model":"m","usage":{"inputOther":1200,"output":80,"inputCacheRead":40000,"inputCacheCreation":900},"usageScope":"session","time":1787799869000}"#;
        let p = parse_line(line).expect("session scope is billed");
        assert_eq!(p.counts.total(), 1200.0 + 900.0 + 40000.0 + 80.0);
        assert_eq!(
            p.counts.input + p.counts.cache_read + p.counts.cache_creation,
            42100.0,
            "the vendor's own inputTotal(): prompt stages are mutually exclusive, none inside another"
        );
    }

    #[test]
    fn a_missing_scope_is_still_a_call() {
        // `usageScope` is `.optional()` in the vendor schema; compaction-era rows
        // written before it existed must not become invisible.
        let line = r#"{"type":"usage.record","agentId":"main","model":"m","usage":{"inputOther":10,"output":5},"time":1787799869000}"#;
        assert_eq!(parse_line(line).unwrap().counts.total(), 15.0);
    }

    #[test]
    fn the_routing_prefix_leaves_the_model_the_pricer_can_look_up() {
        let line = r#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":1,"output":1},"time":1}"#;
        assert_eq!(parse_line(line).unwrap().model.as_deref(), Some("kimi-for-coding"));
    }

    #[test]
    fn nothing_to_bill_and_nothing_guessed() {
        // Zero stages: a call that carried no usage payload.
        assert!(parse_line(r#"{"type":"usage.record","model":"m","usage":{"inputOther":0,"output":0},"time":1}"#).is_none());
        // An absent usage object entirely.
        assert!(parse_line(r#"{"type":"usage.record","model":"m","time":1}"#).is_none());
        // The header row.
        assert!(parse_line(r#"{"type":"metadata","protocol_version":"2","created_at":1787799862846}"#).is_none());
        // Torn JSON is skipped, not guessed at.
        assert!(parse_line("{oops").is_none());
        // A model id that is not there stays absent: pricing it as
        // `kimi-for-coding` would bill a model the source never named.
        let line = r#"{"type":"usage.record","usage":{"inputOther":5,"output":5},"time":1}"#;
        assert_eq!(parse_line(line).unwrap().model, None);
    }

    #[test]
    fn a_stringified_number_does_not_lose_the_row() {
        let line = r#"{"type":"usage.record","model":"m","usage":{"inputOther":"1200","output":17},"time":1787799862846}"#;
        assert_eq!(parse_line(line).unwrap().counts.total(), 1217.0);
    }

    #[test]
    fn legacy_status_update_rows_are_read_at_their_seconds_stamp() {
        let line = r#"{"type":"anything","timestamp":1787799862.846,"message":{"type":"StatusUpdate","payload":{"message_id":"msg_1","model":"kimi-k2","token_usage":{"input_other":100,"output":20,"input_cache_read":300,"input_cache_creation":5,"total":425}}}}"#;
        let p = parse_line(line).expect("legacy row");
        assert_eq!(p.message_id.as_deref(), Some("msg_1"));
        assert_eq!(p.model.as_deref(), Some("kimi-k2"));
        assert_eq!(p.time_ms, Some(1_787_799_862_000), "seconds become milliseconds");
        assert_eq!(p.counts.total(), 425.0);
    }

    #[test]
    fn a_millisecond_stamp_is_not_multiplied_again() {
        assert_eq!(normalise_ms(1_787_799_862_846), 1_787_799_862_846);
        assert_eq!(normalise_ms(1_787_799_862), 1_787_799_862_000);
    }
}

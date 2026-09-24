//! Decoding of one rollout line into something the loader can attribute.
//!
//! Only three record kinds matter and they are gated by substring before any
//! JSON parsing: ~4.9 GB of `response_item` payload must never be deserialised
//! just to find out it carries no usage.

use serde_json::Value;
use usage_core::{QuotaSample, TokenCounts};

/// Everything the loader needs from one line.
#[derive(Debug, PartialEq)]
pub(crate) enum Record {
    /// New session boundary: carries `session_id`, `cwd` and, on some versions,
    /// a default `model`.
    SessionMeta {
        session: Option<String>,
        cwd: Option<String>,
        model: Option<String>,
    },
    TurnContext {
        model: Option<String>,
        cwd: Option<String>,
    },
    Usage {
        ts_ms: i64,
        counts: TokenCounts,
        reported_total: f64,
        /// `info.total_token_usage.total_tokens` for this record; unchanged from
        /// the previous record means the call was re-reported, not newly billed.
        accumulator: f64,
        quota: Option<QuotaSample>,
    },
}

/// Cheap pre-filter: `None` means the line cannot carry usage or attribution, so
/// the caller skips it without parsing.
pub(crate) fn worth_parsing(raw: &str) -> bool {
    raw.contains("token_count") || raw.contains("turn_context") || raw.contains("session_meta")
}

fn string_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_string)
}

fn num_field(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

/// `TokenCounts` from a Codex usage object.
///
/// Trap #1: `input_tokens` already *contains* `cached_input_tokens` (real record:
/// 15632 input / 3072 cached / 55 output / 37 reasoning = 15687 total), so the
/// cache has to be split *out* of input or every cached token is billed twice.
pub(crate) fn counts_from(usage: &Value) -> TokenCounts {
    let cached = num_field(usage, "cached_input_tokens").max(0.0);
    TokenCounts {
        input: (num_field(usage, "input_tokens") - cached).max(0.0),
        cache_read: cached,
        cache_creation: num_field(usage, "cache_write_input_tokens").max(0.0),
        output: num_field(usage, "output_tokens").max(0.0),
        // Informational sub-split of `output`: `TokenCounts::total()` never adds
        // it on top, which is what keeps our total equal to `total_tokens`.
        reasoning: num_field(usage, "reasoning_output_tokens").max(0.0),
        credits: 0.0,
    }
}

/// `rate_limits.primary` -> quota sample.
///
/// Trap #3: `resets_at` is unix *seconds*, and every timestamp in `usage-core`
/// is milliseconds; feeding it through unchanged puts the reset 50 years out.
fn quota_from(rate_limits: &Value) -> Option<QuotaSample> {
    let primary = rate_limits.get("primary")?.as_object()?;
    let used_percent = primary.get("used_percent").and_then(Value::as_f64)?;
    let window_minutes = primary.get("window_minutes").and_then(Value::as_i64).unwrap_or(0);
    let resets_at = primary.get("resets_at").and_then(Value::as_i64).unwrap_or(0);
    Some(QuotaSample {
        used_percent,
        window_minutes,
        resets_at_ms: resets_at * 1000,
        label: rate_limits
            .get("limit_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        id: None,
    })
}

/// One `token_count` payload.
///
/// Trap #2: `info.total_token_usage` is a running session accumulator — summing
/// it across records inflates history by orders of magnitude. `last_token_usage`
/// is the per-call delta and is the only field this adapter reads.
pub(crate) fn parse_record(raw: &str) -> Option<Record> {
    let line: Value = serde_json::from_str(raw).ok()?;
    let kind = line.get("type").and_then(Value::as_str)?;
    let payload = line.get("payload")?;
    match kind {
        "session_meta" => Some(Record::SessionMeta {
            session: string_field(payload, "session_id").or_else(|| string_field(payload, "id")),
            cwd: string_field(payload, "cwd"),
            model: string_field(payload, "model"),
        }),
        "turn_context" => Some(Record::TurnContext {
            model: string_field(payload, "model"),
            cwd: string_field(payload, "cwd"),
        }),
        "event_msg" if payload.get("type").and_then(Value::as_str) == Some("token_count") => {
            let ts_ms = usage_core::parse_ts_ms(line.get("timestamp").and_then(Value::as_str)?)?;
            let info = payload.get("info")?;
            let last = info.get("last_token_usage")?;
            let counts = counts_from(last);
            let quota = payload.get("rate_limits").and_then(quota_from);
            Some(Record::Usage {
                ts_ms,
                reported_total: num_field(last, "total_tokens"),
                accumulator: info
                    .get("total_token_usage")
                    .map(|t| num_field(t, "total_tokens"))
                    .unwrap_or(0.0),
                counts,
                quota,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verified real record: 15632 input of which 3072 cached, 55 output of
    /// which 37 reasoning, `total_tokens` 15687.
    const SAMPLE: &str = r#"{"timestamp":"2026-09-23T11:32:31.944Z","ordinal":19,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":999999,"cached_input_tokens":9,"cache_write_input_tokens":9,"output_tokens":9,"reasoning_output_tokens":9,"total_tokens":999999},"last_token_usage":{"input_tokens":15632,"cached_input_tokens":3072,"cache_write_input_tokens":0,"output_tokens":55,"reasoning_output_tokens":37,"total_tokens":15687},"model_context_window":258400}}}"#;

    fn usage(line: &str) -> Record {
        parse_record(line).expect("record")
    }

    #[test]
    fn cached_tokens_are_split_out_of_input_not_added() {
        let Record::Usage { counts, reported_total, .. } = usage(SAMPLE) else {
            panic!("expected a usage record")
        };
        assert_eq!(counts.input, 15632.0 - 3072.0);
        assert_eq!(counts.cache_read, 3072.0);
        assert_eq!(counts.output, 55.0);
        assert_eq!(counts.reasoning, 37.0);
        // The whole point: reasoning is a sub-split, so total() lands exactly on
        // the source's own billable total.
        assert_eq!(counts.total(), 15687.0);
        assert_eq!(counts.total(), reported_total);
    }

    #[test]
    fn the_running_accumulator_is_never_read() {
        let Record::Usage { counts, .. } = usage(SAMPLE) else { panic!() };
        assert!(counts.total() < 100_000.0, "total_token_usage leaked into the event");
    }

    #[test]
    fn quota_comes_from_the_same_payload_in_seconds() {
        let line = r#"{"timestamp":"2026-09-23T11:32:31.944Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":1,"total_tokens":11}},"rate_limits":{"limit_id":"codex","primary":{"used_percent":45.0,"window_minutes":300,"resets_at":1790170235},"secondary":{"used_percent":10.0,"window_minutes":10080,"resets_at":1790739004},"plan_type":"plus"}}}"#.to_string();
        let Record::Usage { quota, .. } = usage(&line) else { panic!() };
        let q = quota.expect("primary present");
        assert_eq!(q.used_percent, 45.0);
        assert_eq!(q.window_minutes, 300, "the 5h window, not the weekly secondary");
        assert_eq!(q.resets_at_ms, 1_790_170_235_000, "seconds scaled to ms once");
    }

    #[test]
    fn attribution_records_are_decoded_without_a_model_on_meta() {
        let Record::SessionMeta { session, cwd, model } =
            usage(r#"{"timestamp":"t","type":"session_meta","payload":{"session_id":"01a0","cwd":"/w","originator":"codex-tui"}}"#)
        else {
            panic!()
        };
        assert_eq!(session.as_deref(), Some("01a0"));
        assert_eq!(cwd.as_deref(), Some("/w"));
        assert_eq!(model, None, "this Codex version keeps the model on turn_context only");
        let Record::TurnContext { model, cwd } =
            usage(r#"{"type":"turn_context","payload":{"turn_id":"x","cwd":"/w","model":"gpt-5.6-luna"}}"#) else {
            panic!()
        };
        assert_eq!(model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(cwd.as_deref(), Some("/w"));
    }

    #[test]
    fn irrelevant_malformed_and_partial_input_is_rejected_not_panicking() {
        assert!(!worth_parsing(r#"{"type":"response_item","payload":{"type":"function_call"}}"#));
        assert!(parse_record("{ not json").is_none());
        assert!(parse_record(r#"{"type":"session_meta""#).is_none());
        assert!(parse_record(r#"{"type":"event_msg","payload":{"type":"token_count"}}"#).is_none(), "no info at all");
        assert!(parse_record(r#"{"timestamp":"nope","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":5}}}}"#)
            .is_none(),
            "an unusable timestamp cannot be placed in time");
        assert!(worth_parsing(SAMPLE));
    }

    #[test]
    fn absent_stages_default_to_zero_not_negative() {
        let c = counts_from(&serde_json::json!({"input_tokens": 4, "cached_input_tokens": 9}));
        assert_eq!((c.input, c.cache_read, c.output, c.total()), (0.0, 9.0, 0.0, 9.0));
    }
}

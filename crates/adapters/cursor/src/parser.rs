//! One ledger line → one billable Cursor turn.
//!
//! The ledger is ours (`cursor-usage.jsonl`). Each line is already reduced to
//! token counts, model, and time — the dashboard payload can carry account
//! ids and fees, and none of that is written down.

use usage_core::{Meter, TokenCounts, UsageEvent};

use crate::TOOL_ID;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LedgerRow {
    pub ts_ms: i64,
    pub model: String,
    pub counts: TokenCounts,
    pub dedupe: String,
}

pub(crate) fn parse_line(text: &str) -> Option<LedgerRow> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let ts_ms = value.get("ts")?.as_i64().filter(|ts| *ts > 0)?;
    let model = value.get("model")?.as_str().map(str::trim).filter(|m| !m.is_empty())?;
    let counts = TokenCounts {
        input: num(value.get("input")),
        output: num(value.get("output")),
        cache_read: num(value.get("cache_read")),
        cache_creation: num(value.get("cache_write")),
        ..TokenCounts::default()
    };
    if counts.is_zero() {
        return None;
    }
    let dedupe = value
        .get("dedupe")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| dedupe_key(ts_ms, model, &counts));
    Some(LedgerRow { ts_ms, model: model.to_string(), counts, dedupe })
}

pub(crate) fn event_from(row: LedgerRow, source: &str) -> UsageEvent {
    let mut event = UsageEvent::new(TOOL_ID, row.ts_ms, session_of(row.ts_ms));
    event.model = Some(row.model);
    event.counts = row.counts;
    event.meter = Meter::Tokens;
    event.dedupe_key = Some(row.dedupe);
    event.source = source.to_string();
    event
}

pub(crate) fn dedupe_key(ts_ms: i64, model: &str, counts: &TokenCounts) -> String {
    format!(
        "cursor#{ts_ms}#{model}#{:.0}#{:.0}#{:.0}#{:.0}",
        counts.input, counts.output, counts.cache_read, counts.cache_creation
    )
}

/// UTC day, so two machines hashing the same account event name the same session.
fn session_of(ts_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts_ms)
        .map(|dt| format!("cursor-{}", dt.format("%Y-%m-%d")))
        .unwrap_or_else(|| "cursor".into())
}

fn num(value: Option<&serde_json::Value>) -> f64 {
    let Some(value) = value else { return 0.0 };
    if let Some(n) = value.as_f64() {
        return if n.is_finite() && n > 0.0 { n } else { 0.0 };
    }
    value.as_str().and_then(|s| s.trim().parse::<f64>().ok()).filter(|n| n.is_finite() && *n > 0.0).unwrap_or(0.0)
}

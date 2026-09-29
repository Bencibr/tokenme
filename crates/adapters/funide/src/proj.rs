//! One cumulative event per FunIDE session, from the session file's own
//! ledger.
//!
//! `cacheUsage` is the session's running total the UI itself displays:
//! `{promptTokens, cachedTokens, completionTokens, chargedPoints, reported}` —
//! measured against a live session, `cachedTokens` is a subset of
//! `promptTokens` (the UI prints 缓存 as a percent *of* 输入), so it maps to
//! `cache_read` and never doubles the input. Per-turn `items[].usage*` numbers
//! sum to the same totals but carry no timestamps, so per-turn events would
//! day-bucket a multi-day session onto its last edit — one cumulative event
//! replaced on a stable key (`funide#<session>`) ages the way the hermes and
//! dsh-projection slices do, and the file's mtime is the pass cursor.
//!
//! `activeModelId` (`cloud:GLM-plan-flash`) is the session's model as the UI
//! shows it; the `cloud:` prefix is the FunIDE-hosted family, stripped for the
//! display id. `chargedPoints` is FunIDE's points pricing, drawn from the
//! cloud account — and that balance IS reachable (cloud API `billing/balance`
//! with the IDE's own stored token), which the `usage-quota` FunIdeQuota
//! probe reads; this adapter stays usage-only.

use serde_json::Value;
use usage_core::{Meter, UsageEvent};

/// Parse one session file into the cumulative event it represents. `None` for
/// sessions with no billable usage yet (opened, never answered).
pub fn parse(text: &str, mtime_ms: i64, source_key: &str) -> Option<Box<UsageEvent>> {
    let v: Value = serde_json::from_str(text).ok()?;
    let session = v.get("id")?.as_str()?.to_string();
    let usage = v.get("cacheUsage")?;
    let num = |k: &str| usage.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
    let prompt = num("promptTokens");
    let cached = num("cachedTokens").min(prompt);
    let output = num("completionTokens");
    if prompt + output <= 0.0 {
        return None;
    }
    let model = v
        .get("activeModelId")
        .and_then(Value::as_str)
        .map(|m| m.strip_prefix("cloud:").unwrap_or(m).to_string());
    let ts_ms = v
        .get("updatedAt")
        .and_then(Value::as_i64)
        .filter(|ts| *ts > 0)
        .unwrap_or(mtime_ms.max(0));
    let mut event = UsageEvent::new(crate::TOOL_ID, ts_ms, &session);
    event.counts.input = prompt - cached;
    event.counts.cache_read = cached;
    event.counts.output = output;
    event.meter = Meter::Tokens;
    event.model = model.filter(|m| !m.is_empty());
    event.project = v
        .get("workspaceKey")
        .and_then(Value::as_str)
        .map(|w| {
            w.trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(w)
                .to_string()
        });
    event.dedupe_key = Some(format!("{}{}", crate::DEDUPE_PREFIX, session));
    event.source = source_key.to_string();
    Some(Box::new(event))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r#"{
      "version": 2,
      "id": "smumnmwb73amg",
      "title": "扫描这个项目",
      "workspaceKey": "d:\\workspace\\figma-mcp-server",
      "activeModelId": "cloud:GLM-plan-flash",
      "updatedAt": 1790685147111,
      "cacheUsage": {"totalTokens": 88870, "promptTokens": 87164,
                     "cachedTokens": 23296, "completionTokens": 1706,
                     "chargedPoints": 0.006123, "reported": true}
    }"#;

    #[test]
    fn the_session_becomes_one_cumulative_event_with_exclusive_cache() {
        let e = parse(LIVE, 1790685000000, "funide/s1.json").expect("a used session yields an event");
        assert_eq!(e.session, "smumnmwb73amg");
        assert_eq!(e.model.as_deref(), Some("GLM-plan-flash"), "the cloud: prefix is display noise");
        assert_eq!(e.project.as_deref(), Some("figma-mcp-server"));
        // cached is a subset of prompt: input lands exclusive, cache as read.
        assert_eq!(e.counts.input, 87164.0 - 23296.0);
        assert_eq!(e.counts.cache_read, 23296.0);
        assert_eq!(e.counts.output, 1706.0);
        assert_eq!(e.counts.total(), 88870.0, "matches the session's own totalTokens");
        assert_eq!(e.ts_ms, 1790685147111);
        assert_eq!(e.dedupe_key.as_deref(), Some("funide#smumnmwb73amg"));
    }

    #[test]
    fn an_unused_session_yields_nothing() {
        let empty = r#"{"id":"s2","activeModelId":"cloud:GLM-plan-flash","updatedAt":5,
            "cacheUsage":{"promptTokens":0,"cachedTokens":0,"completionTokens":0}}"#;
        assert!(parse(empty, 9, "k").is_none(), "no billable usage, no event");
    }
}

//! One event per FunIDE turn, from the session file's own per-turn ledger.
//!
//! `cacheUsage` is the session's LIFETIME running total the UI itself displays,
//! and `items[]` carries the same money broken out per turn: `usagePrompt`,
//! `usageCached`, `usageCompletion`. Measured against a live session,
//! `usageCached` is a subset of `usagePrompt` (the UI prints 缓存 as a percent
//! *of* 输入), so it maps to `cache_read` and never doubles the input, and the
//! per-turn sums equal `cacheUsage` exactly. Each assistant item's `turnId` is
//! `turn_<the turn's start, in ms>_<seq>`, which makes the turns day-true.
//!
//! Stamping one cumulative event with the session's `updatedAt` is what this
//! file did until 2026-10-21, and it cannot age correctly: a session continued
//! across a day boundary drags its whole lifetime total into the newest day.
//! Measured here (session `smvf8pvrm7qrg`): 477,578,323 prompt tokens all
//! landed on 10-21 while 10-19 and 10-20 — the days its 13 turns actually ran —
//! went to zero. The same shape the dsh projection was retired for.
//!
//! `activeModelId` (`cloud:GLM-plan-flash`) is the session's model as the UI
//! shows it; the `cloud:` prefix is the FunIDE-hosted family, stripped for the
//! display id, and the turns of a session share it. `chargedPoints` is FunIDE's
//! points pricing, drawn from the cloud account — that balance IS reachable
//! (cloud API `billing/balance` with the IDE's own stored token), which the
//! `usage-quota` FunIdeQuota probe reads; this adapter stays usage-only.

use serde_json::Value;
use usage_core::{Meter, UsageEvent};

/// Parse one session file into the per-turn events its items record. Empty for
/// sessions with no billable usage yet (opened, never answered).
pub fn parse(text: &str, mtime_ms: i64, source_key: &str) -> Vec<UsageEvent> {
    let mut events = Vec::new();
    let Ok(v) = serde_json::from_str::<Value>(text) else { return events };
    let Some(session) = v.get("id").and_then(Value::as_str) else { return events };
    let model = v
        .get("activeModelId")
        .and_then(Value::as_str)
        .map(|m| m.strip_prefix("cloud:").unwrap_or(m).to_string())
        .filter(|m| !m.is_empty());
    let project = v
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
    // A turn with no `turnId` still has to be billed somewhere, and the
    // session's own edit time is the only timestamp the file offers for it.
    let fallback_ts = v
        .get("updatedAt")
        .and_then(Value::as_i64)
        .filter(|ts| *ts > 0)
        .unwrap_or(mtime_ms.max(0));
    let items: &[Value] = v
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (idx, item) in items.iter().enumerate() {
        if item.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let num = |k: &str| item.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        let prompt = num("usagePrompt");
        let cached = num("usageCached").min(prompt);
        let output = num("usageCompletion");
        if prompt + output <= 0.0 {
            continue;
        }
        let turn = item.get("turnId").and_then(Value::as_str).filter(|t| !t.is_empty());
        let ts_ms = turn.and_then(turn_start_ms).unwrap_or(fallback_ts);
        let mut event = UsageEvent::new(crate::TOOL_ID, ts_ms, session);
        event.counts.input = prompt - cached;
        event.counts.cache_read = cached;
        event.counts.output = output;
        event.meter = Meter::Tokens;
        event.model = model.clone();
        event.project = project.clone();
        // The indexer's dedupe index is global and the file is re-read whole on
        // every pass, so this key is the only thing keeping the replay
        // idempotent. A turn with no id falls back to its position — item order
        // is stable across rewrites.
        event.dedupe_key = Some(match turn {
            Some(t) => format!("{}{}#{}", crate::DEDUPE_PREFIX, session, t),
            None => format!("{}{}#idx{}", crate::DEDUPE_PREFIX, session, idx),
        });
        event.source = source_key.to_string();
        events.push(event);
    }
    events
}

/// `turn_1790510647992_1111` — the middle field is the turn's start in ms.
fn turn_start_ms(turn_id: &str) -> Option<i64> {
    turn_id
        .split('_')
        .nth(1)?
        .parse::<i64>()
        .ok()
        // A seconds stamp would day-bucket 1970. The writer only ever emits ms,
        // so anything under this is not the field we think it is.
        .filter(|ms| *ms > 100_000_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two billed turns of a live session, 36 hours apart — the case the
    /// lifetime projection collapsed onto one day.
    const LIVE: &str = r#"{
      "version": 2,
      "id": "smumnmwb73amg",
      "title": "扫描这个项目",
      "workspaceKey": "C:\\Users\\demo\\figma-mcp-server",
      "activeModelId": "cloud:GLM-plan-flash",
      "updatedAt": 1790685147111,
      "cacheUsage": {"totalTokens": 2267706, "promptTokens": 2187464,
                     "cachedTokens": 1864256, "completionTokens": 80242,
                     "chargedPoints": 0.006123, "reported": true},
      "items": [
        {"role": "user", "turnId": "turn_1790510647992_1111", "text": "扫一下"},
        {"role": "assistant", "turnId": "turn_1790510647992_1111",
         "usagePrompt": 68403, "usageCached": 23296, "usageCompletion": 1415,
         "usage": "输入 68403 · 输出 1415 · 缓存 23296（34.1%）"},
        {"role": "user", "turnId": "turn_1790640247992_2222", "text": "再来"},
        {"role": "assistant", "turnId": "turn_1790640247992_2222",
         "usagePrompt": 2119061, "usageCached": 1840960, "usageCompletion": 78827}
      ]
    }"#;

    #[test]
    fn each_turn_is_its_own_event_stamped_with_that_turns_start() {
        let events = parse(LIVE, 1790685000000, "funide/s1.json");
        assert_eq!(events.len(), 2, "one event per billed assistant turn");
        let [a, b] = events.as_slice() else { panic!("two turns expected") };

        assert_eq!(a.session, "smumnmwb73amg");
        assert_eq!(a.model.as_deref(), Some("GLM-plan-flash"), "the cloud: prefix is display noise");
        assert_eq!(a.project.as_deref(), Some("figma-mcp-server"));
        // cached is a subset of prompt: input lands exclusive, cache as read.
        assert_eq!(a.counts.input, 68403.0 - 23296.0);
        assert_eq!(a.counts.cache_read, 23296.0);
        assert_eq!(a.counts.output, 1415.0);
        assert_eq!(a.ts_ms, 1790510647992, "the turn's own start, not the session's edit time");
        assert_eq!(
            a.dedupe_key.as_deref(),
            Some("funide#smumnmwb73amg#turn_1790510647992_1111")
        );

        assert_eq!(b.ts_ms, 1790640247992, "the later turn keeps its later day");
        assert_ne!(
            usage_core::local_day_of(a.ts_ms),
            usage_core::local_day_of(b.ts_ms),
            "the fixture spans a local day boundary — the case the projection lost"
        );
    }

    #[test]
    fn the_turn_events_sum_to_the_sessions_own_lifetime_total() {
        let events = parse(LIVE, 1790685000000, "funide/s1.json");
        let mut prompt = 0.0;
        let mut output = 0.0;
        for e in &events {
            prompt += e.counts.input + e.counts.cache_read;
            output += e.counts.output;
        }
        assert_eq!(prompt, 2187464.0, "promptTokens, split the way the UI reads it");
        assert_eq!(output, 80242.0, "completionTokens");
        assert_eq!(prompt + output, 2267706.0, "totalTokens");
    }

    #[test]
    fn an_unanswered_turn_and_an_itemless_file_yield_nothing() {
        let text = r#"{"id":"s2","updatedAt":5,
            "items":[{"role":"user","turnId":"turn_1790510647992_1","text":"hi"},
                     {"role":"assistant","turnId":"turn_1790510647992_1",
                      "usagePrompt":0,"usageCached":0,"usageCompletion":0}]}"#;
        assert!(parse(text, 9, "k").is_empty(), "no billable usage, no event");
        assert!(
            parse(r#"{"id":"s3","cacheUsage":{"promptTokens":10}}"#, 9, "k").is_empty(),
            "a file with no items has no turn to bill: the lifetime total is not a day"
        );
    }

    #[test]
    fn a_turn_without_an_id_falls_back_to_its_position_and_the_edit_time() {
        let text = r#"{"id":"s4","activeModelId":"cloud:GLM-plan-flash","updatedAt":1790510647992,
            "items":[{"role":"assistant","usagePrompt":100,"usageCached":40,"usageCompletion":7}]}"#;
        let events = parse(text, 1, "k");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ts_ms, 1790510647992, "no turn id: the session's stamp is all there is");
        assert_eq!(events[0].dedupe_key.as_deref(), Some("funide#s4#idx0"), "a replayed file still dedupes");
    }

    #[test]
    fn a_turn_id_without_a_usable_stamp_is_not_guessed_at() {
        assert_eq!(turn_start_ms("turn_1790510647992_1111"), Some(1790510647992));
        assert_eq!(turn_start_ms("turn_1790510647_1111"), None, "seconds is not ms");
        assert_eq!(turn_start_ms("turn_x_1111"), None);
        assert_eq!(turn_start_ms("turn"), None);
    }
}

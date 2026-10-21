//! One event per FunIDE **turn**, from the session file's own per-turn ledger.
//!
//! The session file carries two ledgers: `cacheUsage`, the whole session's
//! running total that the UI prints, and a per-item `usage*` block on every
//! assistant message (`usagePrompt`, `usageCached`, `usageCompletion`,
//! `usageCacheReported`, plus the preformatted `usage` string). Measured against
//! a live session, `usageCached` is a subset of `usagePrompt` (the UI prints 缓存
//! as a percent *of* 输入), so it maps to `cache_read` and never doubles the
//! input.
//!
//! Reading `cacheUsage` alone was wrong in a way that only shows up across days:
//! it is a LIFETIME total, and the only timestamp on the file is `updatedAt` —
//! the instant the session was last written. A session continued into the next
//! morning dragged its entire history onto that morning: one DeepSeek session
//! carrying 360.20M tokens from 2026-10-20 was last touched at 09:12 the next
//! day and showed up as 360.20M **today**, on a machine that had run 0 calls
//! (reported 2026-10-21, session smvg415dt5wbv). The wake-from-sleep pass is
//! what surfaces it: the file's mtime moves, the projection is re-read, and the
//! whole lifetime re-lands on the new day.
//!
//! The per-turn timestamps were there all along: every `turnId` is
//! `turn_<epoch_ms>_<rand>`. Each turn therefore becomes its own event stamped
//! with the turn's real start, keyed `funide#<session>#<turnId>` — stable across
//! re-reads, so a turn whose usage grows mid-flight is replaced on grow and the
//! day attribution stays true.
//!
//! `cacheUsage` is still read, but only as a **residual**: the turns' sums equal
//! it exactly on 10 of 11 live sessions, and the one that does not (50,466 of
//! 89.4M prompt tokens) is compacted-away history no item accounts for. The
//! remainder is added to the session's newest turn, so the totals stay exactly
//! what the UI reports while nothing older than the session's own last activity
//! gets re-dated.
//!
//! `activeModelId` (`cloud:GLM-plan-flash`) is the session's model as the UI
//! shows it; the `cloud:` prefix is the FunIDE-hosted family, stripped for the
//! display id. `chargedPoints` is FunIDE's points pricing, drawn from the
//! cloud account — and that balance IS reachable (cloud API `billing/balance`
//! with the IDE's own stored token), which the `usage-quota` FunIdeQuota
//! probe reads; this adapter stays usage-only.

use std::collections::HashMap;

use serde_json::Value;
use usage_core::{Meter, UsageEvent};

/// One turn's ledger, before it becomes a [`UsageEvent`].
struct Turn {
    id: String,
    ts_ms: i64,
    prompt: f64,
    cached: f64,
    output: f64,
}

/// Parse one session file into the events its turns represent. Empty for a
/// session with no billable usage yet (opened, never answered).
pub fn parse(text: &str, mtime_ms: i64, source_key: &str) -> Vec<UsageEvent> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let Some(session) = v.get("id").and_then(Value::as_str) else { return Vec::new() };
    let model = v
        .get("activeModelId")
        .and_then(Value::as_str)
        .map(|m| m.strip_prefix("cloud:").unwrap_or(m).to_string())
        .filter(|m| !m.is_empty());
    let project = v.get("workspaceKey").and_then(Value::as_str).map(|w| {
        w.trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(w)
            .to_string()
    });
    let updated_at = v
        .get("updatedAt")
        .and_then(Value::as_i64)
        .filter(|ts| *ts > 0)
        .unwrap_or(mtime_ms.max(0));

    let mut turns = collect_turns(&v, updated_at);
    if let Some(usage) = v.get("cacheUsage") {
        if turns.is_empty() {
            // No per-turn ledger to split (an older writer, or a session whose
            // messages were all dropped): fall back to the lifetime total, which
            // is at least the session's real money, dated to its last write.
            let prompt = field(usage, "promptTokens");
            let output = field(usage, "completionTokens");
            if prompt + output <= 0.0 {
                return Vec::new();
            }
            turns.push(Turn {
                id: String::new(),
                ts_ms: updated_at,
                prompt,
                cached: field(usage, "cachedTokens").min(prompt),
                output,
            });
        } else {
            apply_residual(&mut turns, usage);
        }
    }

    turns
        .into_iter()
        .filter(|t| t.prompt + t.output > 0.0)
        .map(|t| {
            let mut event = UsageEvent::new(crate::TOOL_ID, t.ts_ms, session);
            // cached is a subset of prompt: input lands exclusive, cache as read.
            event.counts.input = t.prompt - t.cached;
            event.counts.cache_read = t.cached;
            event.counts.output = t.output;
            event.meter = Meter::Tokens;
            event.model = model.clone();
            event.project = project.clone();
            event.dedupe_key = Some(if t.id.is_empty() {
                // The lifetime fallback keeps the pre-per-turn key shape, so a
                // session that later grows a turn ledger cannot double-count: its
                // turns arrive under their own keys and this row is replaced by
                // the residual path above only when turns exist.
                format!("{}{}", crate::DEDUPE_PREFIX, session)
            } else {
                format!("{}{}#{}", crate::DEDUPE_PREFIX, session, t.id)
            });
            event.source = source_key.to_string();
            event
        })
        .collect()
}

/// Every assistant item carrying a usage block, deduped by `turnId` (a
/// regenerate can repeat one) and stamped from the id's own epoch millis.
fn collect_turns(v: &Value, fallback_ts: i64) -> Vec<Turn> {
    let Some(items) = v.get("items").and_then(Value::as_array) else { return Vec::new() };
    let mut turns: Vec<Turn> = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for item in items {
        let Some(id) = item.get("turnId").and_then(Value::as_str) else { continue };
        let prompt = field(item, "usagePrompt");
        let output = field(item, "usageCompletion");
        if prompt <= 0.0 && output <= 0.0 {
            continue;
        }
        let cached = field(item, "usageCached").min(prompt);
        match seen.get(id) {
            Some(&i) => {
                // Same turn seen twice: keep the larger reading, never the sum.
                let t = &mut turns[i];
                if prompt + output > t.prompt + t.output {
                    t.prompt = prompt;
                    t.cached = cached;
                    t.output = output;
                }
            }
            None => {
                seen.insert(id, turns.len());
                turns.push(Turn {
                    id: id.to_string(),
                    ts_ms: turn_ts_ms(id).unwrap_or(fallback_ts),
                    prompt,
                    cached,
                    output,
                });
            }
        }
    }
    turns
}

/// Fold whatever `cacheUsage` holds beyond the turns' own sums onto the newest
/// turn. Compaction is the known cause; a ledger that lags behind its items
/// (sum > total) is left alone, because the items are the finer truth.
fn apply_residual(turns: &mut [Turn], usage: &Value) {
    let total_prompt = field(usage, "promptTokens");
    let total_output = field(usage, "completionTokens");
    let total_cached = field(usage, "cachedTokens").min(total_prompt);
    let summed_prompt: f64 = turns.iter().map(|t| t.prompt).sum();
    let summed_output: f64 = turns.iter().map(|t| t.output).sum();
    let summed_cached: f64 = turns.iter().map(|t| t.cached).sum();
    let dp = total_prompt - summed_prompt;
    let do_ = total_output - summed_output;
    let dc = total_cached - summed_cached;
    if dp <= 0.0 && do_ <= 0.0 && dc <= 0.0 {
        return;
    }
    let Some(t) = turns.iter_mut().max_by_key(|t| t.ts_ms) else { return };
    t.prompt += dp.max(0.0);
    t.output += do_.max(0.0);
    t.cached = (t.cached + dc.max(0.0)).min(t.prompt);
}

/// `turn_<epoch_ms>_<rand>` → the turn's start. `None` for an id that does not
/// carry a plausible instant, so the caller can fall back to `updatedAt`.
fn turn_ts_ms(turn_id: &str) -> Option<i64> {
    let rest = turn_id.strip_prefix("turn_")?;
    let digits = rest.split('_').next()?;
    let ms: i64 = digits.parse().ok()?;
    // 2000-01-01, below which a "timestamp" is some other number entirely.
    (ms >= 946_684_800_000).then_some(ms)
}

/// A non-negative number off `obj`, `0.0` when absent or of another type.
fn field(obj: &Value, key: &str) -> f64 {
    obj.get(key).and_then(Value::as_f64).unwrap_or(0.0).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r#"{
      "version": 2,
      "id": "smumnmwb73amg",
      "title": "扫描这个项目",
      "workspaceKey": "C:\\Users\\demo\\figma-mcp-server",
      "activeModelId": "cloud:GLM-plan-flash",
      "updatedAt": 1790685147111,
      "items": [
        {"role": "user", "turnId": "turn_1790640000000_1", "text": "hi"},
        {"role": "assistant", "turnId": "turn_1790640000000_1",
         "usagePrompt": 1000, "usageCached": 400, "usageCompletion": 50,
         "usageCacheReported": true}
      ],
      "cacheUsage": {"totalTokens": 1050, "promptTokens": 1000,
                     "cachedTokens": 400, "completionTokens": 50,
                     "chargedPoints": 0.006123, "reported": true}
    }"#;

    #[test]
    fn a_turn_becomes_an_event_stamped_from_its_own_id() {
        let events = parse(LIVE, 1790685000000, "funide/s1.json");
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.session, "smumnmwb73amg");
        assert_eq!(e.model.as_deref(), Some("GLM-plan-flash"), "the cloud: prefix is display noise");
        assert_eq!(e.project.as_deref(), Some("figma-mcp-server"));
        // cached is a subset of prompt: input lands exclusive, cache as read.
        assert_eq!(e.counts.input, 600.0);
        assert_eq!(e.counts.cache_read, 400.0);
        assert_eq!(e.counts.output, 50.0);
        assert_eq!(e.counts.total(), 1050.0, "matches the session's own totalTokens");
        assert_eq!(e.ts_ms, 1790640000000, "the turn's own instant, not updatedAt");
        assert_eq!(e.dedupe_key.as_deref(), Some("funide#smumnmwb73amg#turn_1790640000000_1"));
    }

    #[test]
    fn a_session_continued_across_days_keeps_each_turn_on_its_own_day() {
        // The reported shape: yesterday's 360.20M last written this morning.
        let text = r#"{
          "id": "smvg415dt5wbv",
          "activeModelId": "cloud:deepseek-flash-discount-1",
          "workspaceKey": "/Users/demo/proj",
          "updatedAt": 1792545176291,
          "items": [
            {"role": "assistant", "turnId": "turn_1792466181912_254",
             "usagePrompt": 300000000, "usageCached": 280000000, "usageCompletion": 300000},
            {"role": "assistant", "turnId": "turn_1792498004144_3629",
             "usagePrompt": 60000000, "usageCached": 50000000, "usageCompletion": 49210}
          ],
          "cacheUsage": {"promptTokens": 360000000, "cachedTokens": 330000000,
                         "completionTokens": 349210}
        }"#;
        let events = parse(text, 1792545176291, "funide/s2.json");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].ts_ms, 1792466181912);
        assert_eq!(events[1].ts_ms, 1792498004144);
        assert_ne!(
            events[0].dedupe_key, events[1].dedupe_key,
            "one session, one key per turn — a shared key would fold them together"
        );
        let total: f64 = events.iter().map(|e| e.counts.total()).sum();
        assert_eq!(total, 360349210.0, "the session's own lifetime total is preserved");
        // Both turns predate the write, so neither may land on the write's day.
        assert!(events.iter().all(|e| e.ts_ms < 1792545176291));
    }

    #[test]
    fn a_compaction_remainder_lands_on_the_newest_turn_only() {
        let text = r#"{
          "id": "s3",
          "activeModelId": "cloud:GLM-plan-flash",
          "updatedAt": 1792545176291,
          "items": [
            {"role": "assistant", "turnId": "turn_1792466181912_1",
             "usagePrompt": 100, "usageCached": 0, "usageCompletion": 10},
            {"role": "assistant", "turnId": "turn_1792498004144_2",
             "usagePrompt": 200, "usageCached": 0, "usageCompletion": 20}
          ],
          "cacheUsage": {"promptTokens": 800, "cachedTokens": 0, "completionTokens": 80}
        }"#;
        let events = parse(text, 0, "k");
        assert_eq!(events.len(), 2, "the remainder joins a turn, it does not become one");
        assert_eq!(events[0].counts.total(), 110.0);
        assert_eq!(events[1].counts.total(), 770.0, "200+500 prompt, 20+50 output");
        assert_eq!(events[1].ts_ms, 1792498004144, "the newest turn carries it");
        let total: f64 = events.iter().map(|e| e.counts.total()).sum();
        assert_eq!(total, 880.0, "800 prompt + 80 output, the session's own ledger");
    }

    #[test]
    fn a_session_without_a_turn_ledger_falls_back_to_the_lifetime_total() {
        let text = r#"{"id":"s4","activeModelId":"cloud:GLM-plan-flash","updatedAt":1790685147111,
            "items":[{"role":"assistant","turnId":"turn_1790685000000_1","text":"no usage block"}],
            "cacheUsage":{"promptTokens":100,"cachedTokens":20,"completionTokens":10}}"#;
        let events = parse(text, 9, "k");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].counts.input, 80.0);
        assert_eq!(events[0].counts.total(), 110.0);
        assert_eq!(events[0].ts_ms, 1790685147111, "dated to the file's last write");
        assert_eq!(events[0].dedupe_key.as_deref(), Some("funide#s4"));
    }

    #[test]
    fn an_unused_session_yields_nothing() {
        let empty = r#"{"id":"s2","activeModelId":"cloud:GLM-plan-flash","updatedAt":5,
            "items":[{"role":"assistant","turnId":"turn_1790685000000_1","usagePrompt":0,
                      "usageCached":0,"usageCompletion":0}],
            "cacheUsage":{"promptTokens":0,"cachedTokens":0,"completionTokens":0}}"#;
        assert!(parse(empty, 9, "k").is_empty(), "no billable usage, no event");
    }

    #[test]
    fn a_repeated_turn_id_keeps_the_larger_reading() {
        let text = r#"{"id":"s5","updatedAt":1792545176291,
            "items":[
              {"role":"assistant","turnId":"turn_1792466181912_1","usagePrompt":100,"usageCached":0,"usageCompletion":10},
              {"role":"assistant","turnId":"turn_1792466181912_1","usagePrompt":150,"usageCached":0,"usageCompletion":15}
            ],
            "cacheUsage":{"promptTokens":150,"cachedTokens":0,"completionTokens":15}}"#;
        let events = parse(text, 0, "k");
        assert_eq!(events.len(), 1, "one turn, one event");
        assert_eq!(events[0].counts.total(), 165.0, "the later reading, not 100+150");
    }
}

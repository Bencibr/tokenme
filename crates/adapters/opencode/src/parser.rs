//! Decoding of an OpenCode `message.data` blob into normalised token counts.

use serde::Deserialize;
use usage_core::TokenCounts;

#[derive(Debug, Default, Deserialize)]
struct Cache {
    #[serde(default)]
    read: f64,
    #[serde(default)]
    write: f64,
}

#[derive(Debug, Default, Deserialize)]
struct Tokens {
    /// What OpenCode itself considers billable for this call; asserted against
    /// `TokenCounts::total()` so a source-side change in stage semantics shows up
    /// as a failing test rather than a silent drift in the numbers.
    #[serde(default)]
    total: f64,
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    reasoning: f64,
    #[serde(default)]
    cache: Cache,
}

#[derive(Debug, Default, Deserialize)]
struct MsgPath {
    #[serde(default)]
    cwd: String,
}

#[derive(Debug, Default, Deserialize)]
struct MessageData {
    #[serde(default)]
    role: String,
    /// Bare model id, e.g. `muse-spark-1.2-contributor-free`. Emitted as-is:
    /// `PricingMap` matches on bare names, and `providerID` (`opencode`, `qimeng`)
    /// is a routing detail that would only break the lookup.
    #[serde(rename = "modelID", default)]
    model_id: String,
    #[serde(default)]
    tokens: Tokens,
    #[serde(default)]
    path: MsgPath,
}

/// A decoded assistant message, ready to become an event.
#[derive(Debug, PartialEq)]
pub(crate) struct Parsed {
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub counts: TokenCounts,
    pub reported_total: f64,
}

fn some_if(text: String) -> Option<String> {
    let t = text.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// `None` for anything that is not a billable assistant record.
pub(crate) fn parse_message(raw: &str) -> Option<Parsed> {
    // Trap: `role` is inside the JSON, which the SQL already filters on; re-check
    // it here rather than trusting the query, because a user message carries a
    // `tokens` object of its own and would double the count.
    let d: MessageData = serde_json::from_str(raw).ok()?;
    if d.role != "assistant" {
        return None;
    }
    let t = &d.tokens;
    let counts = TokenCounts {
        // Trap: unlike Codex, OpenCode's `input` already excludes the cache, so the
        // stages go across one-for-one.
        input: t.input.max(0.0),
        cache_creation: t.cache.write.max(0.0),
        cache_read: t.cache.read.max(0.0),
        // Trap: OpenCode keeps `reasoning` *outside* `output` (55073 + 70 + 214 +
        // 241 = 55598 = tokens.total), while our invariant forbids adding reasoning
        // on top. Folding it into output is what keeps total() honest.
        output: t.output.max(0.0) + t.reasoning.max(0.0),
        reasoning: t.reasoning.max(0.0),
        // The source's own `cost` is ignored on purpose: money is computed
        // centrally from `PricingMap`, so every tool stays comparable.
        credits: 0.0,
    };
    if counts.is_zero() {
        return None;
    }
    Some(Parsed {
        model: some_if(d.model_id),
        cwd: some_if(d.path.cwd),
        reported_total: t.total,
        counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"parentID":"msg_0ce07f070001nCMOF4ihn9bPVs","role":"assistant","mode":"build","agent":"build","variant":"xhigh","path":{"cwd":"/Users/dev/workspace/test","root":"/"},"cost":1.234,"tokens":{"total":55598,"input":55073,"output":70,"reasoning":214,"cache":{"write":0,"read":241}},"modelID":"muse-spark-1.2-contributor-free","providerID":"opencode","time":{"created":1787799862846,"completed":1787799869012},"finish":"stop"}"#;

    #[test]
    fn reasoning_folds_into_output_so_total_matches_the_source() {
        let p = parse_message(SAMPLE).expect("assistant record");
        assert_eq!(p.counts.input, 55073.0);
        assert_eq!(p.counts.cache_read, 241.0);
        assert_eq!(p.counts.cache_creation, 0.0);
        assert_eq!(p.counts.output, 70.0 + 214.0);
        assert_eq!(p.counts.reasoning, 214.0, "kept as the informational sub-split");
        assert_eq!(p.counts.total(), 55598.0);
        assert_eq!(p.counts.total(), p.reported_total);
        assert_eq!(p.model.as_deref(), Some("muse-spark-1.2-contributor-free"));
        assert_eq!(p.cwd.as_deref(), Some("/Users/dev/workspace/test"));
    }

    #[test]
    fn the_sources_own_cost_never_becomes_a_credit_or_a_price() {
        let p = parse_message(SAMPLE).unwrap();
        assert_eq!(p.counts.credits, 0.0);
    }

    #[test]
    fn non_billable_and_broken_records_are_dropped_not_guessed() {
        assert!(parse_message(r#"{"role":"user","tokens":{"total":100,"input":100}}"#).is_none());
        assert!(parse_message(r#"{"role":"assistant","tokens":{"total":0,"input":0,"output":0}}"#).is_none());
        assert!(parse_message(r#"{"role":"assistant"}"#).is_none());
        assert!(parse_message("{oops").is_none());
        // Cache write belongs on its own stage and must not be dropped.
        let p = parse_message(r#"{"role":"assistant","modelID":"m","tokens":{"total":10,"input":1,"output":1,"reasoning":0,"cache":{"write":8,"read":0}}}"#).unwrap();
        assert_eq!((p.counts.cache_creation, p.counts.total()), (8.0, 10.0));
        assert_eq!(p.counts.total(), p.reported_total);
    }
}

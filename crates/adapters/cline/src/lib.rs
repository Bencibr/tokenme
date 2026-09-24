//! cline adapter.
//!
//! Source: `~/.cline/data/sessions/<ts>_<id>/<ts>_<id>.messages.json` — one JSON
//! object rewritten in place, `{version, updated_at, agent, sessionId, origin,
//! system_prompt, messages:[…]}`; assistant entries carry
//! `metrics{inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens}`.
//! The sibling `<ts>_<id>.json` is read for the project label only.
//!
//! Owned by the cline-adapter workstream.
//! The public surface (`ClineAdapter`, `TOOL_ID`) is frozen — `usage-adapter-all`
//! and the CLI already link against it.

mod loader;
mod parser;
mod paths;

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

pub const TOOL_ID: &str = "cline";
pub(crate) const DISPLAY_NAME: &str = "Cline";

#[derive(Debug, Default, Clone, Copy)]
pub struct ClineAdapter;

impl SourceAdapter for ClineAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn semantics(&self) -> Semantics {
        // PerCall: usage is per API call, read from the transcript's `metrics`.
        // `cacheReadTokens` is a *stage* of the prompt, not an extra call — see
        // `parser`'s module docs for why `input` is netted down.
        Semantics {
            usage_form: usage_core::UsageForm::PerCall,
            meter: usage_core::Meter::Tokens,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        loader::probe()
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        loader::discover(filter)
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        loader::read(file, cursor)
    }
}

#[cfg(test)]
mod smoke {
    use std::collections::BTreeSet;
    use std::path::Path;

    use usage_core::{DateFilter, ReadCursor, TokenCounts};

    use super::*;

    /// `metadata.usage` + `metadata.aggregateUsage` of one session meta file.
    #[derive(Debug, serde::Deserialize)]
    struct SessionMeta {
        #[serde(default)]
        metadata: Option<Metadata>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct Metadata {
        #[serde(default)]
        usage: Option<Accumulator>,
        #[serde(default)]
        #[serde(rename = "aggregateUsage")]
        aggregate_usage: Option<Accumulator>,
    }

    #[derive(Debug, Default, Clone, Copy, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Accumulator {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        total_cost: f64,
    }

    fn meta_for(transcript: &Path) -> Option<Accumulator> {
        let path = paths::session_meta_path(transcript)?;
        let text = std::fs::read_to_string(path).ok()?;
        let meta: SessionMeta = serde_json::from_str(&text).ok()?;
        let md = meta.metadata?;
        let usage = md.usage?;
        // The two accumulators are one counter written twice; if a future build
        // ever splits them, this smoke test must say so rather than pick one.
        assert_eq!(Some(usage.total_cost), md.aggregate_usage.map(|a| a.total_cost), "usage and aggregateUsage diverged");
        Some(usage)
    }

    /// Reads the real `~/.cline/data/sessions` and proves the mapping against
    /// the numbers Cline accumulates itself.
    #[test]
    #[ignore = "reads ~/.cline/data/sessions"]
    fn real_data_matches_the_session_accumulator() {
        let adapter = ClineAdapter;
        let files = adapter.discover(&DateFilter::default());
        assert!(!files.is_empty(), "no Cline transcripts under ~/.cline/data/sessions");
        let mut bytes = 0_u64;
        let mut billed = TokenCounts::default();
        let mut reported = TokenCounts::default();
        let mut models = BTreeSet::new();
        let mut events = 0_usize;
        let mut sessions = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for file in &files {
            let outcome = adapter.read(file, ReadCursor(0)).unwrap();
            let stats = loader::stats_for(file);
            assert_eq!(stats.events, outcome.events.len(), "no call is undated: {}", file.path.display());
            if let Some(meta) = meta_for(&file.path) {
                // Per-message `metrics` sum to exactly the session accumulator,
                // which is why the meta file may never produce events.
                let wire = TokenCounts {
                    input: meta.input_tokens as f64,
                    cache_creation: meta.cache_write_tokens as f64,
                    cache_read: meta.cache_read_tokens as f64,
                    output: meta.output_tokens as f64,
                    reasoning: 0.0,
                    credits: 0.0,
                };
                assert_eq!(wire, stats.wire_counts(), "{}", file.path.display());
                reported += &stats.mapped_counts();
            }
            bytes += file.size;
            events += outcome.events.len();
            for e in &outcome.events {
                assert!(e.dedupe_key.as_ref().is_some_and(|k| keys.insert(k.clone())), "duplicate dedupe key across sessions: {:?}", e.dedupe_key);
                sessions.insert(e.session.clone());
                models.insert(e.model.clone().unwrap_or_default());
                billed += &e.counts;
            }
        }
        assert_eq!(billed, reported, "our per-call totals must equal Cline's own accumulator");
        assert_eq!(billed.credits, 0.0, "cost is priced centrally, never read from the source");
        assert!(reported.cache_creation == 0.0, "Cline never reports a cache write: {}", reported.cache_creation);
        println!("[cline] files={} bytes={:.1}MiB sessions={} events={} models={:?}", files.len(), bytes as f64 / 1_048_576.0, sessions.len(), events, models);
        println!("[cline] input(uncached)={:.0} cacheRead={:.0} cacheWrite={:.0} output={:.0} total={:.0} | prompt incl. cache={:.0} (would be {:.2}x if cache were double-counted)", billed.input, billed.cache_read, billed.cache_creation, billed.output, billed.total(), billed.input + billed.cache_read, (billed.input + 2.0 * billed.cache_read) / billed.total());
        assert!(events > 200, "expected the real sessions, got {events}");
    }
}

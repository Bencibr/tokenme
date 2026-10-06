//! cline adapter.
//!
//! Source: `~/.cline/data/sessions/<session>/<name>.messages.json` — one JSON
//! object rewritten in place, `{version, updated_at, agent, sessionId, messages:
//! […]}`; assistant entries carry
//! `metrics{inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens}`. The
//! CLI, the older desktop build and the current one differ only in how they name
//! the session dir, and a desktop sub-agent's transcript lives inside its
//! parent's dir — see `paths`. A session meta (`<name>.json`) is read for the
//! project label only.
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use usage_core::{DateFilter, ReadCursor, TokenCounts};

    use super::*;

    /// One session dir's ledger, as Cline accumulates it: `metadata.usage` covers
    /// the orchestrator transcript and `metadata.aggregateUsage` that transcript
    /// plus every sub-agent's, so the wider one is the dir's total.
    fn ledger(dir: &Path) -> Option<(TokenCounts, i64)> {
        let name = dir.file_name()?.to_str()?;
        let path = dir.join(format!("{name}.json"));
        #[derive(Debug, Default, serde::Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        struct Accumulator {
            input_tokens: u64,
            output_tokens: u64,
            cache_read_tokens: u64,
            cache_write_tokens: u64,
        }
        #[derive(serde::Deserialize)]
        struct Meta {
            metadata: Option<MetaInner>,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct MetaInner {
            #[serde(default)]
            usage: Option<Accumulator>,
            #[serde(default)]
            aggregate_usage: Option<Accumulator>,
        }
        let text = std::fs::read_to_string(&path).ok()?;
        let meta: Meta = serde_json::from_str(&text).ok()?;
        let inner = meta.metadata?;
        let acc = inner.aggregate_usage.or(inner.usage)?;
        let mtime = std::fs::metadata(&path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as i64;
        // The adapter's stage caliber: `inputTokens` already holds the cached
        // prefix, so the net input is the difference.
        Some((
            TokenCounts {
                input: acc.input_tokens.saturating_sub(acc.cache_read_tokens) as f64,
                cache_creation: acc.cache_write_tokens as f64,
                cache_read: acc.cache_read_tokens as f64,
                output: acc.output_tokens as f64,
                reasoning: 0.0,
                credits: 0.0,
            },
            mtime,
        ))
    }

    /// Reads the real `~/.cline/data/sessions` and asserts what the desktop
    /// dialect cannot break: every call is dated, every transcript finds a meta
    /// that names its workspace (including a sub-agent's, whose own meta lives in
    /// a sibling `…__agent_<uuid>/` dir or not at all), no dedupe key repeats
    /// across sessions, Cline bills no cache-write tier, and no money is read
    /// from the source. The numeric reconciliation against the ledger printed
    /// below is `scripts/verify-totals.py`'s `cline._vendor` check — it owns the
    /// tolerance and the vendor-pruned-row accounting, and a ledger written
    /// before its transcripts (a session still running) proves nothing here.
    #[test]
    #[ignore = "reads ~/.cline/data/sessions"]
    fn real_data_matches_the_session_accumulator() {
        let adapter = ClineAdapter;
        let files = adapter.discover(&DateFilter::default());
        assert!(!files.is_empty(), "no Cline transcripts under ~/.cline/data/sessions");

        let mut per_dir: BTreeMap<PathBuf, (TokenCounts, i64)> = BTreeMap::new();
        let mut keys = BTreeSet::new();
        let mut models = BTreeSet::new();
        let mut events = 0_usize;
        let mut billed = TokenCounts::default();
        for file in &files {
            let outcome = adapter.read(file, ReadCursor(0)).unwrap();
            let stats = loader::stats_for(file);
            assert_eq!(stats.events, outcome.events.len(), "no call is undated: {}", file.path.display());
            let dir = file.path.parent().expect("a transcript lives in a session dir").to_path_buf();
            let slot = per_dir.entry(dir).or_default();
            for e in &outcome.events {
                assert!(
                    e.dedupe_key.as_ref().is_some_and(|k| keys.insert(k.clone())),
                    "duplicate dedupe key across sessions: {:?}",
                    e.dedupe_key
                );
                assert!(e.project.is_some(), "transcript with no workspace label: {}", file.path.display());
                models.insert(e.model.clone().unwrap_or_default());
                billed += &e.counts;
                slot.0 += &e.counts;
            }
            slot.1 = slot.1.max(file.mtime_ms);
            events += outcome.events.len();
        }
        assert_eq!(billed.cache_creation, 0.0, "Cline never reports a cache write: {}", billed.cache_creation);
        assert_eq!(billed.credits, 0.0, "cost is priced centrally, never read from the source");
        assert!(events > 200, "expected the real sessions, got {events}");

        let mut settled = 0_usize;
        let mut lagging = 0_usize;
        for (dir, (ours, newest)) in &per_dir {
            let Some((theirs, meta_mtime)) = ledger(dir) else { continue };
            if meta_mtime < *newest {
                lagging += 1;
                continue;
            }
            settled += 1;
            println!(
                "[cline] {:<46} ledger in/cr/out={:>12.0}/{:>12.0}/{:>9.0}  ours={:>12.0}/{:>12.0}/{:>9.0}  Δ={:+.0}/{:+.0}/{:+.0}",
                dir.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                theirs.input,
                theirs.cache_read,
                theirs.output,
                ours.input,
                ours.cache_read,
                ours.output,
                theirs.input - ours.input,
                theirs.cache_read - ours.cache_read,
                theirs.output - ours.output,
            );
            assert_eq!(theirs.cache_creation, 0.0, "the ledger bills a cache-write tier: {}", dir.display());
        }
        println!("[cline] files={} sessions={} settled={} ledger-predates-its-transcripts={} events={} models={:?}", files.len(), per_dir.len(), settled, lagging, events, models);
        println!("[cline] billed input(uncached)={:.0} cacheRead={:.0} cacheWrite={:.0} output={:.0} | prompt incl. cache={:.0}", billed.input, billed.cache_read, billed.cache_creation, billed.output, billed.input + billed.cache_read);
    }
}

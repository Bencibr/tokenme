//! Tuanjie (Unity China) Codely / Cowork adapter.
//!
//! One source, two faces: the Codely CLI writes the chat snapshots and the
//! Tuanjie Cowork desktop app drives that same CLI over ACP, so both products'
//! calls land in `~/.codely-cli/tmp/<project-hash>/chat-<session>.json`. Reading
//! them separately would double count one account's work; this adapter is one
//! tool id for both.
//!
//! Each assistant message carries its own `tokens` object
//! (`input`/`output`/`cached`/`thoughts`/`tool`), which is a per-call record —
//! see [`parser`]. The plan windows (5-hour and weekly) are not in the logs at
//! all and come from the account's usage endpoint in
//! `usage-quota::providers::codely`.
//!
//! The public surface (`CodelyAdapter`, `TOOL_ID`) follows the frozen contract
//! the other adapter crates use: `usage-adapter-all` links against it.

mod loader;
mod parser;
mod paths;

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

pub const TOOL_ID: &str = "codely";
pub(crate) const DISPLAY_NAME: &str = "Codely";

#[derive(Debug, Default, Clone, Copy)]
pub struct CodelyAdapter;

impl SourceAdapter for CodelyAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            // One `tokens` object per API round, so nothing is a running total.
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

    use usage_core::{DateFilter, ReadCursor, TokenCounts};

    use super::*;

    /// Reads the real store and asserts what the snapshot dialect cannot break:
    /// every emitted call has a stamp and an identity, replaying a file yields
    /// the same events, no key repeats, and the split's own accounting matches
    /// the wire's `input + output`. The numeric totals are printed for the
    /// accuracy ledger (`scripts/verify-totals.py` owns the independent SQL).
    #[test]
    #[ignore = "reads ~/.codely-cli/tmp"]
    fn real_snapshots_replay_identically_and_conserve_the_wire_total() {
        let adapter = CodelyAdapter;
        let files = adapter.discover(&DateFilter::default());
        assert!(!files.is_empty(), "no Codely chat snapshots under ~/.codely-cli/tmp");

        let mut keys = BTreeSet::new();
        let mut billed = TokenCounts::default();
        let mut wire = TokenCounts::default();
        let mut events = 0_usize;
        for file in &files {
            let first = adapter.read(file, ReadCursor(0)).unwrap();
            let again = adapter.read(file, ReadCursor(0)).unwrap();
            assert_eq!(again.events, first.events, "a re-read must not change identity: {}", file.path.display());
            let stats = loader::stats_for(file);
            let subtotal = first.events.iter().fold(TokenCounts::default(), |mut all, event| {
                all += &event.counts;
                all
            });
            billed += &subtotal;
            wire.input += stats.wire_input.saturating_sub(stats.wire_cache_read) as f64;
            wire.cache_read += stats.wire_cache_read as f64;
            wire.output += stats.wire_output as f64;
            for event in &first.events {
                assert!(event.ts_ms > 0, "undated event in {}", file.path.display());
                let key = event.dedupe_key.as_ref().expect("every Codely event carries an identity");
                assert!(keys.insert(key.clone()), "duplicate dedupe key across snapshots: {key}");
                assert!(event.model.is_some(), "a billed call with no model: {key}");
                assert!(event.project.is_some(), "the startup snapshot lost its cwd: {key}");
                events += 1;
            }
            assert_eq!(stats.events, first.events.len(), "wire census differs from emitted events: {}", file.path.display());
        }
        // `total()` is the disjoint-stage sum; `input + output` is what the wire
        // says. `reasoning` is a sub-breakdown and never added on top.
        let wire_total = wire.input + wire.cache_read + wire.output;
        println!(
            "[codely] files={} events={} models={:?}",
            files.len(),
            events,
            files
                .iter()
                .flat_map(|file| adapter.read(file, ReadCursor(0)).unwrap().events)
                .filter_map(|event| event.model)
                .collect::<BTreeSet<_>>()
        );
        println!(
            "[codely] billed input(uncached)={:.0} cacheRead={:.0} output={:.0} total={:.0} | wire total={:.0}",
            billed.input,
            billed.cache_read,
            billed.output,
            billed.total(),
            wire_total
        );
        assert_eq!(billed.total(), wire_total, "the stage split must conserve the wire's input + output");
        assert_eq!(billed.cache_creation, 0.0, "Codely never reports a cache write");
        assert_eq!(billed.credits, 0.0, "cost is priced centrally, never read from the source");
    }
}

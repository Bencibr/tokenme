//! Grok Build (xAI) adapter.
//!
//! Grok Build (`grok` CLI / TUI / ACP) writes every conversation under
//! `$GROK_HOME/sessions/<cwd-encoded>/<session-id>/`. The billable ledger is
//! `usage.json` — per-turn token totals with cache and reasoning breakdowns.
//! Live SuperGrok weekly allowance is **not** in those files; it comes from
//! `usage-quota::providers::grokbuild` via the same `~/.grok/auth.json` login
//! (and, when `isUnifiedBillingUser`, covers Cursor / Grok Bot / Chat too).
//!
//! The public surface (`GrokBuildAdapter`, `TOOL_ID`) follows the frozen
//! contract the other adapter crates use: `usage-adapter-all` links against it.

mod loader;
mod parser;
mod paths;

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

pub const TOOL_ID: &str = "grokbuild";
pub(crate) const DISPLAY_NAME: &str = "Grok Build";

#[derive(Debug, Default, Clone, Copy)]
pub struct GrokBuildAdapter;

impl SourceAdapter for GrokBuildAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn semantics(&self) -> Semantics {
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

    use usage_core::{DateFilter, ReadCursor, TokenCounts};

    use super::*;

    #[test]
    #[ignore = "reads ~/.grok/sessions"]
    fn real_usage_files_replay_identically_and_conserve_the_wire_total() {
        let adapter = GrokBuildAdapter;
        let files = adapter.discover(&DateFilter::default());
        assert!(
            !files.is_empty(),
            "no Grok Build usage.json under ~/.grok/sessions"
        );

        let mut keys = BTreeSet::new();
        let mut billed = TokenCounts::default();
        let mut wire = TokenCounts::default();
        let mut events = 0_usize;
        for file in &files {
            let first = adapter.read(file, ReadCursor(0)).unwrap();
            let again = adapter.read(file, ReadCursor(0)).unwrap();
            assert_eq!(
                again.events, first.events,
                "a re-read must not change identity: {}",
                file.path.display()
            );
            let stats = loader::stats_for(file);
            let subtotal = first.events.iter().fold(TokenCounts::default(), |mut all, event| {
                all += &event.counts;
                all
            });
            billed += &subtotal;
            wire.input += stats.wire_input.saturating_sub(
                stats.wire_cache_read.saturating_add(stats.wire_cache_creation),
            ) as f64;
            wire.cache_read += stats.wire_cache_read as f64;
            wire.cache_creation += stats.wire_cache_creation as f64;
            wire.output += stats.wire_output as f64;
            for event in &first.events {
                assert!(event.ts_ms > 0, "undated event in {}", file.path.display());
                let key = event
                    .dedupe_key
                    .as_ref()
                    .expect("every Grok Build event carries an identity");
                assert!(
                    keys.insert(key.clone()),
                    "duplicate dedupe key across sessions: {key}"
                );
                assert!(event.model.is_some(), "a billed call with no model: {key}");
                events += 1;
            }
            assert_eq!(
                stats.events,
                first.events.len(),
                "wire census differs from emitted events: {}",
                file.path.display()
            );
        }
        let wire_total = wire.input + wire.cache_read + wire.cache_creation + wire.output;
        println!(
            "[grokbuild] files={} events={} billed_total={} wire_total={}",
            files.len(),
            events,
            billed.total(),
            wire_total
        );
        assert_eq!(billed.total(), wire_total);
    }
}

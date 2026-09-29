//! The Codex accuracy audit: an independent replay of every rollout file,
//! compared with what the index holds.
//!
//! Codex has no vendor-side ledger to reconcile against — the rollout JSONL
//! files are the only record — so "was anything counted twice" is answered by
//! replay: re-parse every session from scratch (cursor 0, no dedupe, no
//! manifest) and aggregate per session. If the indexer double-counted — a
//! cursor regression, a re-ingest that slipped past dedupe — the replay sums
//! and the indexed sums diverge, and [`sessions`] hands both sides to
//! `tokenme codex-doctor` / the scan log.

use std::collections::BTreeMap;

use usage_core::{DateFilter, ReadCursor};

use crate::{loader, paths};

/// One session's totals, re-derived from the raw rollout files.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub session: String,
    pub files: usize,
    pub calls: usize,
    pub input: f64,
    pub output: f64,
    pub cached: f64,
    pub reasoning: f64,
}

/// Replay every rollout under the Codex sessions root, aggregating per
/// session. `filter` bounds the files by mtime exactly as discovery does —
/// pass the index's retention cutoff to compare like with like.
pub fn sessions(filter: &DateFilter) -> Vec<Row> {
    let Some(home) = paths::codex_home() else { return Vec::new() };
    let root = paths::sessions_root(&home);
    let mut by_session: BTreeMap<String, Row> = BTreeMap::new();
    for file in paths::list_rollouts(&root, filter) {
        let outcome = loader::read_rollout(&file.path, ReadCursor(0), &file.key());
        for event in &outcome.events {
            let row = by_session.entry(event.session.clone()).or_default();
            row.session = event.session.clone();
            row.calls += 1;
            row.input += event.counts.input;
            row.output += event.counts.output;
            row.cached += event.counts.cache_read + event.counts.cache_creation;
            row.reasoning += event.counts.reasoning;
        }
    }
    // File counts ride the same walk (a session may span several rollouts).
    for file in paths::list_rollouts(&root, filter) {
        // Re-derive the session id cheaply: the file name is the rollout uuid,
        // but sessions may share a file set; count files against the session
        // their first event names.
        let outcome = loader::read_rollout(&file.path, ReadCursor(0), &file.key());
        if let Some(event) = outcome.events.first() {
            if let Some(row) = by_session.get_mut(&event.session) {
                row.files += 1;
            }
        }
    }
    let mut rows: Vec<Row> = by_session.into_values().collect();
    rows.sort_by(|a, b| a.session.cmp(&b.session));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_home_answers_an_empty_replay() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CODEX_HOME", dir.path());
        assert!(sessions(&DateFilter::default()).is_empty());
        std::env::remove_var("CODEX_HOME");
    }
}

//! WorkBuddy AI adapter (Tencent's desktop agent, `com.workbuddy.workbuddy-ai`).
//!
//! One usage source: `~/.workbuddy-ai/projects/**/<session>.jsonl`, the app's own
//! transcripts. Every LLM call appends a line whose `providerData.rawUsage` is
//! the vendor's unmodified billing payload — prompt/completion tokens, the cache
//! hit and write, the thinking sub-split, and `credit` for that one call — so
//! this tool meters like every other one in the index: [`Meter::Tokens`] events
//! with a credit column riding along, priced centrally by tokenme.
//!
//! Sub-agents are not a separate product: their transcripts sit under the
//! spawning session's directory and their credit belongs to it, which is the only
//! way the per-call totals reconcile with `session_usage.credit_json` (314.54
//! both ways on this machine, 2026-09-29).
//!
//! `workbuddy.db` is read for the session head count in `probe` and for nothing
//! else ([`db`]); the account's remaining credits is the `usage-quota` provider's
//! job, not this adapter's.
//!
//! The public surface (`TOOL_ID`, `WorkBuddyAdapter`) follows the same frozen
//! contract as the other adapters: `usage-adapter-all` links it.

use usage_core::{DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile};

mod db;
mod paths;
mod transcript;

pub const TOOL_ID: &str = "workbuddy";

#[derive(Debug, Default, Clone, Copy)]
pub struct WorkBuddyAdapter;

impl SourceAdapter for WorkBuddyAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "WorkBuddy AI"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            // One line per call, carrying that call's own usage.
            usage_form: usage_core::UsageForm::PerCall,
            meter: usage_core::Meter::Tokens,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let roots = transcript::roots();
        let files = roots.iter().flat_map(|root| transcript::discover(root, None)).count();
        if files == 0 {
            return None;
        }
        let sessions = paths::db_path().and_then(|db| db::count_sessions(&db)).unwrap_or(0);
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots,
            hint: Some(format!("{sessions} sessions · {files} transcripts")),
        })
    }

    fn discover(&self, filter: &usage_core::DateFilter) -> Vec<SourceFile> {
        let mut files: Vec<SourceFile> = transcript::roots()
            .iter()
            .flat_map(|root| transcript::discover(root, filter.since_ms))
            .collect();
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        files
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        match file.kind {
            FileKind::Jsonl => transcript::read(file, cursor),
            _ => Ok(ReadOutcome { events: Vec::new(), cursor }),
        }
    }
}

/// Domestic WorkBuddy: the same transcript dialect in a separate data store.
#[derive(Debug, Default, Clone, Copy)]
pub struct WorkBuddyCnAdapter;

impl SourceAdapter for WorkBuddyCnAdapter {
    fn id(&self) -> &'static str { "workbuddy_cn" }
    fn display_name(&self) -> &'static str { "WorkBuddy" }
    fn semantics(&self) -> Semantics { WorkBuddyAdapter.semantics() }

    fn probe(&self) -> Option<DetectedSource> {
        let roots = transcript::roots_for(true);
        let files = roots.iter().flat_map(|root| transcript::discover(root, None)).count();
        if files == 0 { return None; }
        let sessions = paths::db_path_for(true).and_then(|db| db::count_sessions(&db)).unwrap_or(0);
        Some(DetectedSource {
            id: self.id().into(), display: self.display_name().into(), roots,
            hint: Some(format!("{sessions} sessions · {files} transcripts")),
        })
    }

    fn discover(&self, filter: &usage_core::DateFilter) -> Vec<SourceFile> {
        let mut files: Vec<_> = transcript::roots_for(true).iter()
            .flat_map(|root| transcript::discover(root, filter.since_ms)).collect();
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        files
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        match file.kind {
            FileKind::Jsonl => transcript::read_for(file, cursor, self.id()),
            _ => Ok(ReadOutcome { events: Vec::new(), cursor }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editions_keep_their_roots_and_event_keys_separate() {
        let _env = paths::lock_env();
        let global = tempfile::tempdir().unwrap();
        let cn = tempfile::tempdir().unwrap();
        transcript::tests::fixture(global.path());
        transcript::tests::fixture(cn.path());
        std::env::set_var(paths::ENV_CONFIG_DIR, global.path());
        std::env::set_var("WORKBUDDY_CN_CONFIG_DIR", cn.path());
        let filter = usage_core::DateFilter::default();
        let global_files = WorkBuddyAdapter.discover(&filter);
        let cn_files = WorkBuddyCnAdapter.discover(&filter);
        std::env::remove_var(paths::ENV_CONFIG_DIR);
        std::env::remove_var("WORKBUDDY_CN_CONFIG_DIR");
        assert!(global_files.iter().all(|f| f.path.starts_with(global.path())));
        assert!(cn_files.iter().all(|f| f.path.starts_with(cn.path())));
        let g = WorkBuddyAdapter.read(&global_files[0], ReadCursor(0)).unwrap();
        let c = WorkBuddyCnAdapter.read(&cn_files[0], ReadCursor(0)).unwrap();
        assert_eq!(g.events[0].tool, "workbuddy");
        assert_eq!(c.events[0].tool, "workbuddy_cn");
        assert_ne!(g.events[0].dedupe_key, c.events[0].dedupe_key);
        assert_eq!(g.events[0].counts, c.events[0].counts);
        assert_eq!(WorkBuddyAdapter.display_name(), "WorkBuddy AI");
        assert_eq!(WorkBuddyCnAdapter.display_name(), "WorkBuddy");
    }

    #[test]
    fn an_absent_store_probes_as_not_installed() {
        let _env = paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(paths::ENV_CONFIG_DIR, dir.path());
        assert!(WorkBuddyAdapter.probe().is_none());
        assert!(WorkBuddyAdapter.discover(&usage_core::DateFilter::default()).is_empty());
        std::env::remove_var(paths::ENV_CONFIG_DIR);
    }

    #[test]
    fn transcripts_are_discovered_as_jsonl_sources_including_sub_agents() {
        let _env = paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let files = transcript::tests::fixture(dir.path());
        std::env::set_var(paths::ENV_CONFIG_DIR, dir.path());
        let outcome = WorkBuddyAdapter.discover(&usage_core::DateFilter::default());
        let probe = WorkBuddyAdapter.probe().unwrap();
        std::env::remove_var(paths::ENV_CONFIG_DIR);
        assert_eq!(outcome.len(), files.len());
        assert!(
            outcome.iter().all(|file| file.kind == FileKind::Jsonl),
            "the adapter only ever reads transcripts"
        );
        assert!(outcome.iter().any(|file| file.path.to_string_lossy().contains("subagents")));
        assert_eq!(probe.roots.len(), 1, "the env override points at one root");
        let hint = probe.hint.unwrap();
        assert!(hint.contains("2 transcripts"), "{hint}");
    }
}

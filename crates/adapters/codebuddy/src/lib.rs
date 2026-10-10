//! CodeBuddy adapter (Tencent's CodeBuddy CN desktop IDE, `CodeBuddy CN.exe`).
//!
//! One usage source: the shared agent runtime's history tree —
//! `%LOCALAPPDATA%\CodeBuddyExtension\Data\<uid>\CodeBuddyIDE\<uid>\history\`
//! — where each conversation keeps a `messages/` directory of one JSON file per
//! message. The turn's final assistant message carries `extra.statsSnapshot`:
//! that turn's token stages plus `credit`, the vendor's own billing unit, so
//! this tool meters like its sibling WorkBuddy — [`Meter::Tokens`] events with
//! a credit riding along, priced centrally by tokenme. The transcript bodies
//! themselves stay server-side; only the usage snapshots land here, which is
//! all the index needs.
//!
//! The account's remaining Craft credits is the `usage-quota` provider's job
//! (`workbuddy.cn`'s billing meter with the login token the app caches
//! encrypted), not this adapter's.
//!
//! The public surface (`TOOL_ID`, `CodeBuddyAdapter`) follows the same frozen
//! contract as the other adapters: `usage-adapter-all` links it.

use usage_core::{DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile};

mod paths;
mod transcript;

pub const TOOL_ID: &str = "codebuddy";

/// The dedupe namespace for per-turn transcript events.
pub(crate) const DEDUPE_PREFIX: &str = "codebuddy#";

#[derive(Debug, Default, Clone, Copy)]
pub struct CodeBuddyAdapter;

impl SourceAdapter for CodeBuddyAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "CodeBuddy"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            // One snapshot per turn, carrying that turn's own usage.
            usage_form: usage_core::UsageForm::PerCall,
            meter: usage_core::Meter::Tokens,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let roots = transcript::roots();
        let files: usize = roots.iter().map(|root| transcript::discover(root, None).len()).sum();
        if files == 0 {
            return None;
        }
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots,
            hint: Some(format!("{files} transcripts")),
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

    fn read(&self, file: &SourceFile, _cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        match file.kind {
            FileKind::Tree => Ok(transcript::read(file)),
            _ => Ok(ReadOutcome { events: Vec::new(), cursor: ReadCursor(0) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::Meter;

    #[test]
    fn an_absent_store_probes_as_not_installed() {
        let _env = paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(paths::ENV_DATA_DIR, dir.path());
        assert!(CodeBuddyAdapter.probe().is_none());
        assert!(CodeBuddyAdapter.discover(&usage_core::DateFilter::default()).is_empty());
        std::env::remove_var(paths::ENV_DATA_DIR);
    }

    #[test]
    fn the_turn_snapshot_becomes_one_netted_tokens_event() {
        let _env = paths::lock_env();
        let dir = tempfile::tempdir().unwrap();
        transcript::tests::fixture(dir.path());
        std::env::set_var(paths::ENV_DATA_DIR, dir.path());
        let files = CodeBuddyAdapter.discover(&usage_core::DateFilter::default());
        std::env::remove_var(paths::ENV_DATA_DIR);
        assert_eq!(files.len(), 2, "one source per message file");
        assert!(files.iter().all(|file| file.kind == FileKind::Tree));

        let mut events = Vec::new();
        for file in &files {
            events.extend(CodeBuddyAdapter.read(file, ReadCursor(0)).unwrap().events);
        }
        assert_eq!(events.len(), 1, "only the turn-final assistant message bills");
        let event = &events[0];
        assert_eq!(event.tool, "codebuddy");
        assert_eq!(event.session, "conv-1");
        assert_eq!(event.model.as_deref(), Some("auto"));
        assert_eq!(event.dedupe_key.as_deref(), Some("codebuddy#msg-1"));
        assert_eq!(event.meter, Meter::Tokens);
        // inputTokens 79962 = 56224 cached + 23738 miss: the stages net apart.
        let counts = &event.counts;
        assert_eq!(counts.input, 23738.0);
        assert_eq!(counts.cache_read, 56224.0);
        assert_eq!(counts.cache_creation, 0.0);
        assert_eq!(counts.output, 2448.0);
        assert_eq!(counts.reasoning, 183.0);
        assert_eq!(counts.credits, 2.69);
        assert_eq!(counts.total(), 23738.0 + 56224.0 + 2448.0);
    }
}

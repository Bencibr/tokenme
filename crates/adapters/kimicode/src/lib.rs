//! Kimi Code adapter (Moonshot AI, `MoonshotAI/kimi-code`).
//!
//! One source: the tool's own event log, `sessions/**/wire.jsonl`, whose
//! `usage.record` lines carry one LLM call each with its four prompt/output stages
//! (see [`parser`]). Usage lives there whether the credential behind it is a
//! membership login or an API key, which is why this is an adapter *and* a quota
//! probe: the windows Kimi Code is sold on (5-hour, 7-day, monthly) are not in the
//! logs at all and come from the vendor's `/usages` interface
//! (`usage-quota::providers::kimicode`).
//!
//! Roots resolve as `KIMI_CODE_HOME` → `KIMI_DATA_DIR` → `~/.kimi-code` →
//! `~/.kimi`, plus the home the **desktop client** provisions for its embedded
//! runtime (`Kimi.app`'s Electron userData, see [`paths`]) — real usage flows
//! through that one, so it is read wherever it exists; see [`paths`].
//!
//! The public surface (`TOOL_ID`, `KimiCodeAdapter`) follows the same frozen
//! contract as the other adapters: `usage-adapter-all` links it.

use usage_core::{
    DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

mod loader;
mod parser;
mod paths;

pub const TOOL_ID: &str = "kimicode";

#[derive(Debug, Default, Clone, Copy)]
pub struct KimiCodeAdapter;

impl SourceAdapter for KimiCodeAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "Kimi Code"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            // One record per LLM call, with that call's own stages — so nothing
            // is a running total and `Cumulative` would be a lie.
            usage_form: usage_core::UsageForm::PerCall,
            meter: usage_core::Meter::Tokens,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let files = paths::wire_files();
        if files.is_empty() {
            return None;
        }
        let bytes: u64 = files
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        let roots = paths::homes();
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots,
            hint: Some(format!("{} wire logs, {}", files.len(), human_bytes(bytes))),
        })
    }

    fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
        // The date filter cannot prune a per-session log listing: a session that
        // started before the window still ends inside it, and the row is what the
        // indexer bounds, not the file.
        paths::wire_files().into_iter().filter_map(loader::source_file).collect()
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        // Deliberately infallible: a log the CLI is mid-write through must not
        // abort the ingest pass for the other sources.
        match file.kind {
            FileKind::Tree => Ok(loader::read_file(file, cursor)),
            _ => Ok(ReadOutcome { events: Vec::new(), cursor }),
        }
    }
}

/// A hint that says how much was read, in the units a person compares at.
fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let size = bytes as f64;
    if size < KIB {
        format!("{bytes} B")
    } else if size < KIB * KIB {
        format!("{:.1} KiB", size / KIB)
    } else if size < KIB * KIB * KIB {
        format!("{:.1} MiB", size / (KIB * KIB))
    } else {
        format!("{:.1} GiB", size / (KIB * KIB * KIB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_install_probes_as_not_installed() {
        let _guard = paths::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("KIMI_DATA_DIR", dir.path());
        assert!(KimiCodeAdapter.probe().is_none());
        assert!(KimiCodeAdapter.discover(&DateFilter::default()).is_empty());
        std::env::remove_var("KIMI_DATA_DIR");
    }

    #[test]
    fn a_wire_log_is_offered_as_a_tree_source_and_says_what_it_holds() {
        let _guard = paths::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("KIMI_DATA_DIR", dir.path());
        let agent = dir.path().join("sessions").join("wd_x").join("ses_1").join("agents").join("main");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("wire.jsonl"),
            "{\"type\":\"usage.record\",\"agentId\":\"main\",\"model\":\"m\",\"usage\":{\"inputOther\":5,\"output\":5},\"usageScope\":\"turn\",\"time\":1787799862846}\n",
        )
        .unwrap();

        let found = KimiCodeAdapter.discover(&DateFilter::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, FileKind::Tree, "the CLI rewrites this file, so no byte cursor");
        let detected = KimiCodeAdapter.probe().expect("detected");
        assert_eq!(detected.id, "kimicode");
        assert_eq!(detected.display, "Kimi Code");
        assert!(detected.hint.unwrap().starts_with("1 wire logs"), "hint names the count");
        std::env::remove_var("KIMI_DATA_DIR");
    }

    #[test]
    fn the_sizes_a_hint_reports_stay_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn the_contract_the_index_keys_on() {
        let sem = KimiCodeAdapter.semantics();
        assert!(sem.dedupes_by_id, "re-reads of a rewritten log must not double bill");
        assert_eq!(sem.usage_form, usage_core::UsageForm::PerCall);
        assert_eq!(sem.meter, usage_core::Meter::Tokens);
        assert!(!sem.reports_quota, "the windows come from the probe, not the logs");
    }
}

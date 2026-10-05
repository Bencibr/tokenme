//! MiniMax Code adapter (MiniMax, `MiniMax-AI/minimax-code`, open source 2026-09-18).
//!
//! One source: the session transcript `v2/sessions/**/messages.jsonl`, whose
//! assistant records carry one LLM call each with its four prompt/output stages
//! (see [`parser`]). Usage lives there whether the plan behind it is a Token Plan
//! subscription or a bring-your-own key, which is why this is an adapter *and* a
//! quota probe: the 5-hour and weekly windows MiniMax Code is sold on are not in
//! the logs at all and come from the vendor's own interface
//! (`usage-quota::providers::minimaxcode`).
//!
//! The product is the desktop app plus its CLI launcher (`~/.minimax/bin/mcode-tools`
//! → `MiniMax Code.app`), and it is built on the vendored `pi-mono` coding agent —
//! so the usage *stages* are pi's, while the *envelope* and the dated session
//! layout are MiniMax's own. That is why this is its own adapter rather than a
//! `crates/adapters/pi`-style sibling sharing that crate's parser.
//!
//! Roots resolve as `MINIMAX_DATA_DIR` → `MAVIS_DATA_DIR` → `~/.minimax[-profile]`
//! (+ the legacy `~/.mavis[-profile]`, which the vendor leaves as a symlink); see
//! [`paths`].
//!
//! The public surface (`TOOL_ID`, `MiniMaxCodeAdapter`) follows the same frozen
//! contract as the other adapters: `usage-adapter-all` links it.

use usage_core::{
    DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

mod loader;
mod parser;
mod paths;

pub const TOOL_ID: &str = "minimaxcode";

#[derive(Debug, Default, Clone, Copy)]
pub struct MiniMaxCodeAdapter;

impl SourceAdapter for MiniMaxCodeAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "MiniMax Code"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            // One record per LLM call, with that call's own stages: the vendor's
            // projection of these very records into its own usage table appends a
            // row per message (`recordLocalTokenUsageFromPiMessages`), so nothing
            // here is a running total and `Cumulative` would be a lie.
            usage_form: usage_core::UsageForm::PerCall,
            meter: usage_core::Meter::Tokens,
            model_attr: usage_core::ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let files = paths::history_files();
        if files.is_empty() {
            return None;
        }
        let bytes: u64 = files
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        let mut hint = format!("{} session logs, {}", files.len(), human_bytes(bytes));
        // Naming the store the panel cannot price without: its absence means the
        // sessions have no workspace label, which is a visible difference in the
        // projects page rather than an error.
        if paths::runtime_db().is_none() {
            hint.push_str(" · no runtime store");
        }
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: paths::homes(),
            hint: Some(hint),
        })
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        paths::history_files()
            .into_iter()
            .filter(|path| {
                // The dated directory is the session's *creation* day, so it is a
                // lower bound on every call inside: a session that opens after the
                // window ends cannot have billed in it. The lower bound cannot be
                // used the same way — a session opened months ago still bills
                // today — so `since_ms` deliberately prunes nothing here.
                match (paths::created_day_ms(path), filter.until_ms) {
                    (Some(day), Some(until)) => day <= until,
                    _ => true,
                }
            })
            .filter_map(loader::source_file)
            .collect()
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        // Deliberately infallible: a transcript the app is mid-rewrite through must
        // not abort the ingest pass for the other sources.
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
        std::env::set_var("MINIMAX_DATA_DIR", dir.path());
        assert!(MiniMaxCodeAdapter.probe().is_none());
        assert!(MiniMaxCodeAdapter.discover(&DateFilter::default()).is_empty());
        std::env::remove_var("MINIMAX_DATA_DIR");
    }

    #[test]
    fn a_transcript_is_offered_as_a_tree_source_and_says_what_it_holds() {
        let _guard = paths::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("MINIMAX_DATA_DIR", dir.path());
        let session = dir.path().join("v2/sessions/2026/10/05/00-07-16-076-session_abc");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("manifest.json"), r#"{"sessionId":"mvs_1"}"#).unwrap();
        std::fs::write(
            session.join("messages.jsonl"),
            "{\"message_id\":\"msg-1\",\"message\":{\"role\":\"assistant\",\"model\":\"MiniMax-M2.7\",\"timestamp\":1791158837144,\"usage\":{\"input\":9,\"output\":5,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":14}}}\n",
        )
        .unwrap();

        let found = MiniMaxCodeAdapter.discover(&DateFilter::default());
        assert_eq!(found.len(), 1);
        // Re-read whole, because the vendor rewrites the file: a byte cursor would
        // resume into a history that has been re-materialised underneath it.
        assert_eq!(found[0].kind, FileKind::Tree);
        let detected = MiniMaxCodeAdapter.probe().expect("minimax code detected");
        assert_eq!(detected.id, "minimaxcode");
        assert_eq!(detected.display, "MiniMax Code");
        assert_eq!(detected.roots, vec![dir.path().to_path_buf()]);
        assert!(
            detected.hint.as_deref().unwrap().starts_with("1 session log"),
            "the hint counts what it read: {:?}",
            detected.hint
        );
        std::env::remove_var("MINIMAX_DATA_DIR");
    }

    #[test]
    fn a_window_that_ended_before_the_session_opened_prunes_the_file() {
        let _guard = paths::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("MINIMAX_DATA_DIR", dir.path());
        let session = dir.path().join("v2/sessions/2026/10/05/00-07-16-076-session_abc");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("messages.jsonl"), "").unwrap();

        // 2026-10-04T12:00Z is before the creation day starts, so nothing in the
        // file can fall inside the window.
        let past = DateFilter::new(None, Some(1_791_115_200_000));
        assert!(MiniMaxCodeAdapter.discover(&past).is_empty());
        // The same file is offered for a window that reaches its creation day…
        assert_eq!(MiniMaxCodeAdapter.discover(&DateFilter::new(None, Some(1_791_158_400_000))).len(), 1);
        // …and a lower bound never hides it, however old the session is.
        assert_eq!(MiniMaxCodeAdapter.discover(&DateFilter::new(Some(1_791_200_000_000), None)).len(), 1);
        std::env::remove_var("MINIMAX_DATA_DIR");
    }
}

//! FunIDE adapter (唐山趣绮梦's VS Code fork with a built-in agent).
//!
//! One source: `<data>/FunIDE/User/funide/sessions/<workspace>/<session>.json`
//! — each file is a whole session (Tree kind): the UI's own `cacheUsage`
//! cumulative ledger, the per-turn `items[].usage*` numbers, the session model,
//! and the workspace key. One event per billed turn, keyed on the turn's own id
//! and stamped with the turn's own start; the file's mtime is the cursor and
//! re-reads are absorbed.
//!
//! `index.json` per workspace is UI bookkeeping (titles, counts) and is not a
//! session. No quota here: the GLM-plan points balance lives in the FunIDE
//! cloud account, and no local endpoint exposes it (module notes in [`proj`]).
//!
//! The public surface (`TOOL_ID`, `FunIdeAdapter`) follows the frozen adapter
//! contract: `usage-adapter-all` links it.

use usage_core::{DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile};

mod paths;
mod proj;

pub const TOOL_ID: &str = "funide";
pub(crate) const DEDUPE_PREFIX: &str = "funide#";

#[derive(Debug, Default, Clone, Copy)]
pub struct FunIdeAdapter;

impl SourceAdapter for FunIdeAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "FunIDE"
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
        let files = paths::session_files();
        let root = paths::sessions_dir()?;
        (files.len() > 0).then(|| DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: vec![root],
            hint: Some(format!("{} sessions", files.len())),
        })
    }

    fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
        let mut out = Vec::new();
        for path in paths::session_files() {
            let Some((size, mtime_ms)) = paths::stat_file(&path) else { continue };
            out.push(SourceFile { path, kind: FileKind::Tree, size, mtime_ms });
        }
        out
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        match file.kind {
            FileKind::Tree => {
                let cursor = ReadCursor(file.size);
                let Ok(text) = std::fs::read_to_string(&file.path) else {
                    return Ok(ReadOutcome { events: Vec::new(), cursor });
                };
                Ok(ReadOutcome { events: proj::parse(&text, file.mtime_ms, &file.key()), cursor })
            }
            _ => Ok(ReadOutcome { events: Vec::new(), cursor }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::DateFilter;

    const SESSION: &str = r#"{"version":2,"id":"s1","workspaceKey":"d:\\w\\proj",
        "activeModelId":"cloud:GLM-plan-flash","updatedAt":1790685147111,
        "cacheUsage":{"promptTokens":220,"cachedTokens":60,"completionTokens":30},
        "items":[{"role":"user","turnId":"turn_1790510647992_1","text":"hi"},
                 {"role":"assistant","turnId":"turn_1790510647992_1",
                  "usagePrompt":200,"usageCached":50,"usageCompletion":20},
                 {"role":"user","turnId":"turn_1790520647992_2","text":"go on"},
                 {"role":"assistant","turnId":"turn_1790520647992_2",
                  "usagePrompt":20,"usageCached":10,"usageCompletion":10}]}
    "#;

    fn fixture(dir: &std::path::Path) -> std::path::PathBuf {
        let ws = dir.join("sessions").join("ws1");
        std::fs::create_dir_all(&ws).unwrap();
        let path = ws.join("s1.json");
        std::fs::write(&path, SESSION).unwrap();
        let index = ws.join("index.json");
        std::fs::write(index, b"{}").unwrap();
        path
    }

    #[test]
    fn a_session_file_reads_as_one_event_per_turn() {
        let _guard = paths::test_lock();
        let dir = tempfile::tempdir().unwrap();
        let _path = fixture(dir.path());
        std::env::set_var(paths::HOME_ENV, dir.path());
        let detected = FunIdeAdapter.probe().expect("the fixture session exists");
        assert_eq!(detected.roots.len(), 1);
        let files = FunIdeAdapter.discover(&DateFilter::default());
        assert_eq!(files.len(), 1, "index.json is not discovered");
        let outcome = FunIdeAdapter.read(&files[0], ReadCursor::default()).unwrap();
        assert_eq!(outcome.events.len(), 2, "two billed turns, not one session total");
        assert_eq!(
            FunIdeAdapter.semantics().usage_form,
            usage_core::UsageForm::PerCall,
            "the day an event belongs to is the day its turn ran"
        );
        let (input, cache, output): (f64, f64, f64) = outcome.events.iter().fold(
            (0.0, 0.0, 0.0),
            |mut acc, e| {
                acc.0 += e.counts.input;
                acc.1 += e.counts.cache_read;
                acc.2 += e.counts.output;
                acc
            },
        );
        assert_eq!((input, cache, output), (160.0, 60.0, 30.0), "the turns make the ledger");
        assert_eq!(outcome.events[0].ts_ms, 1790510647992);
        assert_eq!(outcome.events[1].ts_ms, 1790520647992);
        assert_eq!(outcome.events[0].dedupe_key.as_deref(), Some("funide#s1#turn_1790510647992_1"));
        assert_eq!(outcome.events[1].dedupe_key.as_deref(), Some("funide#s1#turn_1790520647992_2"));
        std::env::remove_var(paths::HOME_ENV);
    }
}

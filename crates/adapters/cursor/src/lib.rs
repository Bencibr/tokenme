//! Cursor usage adapter.
//!
//! Cursor does not write a per-request token ledger. The counts that match
//! the dashboard live on the account, so this adapter copies them into a
//! local JSONL (`cursor-usage.jsonl`) and the index reads that file. The
//! shared weekly quota stays on the Grok Build probe — this crate is the
//! usage rows (today, tools, detail), not a second quota bar.
//!
//! Only token counts, model, and time are stored. The sign-in database is
//! opened read-only.

mod ledger;
mod parser;

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile,
};

pub const TOOL_ID: &str = "cursor";

#[derive(Debug, Default, Clone, Copy)]
pub struct CursorAdapter;

impl SourceAdapter for CursorAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "Cursor"
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
        let ledger = ledger::ledger_path()?;
        let has_rows = std::fs::metadata(&ledger).map(|m| m.len() > 0).unwrap_or(false);
        if !ledger::signed_in() && !has_rows {
            return None;
        }
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: ledger::roots(),
            hint: Some("account usage events".into()),
        })
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        ledger::pull();
        self.list(filter)
    }

    fn discover_cached(&self, filter: &DateFilter, snapshot: Option<Vec<SourceFile>>) -> Vec<SourceFile> {
        // A restat would otherwise skip `discover`, and the ledger would never
        // grow between snapshot rebuilds. The pull itself is throttled.
        ledger::pull();
        match snapshot {
            None => self.list(filter),
            Some(files) => {
                let refreshed: Vec<SourceFile> = files.into_iter().filter_map(SourceFile::restat).collect();
                if refreshed.is_empty() { self.list(filter) } else { refreshed }
            }
        }
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        Ok(ledger::read_ledger(file, cursor))
    }
}

impl CursorAdapter {
    fn list(&self, _filter: &DateFilter) -> Vec<SourceFile> {
        ledger::ledger_path().and_then(|path| ledger::source_file(&path)).into_iter().collect()
    }
}

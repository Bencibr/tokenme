//! Codex adapter (CLI + desktop app).
//!
//! Source: `~/.codex/sessions/**/rollout-*.jsonl`, honouring `CODEX_HOME`.
//! `event_msg.token_count` carries `info.last_token_usage` (use that, never the
//! cumulative `total_token_usage`) and `rate_limits`.
//!
//! Owned by the `codex-opencode-adapters` workstream; public surface is frozen.

mod loader;
mod parser;
mod paths;

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter, SourceFile,
};

pub const TOOL_ID: &str = "codex";

#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAdapter;

impl SourceAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            usage_form: usage_core::UsageForm::PerCall,
            meter: usage_core::Meter::Tokens,
            // The model lives on `turn_context`, a different record than the
            // token counts, and must be resolved inside the adapter.
            model_attr: usage_core::ModelAttr::Joined,
            dedupes_by_id: false,
            reports_quota: true,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let home = paths::codex_home()?;
        let root = paths::sessions_root(&home);
        let first = paths::first_rollout(&root)?;
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: vec![root],
            hint: paths::cli_version_hint(&first).map(|v| format!("cli {v}")),
        })
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        let Some(home) = paths::codex_home() else { return Vec::new() };
        let root = paths::sessions_root(&home);
        if root.is_dir() {
            paths::list_rollouts(&root, filter)
        } else {
            Vec::new()
        }
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        // Never `Err`: a log Codex is mid-rotation on must not abort the pass, and
        // there is nothing a caller could do about it.
        Ok(loader::read_rollout(&file.path, cursor, &file.key()))
    }
}

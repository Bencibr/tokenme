use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Error, Meter, ModelAttr, UsageEvent, UsageForm};

/// How a source stores records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// Append-only JSON lines; cursor is a byte offset.
    Jsonl,
    /// SQLite; cursor is the highest consumed `rowid`.
    Sqlite,
    /// One record per file (JSON/TOML/…); cursor is unused and the file is
    /// re-read whole whenever its size or mtime changes.
    Tree,
}

/// A candidate file handed from `discover` to `read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: i64,
}

impl SourceFile {
    pub fn key(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    /// Cheap change test used by the indexer before calling `read`.
    pub fn unchanged_since(&self, size: u64, mtime_ms: i64) -> bool {
        self.size == size && self.mtime_ms == mtime_ms
    }
}

/// Position within a source file. Meaning depends on [`FileKind`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReadCursor(pub u64);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateFilter {
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
}

impl DateFilter {
    pub fn new(since_ms: Option<i64>, until_ms: Option<i64>) -> Self {
        Self { since_ms, until_ms }
    }

    pub fn within(&self, ts_ms: i64) -> bool {
        self.since_ms.is_none_or(|s| ts_ms >= s) && self.until_ms.is_none_or(|u| ts_ms <= u)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadOutcome {
    pub events: Vec<UsageEvent>,
    /// Advance this file's cursor here only after the events were accepted.
    pub cursor: ReadCursor,
}

/// Result of probing the machine for an installed, readable source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedSource {
    pub id: String,
    pub display: String,
    pub roots: Vec<PathBuf>,
    /// Version or account hint shown in the sources list.
    pub hint: Option<String>,
}

/// The per-source quirks the indexer and the money math must respect. Every
/// field exists because at least one real source violates the naive default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Semantics {
    pub usage_form: UsageForm,
    pub meter: Meter,
    pub model_attr: ModelAttr,
    /// Whether records carry a stable id usable for dedupe.
    pub dedupes_by_id: bool,
    /// Whether the source reports its own quota window (Codex `rate_limits`).
    pub reports_quota: bool,
}

impl Semantics {
    pub const TOKENS_PER_CALL_INLINE: Self = Self {
        usage_form: UsageForm::PerCall,
        meter: Meter::Tokens,
        model_attr: ModelAttr::Inline,
        dedupes_by_id: true,
        reports_quota: false,
    };
}

/// One implementor per AI tool. Adapters own discovery, parsing, token mapping,
/// model mapping and source-specific pricing quirks; everything shared lives in
/// `usage-core`.
pub trait SourceAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn semantics(&self) -> Semantics;

    /// Must short-circuit on the first usable file: this runs at startup to
    /// decide which sources to watch.
    fn probe(&self) -> Option<DetectedSource>;

    /// Enumerate files to ingest. Implementations must honour env-var overrides
    /// (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, …) before falling back to defaults.
    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile>;

    /// Read records appended after `cursor`. Returning an error for one file
    /// must not abort the rest of the ingest pass.
    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error>;

    /// Live quota state that the logs do not carry. Codex embeds its rate window
    /// in every `token_count` record and so leaves this empty; tools that expose
    /// a quota only through a local server or a cached auth file answer here, and
    /// the caller polls it on its own cadence rather than during `read`.
    fn quota(&self) -> Vec<crate::QuotaSample> {
        Vec::new()
    }
}

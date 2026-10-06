//! Shared contract for every tokenme binary (menu-bar app + CLI).
//!
//! Adapters depend on this crate and nothing else, so the two frontends can
//! never drift apart on semantics or money math.

pub mod adapter;
pub mod budget;
pub mod icons;
pub mod pricing;
pub mod report;
pub mod types;

pub use budget::Budget;
pub use adapter::{
    DateFilter, DetectedSource, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};
pub use pricing::{Price, PricingMap, PricingMeta, PricingSource};
pub use report::{
    local_day_of, poll_quota, summarize, summarize_facts, AggregatePlan, Breakdown, CallFact,
    FactGroup, HeatCell, Item, QuotaFact, QuotaOrigin, QuotaView, Report, ReportFacts,
    ReportOptions, RollupRow, SessionFact, SessionGroup, SessionRow, SourceStatus, Summary,
    SyncRecord, UnpricedModel, Window, LIVE_HOUR0, LIVE_PREV, LIVE_TODAY,
};
pub use types::{
    origin_of, origin_ok, parse_ts_ms, Call, CallKind, Meter, ModelAttr, QuotaSample, TokenCounts,
    UsageEvent, UsageForm,
};

use std::path::{Path, PathBuf};

/// Replace `path` with a completed sibling file.
///
/// Unix `rename` replaces an existing destination, while Windows' standard
/// library rename refuses one. Keep the operation in one shared helper so all
/// JSON caches have the same overwrite semantics on every platform.
pub fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(tmp, path)
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };

        let tmp: Vec<u16> = tmp.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        if unsafe {
            MoveFileExW(
                tmp.as_ptr(),
                path.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("unsupported cursor {cursor:?} for file {path:?}")]
    Cursor { path: PathBuf, cursor: u64 },
    #[error("pricing: {0}")]
    Pricing(String),
    #[error("adapter {id}: {message}")]
    Adapter { id: String, message: String },
    #[error("sync: {0}")]
    Sync(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }
    pub fn adapter(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Adapter { id: id.into(), message: message.into() }
    }
    /// Whether the error says the database *file* no longer parses (damaged
    /// pages, "not a database", I/O failures that a torn WAL or a bad header
    /// also produce), as opposed to a statement that merely failed. Only a
    /// rebuild from the source logs answers from such a file, so callers use
    /// this to decide between reporting and quarantining.
    pub fn is_corruption(&self) -> bool {
        match self {
            Error::Sqlite(message) => {
                let message = message.to_lowercase();
                message.contains("malformed")
                    || message.contains("not a database")
                    || message.contains("disk i/o")
            }
            _ => false,
        }
    }
}

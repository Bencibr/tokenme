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
    poll_quota, summarize, Breakdown, HeatCell, Item, QuotaOrigin, QuotaView, Report,
    ReportOptions, SessionRow, SourceStatus, Summary, UnpricedModel, Window,
};
pub use types::{
    parse_ts_ms, Call, CallKind, Meter, ModelAttr, QuotaSample, TokenCounts, UsageEvent, UsageForm,
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
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }
    pub fn adapter(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Adapter { id: id.into(), message: message.into() }
    }
}

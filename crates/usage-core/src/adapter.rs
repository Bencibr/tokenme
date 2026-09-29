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

    /// Fold the `-wal` sidecar's mtime into a SQLite source's activity signal.
    ///
    /// A WAL-mode database writes rows into `<db>-wal` and leaves the main file's
    /// own mtime frozen until SQLite checkpoints, so change detection keyed on the
    /// main file alone keeps a fresh conversation invisible for as long as the
    /// vendor's checkpoint interval — measured here at 5 minutes on OpenCode's
    /// 1.6 GB store (`opencode.db` 16:21:00 vs `opencode.db-wal` 16:23:15) and two
    /// *days* on crow5, whose main file had not moved since 2026-08-27 while its
    /// WAL carried rows through 2026-09-24.
    ///
    /// Only the mtime moves: the size stays the main file's, because the WAL
    /// shrinks on every checkpoint and a shrinking stat is what the ingest reads
    /// as "this log was rewritten — purge and start over". Call it on a freshly
    /// statted [`SourceFile`]; a source already in the manifest gets re-read on the
    /// next pass, which is the point.
    pub fn with_wal_activity(mut self) -> Self {
        if self.kind != FileKind::Sqlite {
            return self;
        }
        let Some(wal_mtime) = wal_mtime_ms(&self.path) else { return self };
        self.mtime_ms = self.mtime_ms.max(wal_mtime);
        self
    }
}

/// The `-wal` sidecar's mtime, or `None` when there is no sidecar to fold in.
///
/// The suffix is the one SQLite itself appends (`<db>-wal`), and a missing or
/// unreadable sidecar means the database is in another journal mode — not an error.
pub fn wal_mtime_ms(db: &std::path::Path) -> Option<i64> {
    let mut name = db.file_name()?.to_os_string();
    name.push("-wal");
    std::fs::metadata(db.with_file_name(name))
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_millis() as i64)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn source(path: PathBuf, kind: FileKind, size: u64, mtime_ms: i64) -> SourceFile {
        SourceFile { path, kind, size, mtime_ms }
    }

    fn touch(path: &Path, secs: u64) {
        std::fs::write(path, b"frame").unwrap();
        let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let file = std::fs::File::open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(when)).unwrap();
    }

    #[test]
    fn an_uncheckpointed_wal_write_is_activity_even_while_the_main_file_sleeps() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.db");
        touch(&db, 1_780_000_000);
        let folded = source(db.clone(), FileKind::Sqlite, 4, 1_780_000_000_000).with_wal_activity();
        assert_eq!(folded.mtime_ms, 1_780_000_000_000, "no sidecar: the main file's own stat stands");

        // The app writes rows into the sidecar and does not touch the main file.
        touch(&dir.path().join("store.db-wal"), 1_780_000_600);
        let folded = source(db, FileKind::Sqlite, 4, 1_780_000_000_000).with_wal_activity();
        assert_eq!(folded.mtime_ms, 1_780_000_600_000, "the newer wal mtime is the activity signal");
        assert_eq!(folded.size, 4, "the size stays the main file's, or a checkpoint would read as a rewrite");
    }

    #[test]
    fn a_log_source_is_left_alone_because_its_own_file_is_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("rollout.jsonl");
        touch(&log, 1_780_000_000);
        touch(&dir.path().join("rollout.jsonl-wal"), 1_780_000_600);
        let folded = source(log, FileKind::Jsonl, 4, 1_780_000_000_000).with_wal_activity();
        assert_eq!(folded.mtime_ms, 1_780_000_000_000);
    }
}

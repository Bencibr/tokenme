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

    /// Fresh stat of the same path for snapshot reuse; `None` when the file vanished.
    /// Size/mtime are re-read now — a snapshot's stale numbers must never reach
    /// `unchanged_since`, or an appended log would be skipped as unchanged.
    /// SQLite sources fold the `-wal` mtime exactly like a fresh discover would.
    pub fn restat(self) -> Option<SourceFile> {
        // Follows symlinks, like the directory walk that built the snapshot.
        let meta = std::fs::metadata(&self.path).ok()?;
        let mtime_ms = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as i64)
            // A pre-epoch mtime is nonsense but real: 0 reads as "moved
            // backwards", which re-reads the file instead of dropping it.
            .unwrap_or(0);
        Some(SourceFile { path: self.path, kind: self.kind, size: meta.len(), mtime_ms }
            .with_wal_activity())
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

    /// Discover, or refresh a previous snapshot without re-walking the tree.
    /// `None` = full discovery (delegates to `discover`); `Some(snapshot)` = re-stat
    /// each file (dropping vanished ones) and return the refreshed list. The engine
    /// decides WHEN to snapshot (root scoping + TTL); adapters only need correct
    /// fresh stats, which [`SourceFile::restat`] guarantees.
    fn discover_cached(&self, filter: &DateFilter, snapshot: Option<Vec<SourceFile>>) -> Vec<SourceFile> {
        match snapshot {
            None => self.discover(filter),
            Some(files) => files.into_iter().filter_map(SourceFile::restat).collect(),
        }
    }

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
        // Windows gates SetFileTime behind FILE_WRITE_ATTRIBUTES: a read-only
        // handle answers PermissionDenied there, while Unix futimens needs none.
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
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

    #[test]
    fn restat_of_an_untouched_file_matches_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("rollout.jsonl");
        touch(&log, 1_780_000_000);
        let snapshot = source(log.clone(), FileKind::Jsonl, 5, 1_780_000_000_000);
        assert_eq!(snapshot.restat(), Some(source(log, FileKind::Jsonl, 5, 1_780_000_000_000)));
    }

    #[test]
    fn restat_sees_an_append_that_happened_after_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("rollout.jsonl");
        touch(&log, 1_780_000_000);
        let snapshot = source(log.clone(), FileKind::Jsonl, 5, 1_780_000_000_000);
        std::fs::write(&log, b"frame appended").unwrap();
        let fresh = snapshot.restat().unwrap();
        assert_eq!(fresh.size, 14, "the fresh size is what feeds unchanged_since");
        assert!(
            fresh.mtime_ms > 1_780_000_000_000,
            "the fresh mtime moved with the append, not the snapshot's stale one"
        );
    }

    #[test]
    fn restat_of_a_sqlite_source_folds_a_newer_wal_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.db");
        touch(&db, 1_780_000_000);
        let snapshot = source(db, FileKind::Sqlite, 4, 1_780_000_000_000);
        // The app writes rows into the sidecar; the main file never moves.
        touch(&dir.path().join("store.db-wal"), 1_780_000_600);
        let fresh = snapshot.restat().unwrap();
        assert_eq!(fresh.mtime_ms, 1_780_000_600_000, "wal activity folds in like a fresh discover");
        // The size is the main file's own, freshly read (the 5 bytes `touch`
        // wrote) — never the snapshot's stale 4, never the sidecar's.
        assert_eq!(fresh.size, 5);
    }

    #[test]
    fn a_vanished_file_drops_out_of_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("gone.jsonl");
        touch(&log, 1_780_000_000);
        let snapshot = source(log, FileKind::Jsonl, 5, 1_780_000_000_000);
        std::fs::remove_file(dir.path().join("gone.jsonl")).unwrap();
        assert_eq!(snapshot.restat(), None, "same semantics as disappearing from a fresh discover");
    }

    #[test]
    fn discover_cached_refreshes_a_snapshot_without_rewalking() {
        // `discover` panics to prove a snapshot refresh never reaches the walk.
        struct One;
        impl SourceAdapter for One {
            fn id(&self) -> &'static str {
                "one"
            }
            fn display_name(&self) -> &'static str {
                "One"
            }
            fn semantics(&self) -> Semantics {
                Semantics::TOKENS_PER_CALL_INLINE
            }
            fn probe(&self) -> Option<DetectedSource> {
                None
            }
            fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
                panic!("a snapshot refresh must not re-walk the tree");
            }
            fn read(
                &self,
                _file: &SourceFile,
                cursor: ReadCursor,
            ) -> Result<ReadOutcome, Error> {
                Ok(ReadOutcome { events: Vec::new(), cursor })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("rollout.jsonl");
        touch(&log, 1_780_000_000);
        let snapshot = vec![source(log, FileKind::Jsonl, 999, 1_780_000_000_000)];
        let refreshed = One.discover_cached(&DateFilter::default(), Some(snapshot));
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].size, 5, "the stale snapshot numbers were restatted fresh");
    }
}

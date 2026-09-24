//! Antigravity CLI adapter.
//!
//! Source: `~/.gemini/antigravity-cli/conversations/<uuid>.db` (SQLite, one
//! conversation per file), honouring `ANTIGRAVITY_DATA_DIR`. Each
//! `gen_metadata` row is a `GeneratorMetadata` protobuf — the shipped CLI is a
//! closed-source Go binary and publishes no `.proto`, so [`wire`] decodes the
//! handful of fields this mapping needs by hand: the usage message at `#1.#4`,
//! whose `#2` is uncached input, `#3` total output, `#5` cache read and `#11` the
//! responseId used as dedupe key, plus the model id at `#1.#19`.
//!
//! `conversation_summaries.db` beside these files is deliberately not read: its
//! 21 columns are all title/step-count metadata and carry no token data.

mod loader;
mod parser;
mod paths;
mod wire;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use usage_core::{
    DateFilter, DetectedSource, Error, FileKind, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

pub const TOOL_ID: &str = "antigravity";

pub use loader::{Audit, SkippedRow};

/// The Antigravity CLI source. Stateless: everything it knows, it re-derives
/// from the conversation databases on disk.
#[derive(Debug, Default, Clone, Copy)]
pub struct AntigravityAdapter;

impl AntigravityAdapter {
    /// Reads every discovered database and reports what the wire actually said,
    /// row by row. The ignored smoke test runs this; `read` fills the same
    /// counters, so the audit cannot drift from the ingest path.
    pub fn audit(&self, files: &[SourceFile]) -> Audit {
        let mut audit = Audit::default();
        for file in files {
            let _ = loader::read_db(file, &mut audit);
        }
        audit
    }
}

impl SourceAdapter for AntigravityAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        "Antigravity"
    }

    fn semantics(&self) -> Semantics {
        // Per-call rows carrying their own model id, deduped by responseId: the
        // shape this source matches exactly. No quota window exists in the blobs.
        Semantics::TOKENS_PER_CALL_INLINE
    }

    fn probe(&self) -> Option<DetectedSource> {
        let roots = paths::roots();
        // One open that succeeds and whose `gen_metadata` answers a query is the
        // whole test, so this stops at the first candidate instead of walking the
        // directory: at startup it must cost one file, not a hundred. A root with
        // no readable database is no source at all, so it reports nothing. The
        // hint counts generations in *that* database, which is all a short-circuit
        // probe can know without opening the rest.
        for db in conversation_dbs(&roots) {
            let Some(db) = paths::open(&db) else { continue };
            let rows = db
                .conn()
                .query_row(paths::COUNT_SQL, [], |row| row.get::<_, i64>(0))
                .unwrap_or(0);
            return Some(DetectedSource {
                id: TOOL_ID.to_string(),
                display: self.display_name().to_string(),
                roots,
                hint: (rows > 0).then(|| format!("{rows} generations")),
            });
        }
        None
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        conversation_dbs(&paths::roots())
            .into_iter()
            .filter_map(|path| {
                let (size, mtime_ms) = paths::stat_file(&path)?;
                // One conversation per file, so the last write to it bounds every
                // row inside: a file older than the retention window is pruned
                // here rather than opened for nothing.
                if filter.since_ms.is_some_and(|since| mtime_ms < since) {
                    return None;
                }
                Some(SourceFile { path, kind: FileKind::Tree, size, mtime_ms })
            })
            .collect()
    }

    fn read(&self, file: &SourceFile, _cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        // Deliberately infallible: a database the CLI has locked, or one that
        // vanished between `discover` and here, must not abort the ingest pass.
        // A `Tree` file ignores the cursor and is re-read whole; `dedupe_key` on
        // every event is what makes that idempotent.
        let mut audit = Audit::default();
        Ok(loader::read_db(file, &mut audit))
    }
}

/// Every `*.db` under each root's conversation directory except the summary
/// index, canonicalised so a root listed twice cannot bill one conversation
/// twice, in path order so a run is reproducible. WAL sidecars carry no
/// database of their own and never appear.
fn conversation_dbs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for dir in roots.iter().map(|root| paths::conversation_dir(root)) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut names: Vec<PathBuf> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let listed = path.extension().is_some_and(|ext| ext == "db")
                    && path.file_name().and_then(|n| n.to_str()) != Some(paths::SUMMARIES_DB)
                    && paths::stat_file(&path).is_some();
                listed.then_some(path)
            })
            .collect();
        names.sort();
        for path in names {
            let key = canonical(&path);
            if seen.insert(key) {
                out.push(path);
            }
        }
    }
    out
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantics_match_what_the_blobs_actually_carry() {
        let a = AntigravityAdapter;
        assert_eq!(a.id(), "antigravity");
        assert_eq!(a.display_name(), "Antigravity");
        let s = a.semantics();
        assert_eq!(s, Semantics::TOKENS_PER_CALL_INLINE);
        assert!(!s.reports_quota, "no quota window exists in these records");
        assert!(s.dedupes_by_id, "every billable row carries a responseId");
    }

    #[test]
    fn a_root_that_is_already_a_directory_of_databases_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("dbs");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("a.db"), b"x").unwrap();
        std::fs::write(bare.join("a.db-wal"), b"x").unwrap();
        let listed = conversation_dbs(std::slice::from_ref(&bare));
        assert_eq!(listed.len(), 1, "the -wal sidecar is not a source");
        assert!(listed[0].ends_with("a.db"));
        // Same directory listed twice still yields one conversation.
        assert_eq!(conversation_dbs(std::slice::from_ref(&bare)).len(), 1, "a root listed twice bills one conversation");
        assert!(conversation_dbs(&[dir.path().join("absent")]).is_empty());
    }

    #[test]
    fn an_absent_source_reads_as_nothing_rather_than_failing_the_pass() {
        let a = AntigravityAdapter;
        let file = SourceFile {
            path: PathBuf::from("/nonexistent/antigravity/conversations/x.db"),
            kind: FileKind::Tree,
            size: 0,
            mtime_ms: 0,
        };
        let out = a.read(&file, ReadCursor(0)).expect("read never fails on an absent db");
        assert_eq!(out, ReadOutcome::default());
        assert!(a.discover(&DateFilter::default()).iter().all(|f| f.kind == FileKind::Tree));
    }
}

//! AtomCode adapter.
//!
//! Source: `$ATOMCODE_HOME/sessions/<project_hash>/<id>.jsonl` (default
//! `~/.atomcode/sessions/…`), AtomCode's own append-only per-turn transcript.
//! Each line is one `TurnRecord` with a **top-level**
//! `usage = {prompt, completion, cached}` — not nested under `message` the way
//! Claude's is — where `prompt` is the inclusive prompt total and `cached` the
//! cache-read subset of it. See [`parser`]'s module doc for the measured numbers
//! and the MIT-source lines that settle each convention.
//!
//! Owned by the atomcode-adapter workstream.

use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

use usage_core::{
    DateFilter, DetectedSource, Error, FileKind, Meter, ModelAttr, ReadCursor, ReadOutcome,
    Semantics, SourceAdapter, SourceFile, UsageForm,
};

mod parser;
mod paths;

pub const TOOL_ID: &str = "atomcode";
pub(crate) const DISPLAY_NAME: &str = "AtomCode";

#[derive(Debug, Default, Clone, Copy)]
pub struct AtomCodeAdapter;

impl SourceAdapter for AtomCodeAdapter {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn semantics(&self) -> Semantics {
        // PerCall, proven by the writer and by the logs. `transcript.rs:1-8`
        // flushes "ONE raw record per completed turn" and `transcript.rs:231-255`
        // fills it with that turn's own figures; the records themselves are not
        // monotonic (`6ade0189…` runs prompt 8256 -> 8988 -> 8981 -> 13845, and 2
        // of the 3 sessions here with three or more turns show a decrease), so
        // they cannot be a running session total and there is nothing to diff.
        // Summing the 64 records as they stand is what yields the measured
        // 7 088 400 prompt / 372 734 completion.
        Semantics {
            usage_form: UsageForm::PerCall,
            meter: Meter::Tokens,
            // No `model` key on any of the 64 records, and none on `TurnRecord`
            // at all: the model lives on the sibling snapshot's per-message meta
            // and in the config that was active at flush time, neither of which
            // this transcript line can be attributed to without inventing spend.
            model_attr: ModelAttr::Unknown,
            // `session_id` (64/64) + `turn_id` + `ts` give every record a stable
            // identity; `turn_id` alone does not, so it is not the key.
            dedupes_by_id: true,
            // AtomCode reports no quota window; `/cost` and `/usage` recompute
            // from these same transcripts rather than from a rate-limit header.
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let root = paths::sessions_dir()?;
        // One readable transcript is the whole of the evidence we need: `probe`
        // runs at startup and must not walk the session tree.
        let first = first_log(&root)?;
        Some(DetectedSource {
            id: TOOL_ID.to_string(),
            display: self.display_name().to_string(),
            roots: vec![root],
            hint: parser::head_version(&first),
        })
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        let Some(root) = paths::sessions_dir() else {
            return Vec::new();
        };
        discover_in(&root, filter)
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        parser::read_file(file, cursor)
    }
}

fn discover_in(root: &Path, filter: &DateFilter) -> Vec<SourceFile> {
    let mut files: Vec<SourceFile> = log_entries(root)
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some(SourceFile {
                path: entry.into_path(),
                // Append-only journal, byte-offset cursor: see `parser`'s module
                // doc for the transcript.rs lines and the 0-1 ms mtime evidence.
                kind: FileKind::Jsonl,
                size: meta.len(),
                mtime_ms: paths::mtime_ms(&meta)?,
            })
        })
        // Sessions are filed per project hash, not per date, so a file's mtime is
        // the only time signal available before it is parsed.
        .filter(|file| filter.since_ms.is_none_or(|since| file.mtime_ms >= since))
        .collect();
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Every `*.jsonl` under the sessions root, and nothing else: the `<id>.meta`,
/// `<id>.snapshot`, `<id>.ui.json`, `<id>.lease` and `*.lock` siblings share the
/// directory and would either double-count a turn or carry no usage at all.
fn log_entries(root: &Path) -> impl Iterator<Item = DirEntry> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && is_jsonl(entry.path()))
}

fn is_jsonl(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("jsonl"))
}

/// Stops at the first non-empty, openable transcript.
fn first_log(root: &Path) -> Option<PathBuf> {
    log_entries(root)
        .find(|entry| readable(entry.path()))
        .map(DirEntry::into_path)
}

fn readable(path: &Path) -> bool {
    File::open(path)
        .and_then(|handle| handle.metadata())
        .map(|meta| meta.len() > 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantics_are_the_contract_the_indexer_keys_on() {
        assert_eq!(
            AtomCodeAdapter.semantics(),
            Semantics {
                usage_form: UsageForm::PerCall,
                meter: Meter::Tokens,
                model_attr: ModelAttr::Unknown,
                dedupes_by_id: true,
                reports_quota: false,
            }
        );
        // Deliberately NOT `Semantics::TOKENS_PER_CALL_INLINE`: that constant's
        // `ModelAttr::Inline` would promise a model per record, and there is none.
        assert_ne!(Semantics::TOKENS_PER_CALL_INLINE, AtomCodeAdapter.semantics());
        assert_eq!(AtomCodeAdapter.id(), TOOL_ID);
        assert_eq!(AtomCodeAdapter.id(), "atomcode");
        assert_eq!(AtomCodeAdapter.display_name(), "AtomCode");
    }

    #[test]
    fn an_empty_or_absent_tree_is_not_an_install() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        assert_eq!(discover_in(&sessions, &DateFilter::default()).len(), 0);
        assert!(first_log(&sessions).is_none());
        let project = sessions.join("45d727130d2f41d9");
        std::fs::create_dir_all(&project).unwrap();
        // A zero-byte transcript proves nothing about a working install, so
        // `probe` skips it, but `discover` still lists it: the indexer decides
        // what an empty file means instead of the adapter hiding it.
        std::fs::write(project.join("242f9f29-b726.jsonl"), "").unwrap();
        assert!(first_log(&sessions).is_none());
        assert_eq!(discover_in(&sessions, &DateFilter::default()).len(), 1);
    }

    #[test]
    fn only_the_transcripts_are_discovered() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("45d727130d2f41d9");
        std::fs::create_dir_all(&project).unwrap();
        let record = "{\"v\":1,\"ts\":1784361677893,\"session_id\":\"a\",\"turn_id\":1,\"usage\":{\"prompt\":1,\"completion\":1,\"cached\":0}}\n";
        std::fs::write(project.join("a.jsonl"), record).unwrap();
        // Every one of these is a real sibling AtomCode writes beside a session.
        std::fs::write(project.join("a.meta"), "{\"working_dir\":\"/x\"}").unwrap();
        std::fs::write(project.join("a.snapshot"), "{\"version\":1}").unwrap();
        std::fs::write(project.join("a.ui.json"), "{\"v\":1,\"entries\":[]}").unwrap();
        std::fs::write(project.join("a.lease"), "").unwrap();
        std::fs::write(project.join("a.meta.lock"), "").unwrap();
        std::fs::write(project.join("a.rewind.json"), "{}").unwrap();

        let found = discover_in(dir.path(), &DateFilter::default());
        assert_eq!(found.len(), 1, "found {found:?}");
        assert_eq!(found[0].path, project.join("a.jsonl"));
        assert_eq!(found[0].kind, FileKind::Jsonl);
        assert!(found[0].size > 0 && found[0].mtime_ms > 0, "stat once, both fields filled");
    }
}

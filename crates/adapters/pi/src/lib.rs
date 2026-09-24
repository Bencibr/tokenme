//! Pi adapter, and the sibling that writes the same transcripts.
//!
//! Source: `~/.pi/agent/sessions/<mangled-cwd>/<ISO>_<uuid>.jsonl`, Pi's own
//! append-only transcript of every session. `type:"message"` records carry
//! `message.usage = {input, output, cacheRead, cacheWrite, cacheWrite1h,
//! reasoning?, totalTokens, cost{...}}`.
//!
//! Cola keeps its coding transcripts in `~/.cola/sessions/<name>/<ISO>_<uuid>.jsonl`
//! in exactly that shape, so it is registered here as a sibling that shares this
//! record parser rather than being given a copy of it (see [`paths::COLA`] for
//! what differs, and why its env override is tokenme's own).
//!
//! The public surface (`TOOL_ID`, `PiAdapter`, this trait impl) is frozen:
//! `usage-adapter-all` and the CLI already link against it.

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

pub const TOOL_ID: &str = "pi";

#[derive(Debug, Default, Clone, Copy)]
pub struct PiAdapter;

/// Cola's transcripts are Pi records; only [`paths::COLA`] differs.
#[derive(Debug, Default, Clone, Copy)]
pub struct ColaAdapter;

/// The one implementation of the transcript dialect, parameterised by product
/// coordinates.
#[derive(Debug, Clone, Copy)]
struct ProductAdapter(&'static paths::Product);

impl SourceAdapter for ProductAdapter {
    fn id(&self) -> &'static str {
        self.0.id
    }

    fn display_name(&self) -> &'static str {
        self.0.display
    }

    fn semantics(&self) -> Semantics {
        // Per-call, proven by the logs themselves: successive assistant turns in
        // one session swing up and down (`input` goes 19218 -> 5558 -> 32789 in
        // 2026-08-24T13-33-59-540Z_01a033fa….jsonl, and 74 of the 83 files with
        // five or more turns show a decrease), and every record's `totalTokens`
        // equals that record's own input+output+cacheRead+cacheWrite. There is
        // nothing to diff, so the totals are summed as they stand. Cola's 65
        // usage records here obey the same identity, which is why it inherits
        // this mapping rather than declaring its own.
        Semantics {
            usage_form: UsageForm::PerCall,
            meter: Meter::Tokens,
            // `message.model` rides on the same record as the usage.
            model_attr: ModelAttr::Inline,
            // `message` records carry a session-unique `id` (no duplicates across
            // all 117 files), prefixed with the session id into `dedupe_key`.
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let root = paths::sessions_dir_for(self.0)?;
        // One readable log is the whole of the evidence we need: `probe` runs at
        // startup and must not walk 95 MB of sessions.
        let first = first_log(&root)?;
        Some(DetectedSource {
            id: self.0.id.to_string(),
            display: self.display_name().to_string(),
            roots: vec![root],
            hint: parser::head_version(&first),
        })
    }

    fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
        let Some(root) = paths::sessions_dir_for(self.0) else {
            return Vec::new();
        };
        discover_in(&root, filter)
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        parser::read_file(file, cursor, self.0)
    }
}

/// Each public adapter is the shared implementation plus its coordinates.
macro_rules! transcript_adapter {
    ($adapter:ty => $product:path) => {
        impl SourceAdapter for $adapter {
            fn id(&self) -> &'static str {
                ProductAdapter(&$product).id()
            }

            fn display_name(&self) -> &'static str {
                ProductAdapter(&$product).display_name()
            }

            fn semantics(&self) -> Semantics {
                ProductAdapter(&$product).semantics()
            }

            fn probe(&self) -> Option<DetectedSource> {
                ProductAdapter(&$product).probe()
            }

            fn discover(&self, filter: &DateFilter) -> Vec<SourceFile> {
                ProductAdapter(&$product).discover(filter)
            }

            fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
                ProductAdapter(&$product).read(file, cursor)
            }
        }
    };
}

transcript_adapter!(PiAdapter => paths::PI);
transcript_adapter!(ColaAdapter => paths::COLA);

fn discover_in(root: &Path, filter: &DateFilter) -> Vec<SourceFile> {
    let mut files: Vec<SourceFile> = log_entries(root)
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some(SourceFile {
                path: entry.into_path(),
                kind: FileKind::Jsonl,
                size: meta.len(),
                mtime_ms: paths::mtime_ms(&meta)?,
            })
        })
        // Sessions are filed per workspace, not per date, so a file's mtime is
        // the only time signal available before it is parsed.
        .filter(|file| filter.since_ms.is_none_or(|since| file.mtime_ms >= since))
        .collect();
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Every `*.jsonl` under the sessions root, minus the derived transcripts.
fn log_entries(root: &Path) -> impl Iterator<Item = DirEntry> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(not_derived)
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && is_jsonl(entry.path()))
}

/// `pi-subagents` writes copies of calls already billed by the parent session
/// under `subagent-artifacts/`; ingesting them would count that spend twice.
fn not_derived(entry: &DirEntry) -> bool {
    !(entry.file_type().is_dir() && entry.file_name() == OsStr::new(paths::DERIVED_DIR))
}

fn is_jsonl(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("jsonl"))
}

/// Stops at the first non-empty, openable log.
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
            PiAdapter.semantics(),
            Semantics {
                usage_form: UsageForm::PerCall,
                meter: Meter::Tokens,
                model_attr: ModelAttr::Inline,
                dedupes_by_id: true,
                reports_quota: false,
            }
        );
        assert_eq!(Semantics::TOKENS_PER_CALL_INLINE, PiAdapter.semantics());
        assert_eq!(PiAdapter.id(), TOOL_ID);
        assert_eq!(PiAdapter.display_name(), "Pi");
    }

    /// A sibling shares the parser and the semantics, and must never share the id.
    #[test]
    fn cola_shares_the_dialect_and_carries_its_own_identity() {
        assert_eq!(ColaAdapter.id(), "cola");
        assert_eq!(ColaAdapter.display_name(), "Cola");
        assert_eq!(ColaAdapter.semantics(), PiAdapter.semantics());
        assert_ne!(ColaAdapter.id(), TOOL_ID);
    }

    #[test]
    fn an_empty_or_absent_tree_is_not_an_install() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        assert_eq!(discover_in(&sessions, &DateFilter::default()).len(), 0);
        assert!(first_log(&sessions).is_none());
        let workspace = sessions.join("--Users-demo-proj--");
        std::fs::create_dir_all(&workspace).unwrap();
        // A zero-byte log proves nothing about a working install, so `probe`
        // skips it, but `discover` still lists it: the indexer decides what an
        // empty file means instead of the adapter hiding it.
        std::fs::write(workspace.join("2026-09-22T14-16-39-708Z_uuid.jsonl"), "").unwrap();
        assert!(first_log(&sessions).is_none());
        assert_eq!(discover_in(&sessions, &DateFilter::default()).len(), 1);
    }
}

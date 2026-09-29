//! OpenCode adapter, and the siblings that speak its dialect.
//!
//! Source: `~/.local/share/opencode/opencode.db` (SQLite), honouring
//! `OPENCODE_DATA_DIR`. Assistant rows in `message.data` carry
//! `tokens.{input,output,reasoning,cache.{read,write}}`, `cost` and `modelID`.
//!
//! Two more products on this machine store their usage in exactly that shape, so
//! they are registered here as siblings sharing this parser rather than being
//! given a copy of it:
//!
//! - Crow5 — `~/.local/share/crow5`, `CROW5_DATA_DIR`, and its database name
//!   carries the app version, so the file is resolved by globbing (see
//!   [`paths::db_path_for`]).
//! - Mimocode — `~/.local/share/mimocode`, `MIMOCODE_DATA_DIR`.
//!
//! Neither fork documents an env override of its own, so the variables above are
//! tokenme's, following `OPENCODE_DATA_DIR`.
//!
//! Owned by the `codex-opencode-adapters` workstream; the `TOOL_ID` and
//! `OpenCodeAdapter` surfaces are frozen.

mod loader;
mod parser;
mod paths;

use usage_core::{
    DateFilter, DetectedSource, Error, ReadCursor, ReadOutcome, Semantics, SourceAdapter,
    SourceFile,
};

pub const TOOL_ID: &str = "opencode";

#[derive(Debug, Default, Clone, Copy)]
pub struct OpenCodeAdapter;

/// Crow5's assistant rows are OpenCode rows; only [`paths::CROW5`] differs.
#[derive(Debug, Default, Clone, Copy)]
pub struct Crow5Adapter;

/// Mimocode's assistant rows are OpenCode rows; only [`paths::MIMOCODE`] differs.
#[derive(Debug, Default, Clone, Copy)]
pub struct MimocodeAdapter;

/// The one implementation of the dialect, parameterised by product coordinates.
#[derive(Debug, Clone, Copy)]
struct ProductAdapter(&'static paths::Product);

impl ProductAdapter {
    /// The tool's own `message` table is the whole detection test: counting rows
    /// fails for a missing file, a database we cannot open in any usable mode,
    /// and a build without the table, all of which mean "nothing to show".
    fn located(&self) -> Option<(std::path::PathBuf, Vec<std::path::PathBuf>, i64)> {
        let dir = paths::data_dir_for(self.0)?;
        let mut files = Vec::new();
        let mut rows = 0i64;
        for db in paths::db_list_for(&dir, self.0) {
            if paths::stat_file(&db).is_none() {
                continue;
            }
            let Ok(conn) = paths::open_readonly(&db, paths::USABLE_SQL) else { continue };
            // `max(rowid)` seeks the table btree's rightmost page; it is not a scan.
            rows += conn
                .query_row("SELECT max(rowid) FROM message", [], |row| row.get(0))
                .unwrap_or(0);
            files.push(db);
        }
        (!files.is_empty()).then_some((dir, files, rows))
    }
}

impl SourceAdapter for ProductAdapter {
    fn id(&self) -> &'static str {
        self.0.id
    }

    fn display_name(&self) -> &'static str {
        self.0.display
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
        let (dir, files, rows) = self.located()?;
        let hint = match (rows, files.len()) {
            // A multi-store product has to say which files it read and how many it
            // skipped over, because that set is decided from the directory.
            (n, k) if n > 0 && self.0.multi_db && k > 1 => Some(format!(
                "{n} messages in {k} stores ({})",
                files.iter().map(|f| paths::file_label(f)).collect::<Vec<_>>().join(", ")
            )),
            (n, _) if n > 0 && self.0.multi_db => Some(format!("{n} messages in {}", paths::file_label(&files[0]))),
            (n, _) if n > 0 => Some(format!("{n} messages")),
            _ => None,
        };
        Some(DetectedSource {
            id: self.0.id.to_string(),
            display: self.display_name().to_string(),
            roots: vec![dir],
            hint,
        })
    }

    fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
        // One database holds every period, so `DateFilter` cannot prune the
        // *listing*; the rowid cursor is what keeps the read bounded instead.
        let Some(dir) = paths::data_dir_for(self.0) else { return Vec::new() };
        paths::db_list_for(&dir, self.0)
            .into_iter()
            .filter_map(|db| {
                let (size, mtime_ms) = paths::stat_file(&db)?;
                Some(SourceFile { path: db, kind: usage_core::FileKind::Sqlite, size, mtime_ms }.with_wal_activity())
            })
            .collect()
    }

    fn read(&self, file: &SourceFile, cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        // Deliberately infallible: a database the app has locked must not abort
        // the ingest pass for the other sources.
        Ok(loader::read_messages(&file.path, cursor, &file.key(), self.0))
    }
}

/// Each public adapter is the shared implementation plus its coordinates.
macro_rules! dialect_adapter {
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

dialect_adapter!(OpenCodeAdapter => paths::OPENCODE);
dialect_adapter!(Crow5Adapter => paths::CROW5);
dialect_adapter!(MimocodeAdapter => paths::MIMOCODE);

#[cfg(test)]
mod tests {
    use super::*;

    /// The siblings must not drift onto OpenCode's id, and OpenCode's frozen
    /// surface must keep answering exactly as before.
    #[test]
    fn each_product_stamps_its_own_identity() {
        assert_eq!(OpenCodeAdapter.id(), TOOL_ID);
        assert_eq!(OpenCodeAdapter.display_name(), "OpenCode");
        assert_eq!(Crow5Adapter.id(), "crow5");
        assert_eq!(Crow5Adapter.display_name(), "Crow5");
        assert_eq!(MimocodeAdapter.id(), "mimocode");
        assert_eq!(MimocodeAdapter.display_name(), "Mimocode");
        // Same dialect, so the same contract the indexer keys on.
        assert_eq!(Crow5Adapter.semantics(), MimocodeAdapter.semantics());
        assert_eq!(OpenCodeAdapter.semantics(), Crow5Adapter.semantics());
    }
}

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

/// What one product's stores actually carry, as [`ProductAdapter::located`] reports it.
struct Located {
    dirs: Vec<std::path::PathBuf>,
    files: Vec<std::path::PathBuf>,
    /// Rows across every store's live dialect table.
    rows: i64,
    /// Those tables, deduped — a store writing `session_message` must say so in
    /// the sources list, because that is the shape a silent blindness comes back as.
    tables: Vec<&'static str>,
}

impl ProductAdapter {
    /// The product's readable stores and what is in them. A database we cannot
    /// open in any usable mode, or that carries neither dialect record table,
    /// means "nothing to show" rather than an error.
    fn located(&self) -> Option<Located> {
        let dirs = paths::data_dirs_for(self.0);
        let mut files = Vec::new();
        let mut rows = 0i64;
        let mut tables: Vec<&'static str> = Vec::new();
        for dir in &dirs {
            for db in paths::db_list_for(dir, self.0) {
                if paths::stat_file(&db).is_none() {
                    continue;
                }
                let Ok(conn) = paths::open_readonly(&db, paths::USABLE_SQL) else { continue };
                // `max(rowid)` seeks the table btree's rightmost page; it is not a scan.
                let Some(dialect) = paths::dialect(&conn) else { continue };
                rows += dialect.max_rowid;
                if !tables.contains(&dialect.table) {
                    tables.push(dialect.table);
                }
                files.push(db);
            }
        }
        (!files.is_empty()).then_some(Located { dirs, files, rows, tables })
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
        let Located { dirs: dir, files, rows, tables } = self.located()?;
        // A store that writes anything but the table this dialect has always used
        // says so in the hint: that is the one visible place a schema flip can be
        // noticed before it turns into numbers that quietly stop moving.
        let other: Vec<&str> = tables.iter().copied().filter(|t| *t != "message").collect();
        let suffix = if other.is_empty() {
            String::new()
        } else {
            format!(" in `{}`", other.join("`, `"))
        };
        let hint = match (rows, files.len()) {
            // A multi-store product has to say which files it read and how many it
            // skipped over, because that set is decided from the directory.
            (n, k) if n > 0 && self.0.multi_db && k > 1 => Some(format!(
                "{n} messages in {k} stores ({}){suffix}",
                files.iter().map(|f| paths::file_label(f)).collect::<Vec<_>>().join(", ")
            )),
            (n, _) if n > 0 && self.0.multi_db => {
                Some(format!("{n} messages in {}{suffix}", paths::file_label(&files[0])))
            }
            (n, _) if n > 0 => Some(format!("{n} messages{suffix}")),
            _ => None,
        };
        Some(DetectedSource {
            id: self.0.id.to_string(),
            display: self.display_name().to_string(),
            roots: dir,
            hint,
        })
    }

    fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
        // One database holds every period, so `DateFilter` cannot prune the
        // *listing*; the rowid cursor is what keeps the read bounded instead.
        let mut out = Vec::new();
        for dir in paths::data_dirs_for(self.0) {
            for db in paths::db_list_for(&dir, self.0) {
                let Some((size, mtime_ms)) = paths::stat_file(&db) else { continue };
                out.push(
                    SourceFile { path: db, kind: usage_core::FileKind::Sqlite, size, mtime_ms }
                        .with_wal_activity(),
                );
            }
        }
        out
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

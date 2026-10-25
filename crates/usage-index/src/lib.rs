//! Incremental SQLite index over the raw logs the adapters read.
//!
//! A real machine holds ~430MB of Claude JSONL and ~3.8GB of Codex JSONL, and a
//! naive single-threaded full scan of the Codex tree measured 20.6s — unusable
//! for a menu-bar process that refreshes on a timer. So every source file is
//! tracked by `(size, mtime_ms, cursor)` in `file_state`: a steady-state pass
//! costs a `stat` per file plus zero inserts, and only appended bytes are ever
//! re-parsed.
//!
//! SQLite sources are statted through [`usage_core::SourceFile::with_wal_activity`]:
//! rows a vendor has written but not checkpointed live in `<db>-wal` while the main
//! file's own mtime stays frozen, so keying the change test on the main file held
//! fresh conversations invisible until the next checkpoint — measured here at 2
//! minutes on OpenCode's 1.6 GB store and a month on crow5's.
//!
//! The index deliberately stores **no money**. Prices change weekly, so cost is
//! derived at query time by [`usage_core::report::summarize`] from the live
//! [`usage_core::PricingMap`]; the index only stores token stages and credits.

mod facts;
mod ingest;
mod sync;
mod watcher;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use usage_core::{
    Call, CallKind, DateFilter, DetectedSource, Error, Meter, QuotaSample, Result, SourceAdapter,
    SourceStatus, TokenCounts, UsageEvent,
};

pub use ingest::{
    retention_cutoff, IngestOptions, IngestReport, CLAIM_TTL_MS, RETENTION_DAYS,
};
pub use sync::{
    default_sync_dir, hostname, sha256_file, ExportOptions, ExportReport, ImportOptions,
    ImportReport, SyncManifest, SyncSums, SYNC_FORMAT,
};
pub use watcher::Watcher;

/// Rebuild only the rollup days touched by a strict device-sync merge.  The
/// protocol crate owns validation and merge decisions; this keeps report-cache
/// repair in the same implementation used by local ingest.
pub fn recompute_network_days(
    tx: &rusqlite::Transaction<'_>,
    timestamps: &[i64],
) -> Result<()> {
    ingest::recompute_days_tx(tx, timestamps)
}

/// Bumping this wipes `event`/`call`/`quota`/`file_state` on next open, because
/// stale cursors would otherwise resume into a differently shaped row.
/// `4` — WorkBuddy changed caliber: its rows went from one rollup per closed
/// session (`workbuddy#<session>`, credits only) to one per LLM call
/// (`workbuddy#<call id>`, tokens + credits). The two key spaces never collide,
/// so an index that kept both would bill every session twice; a version bump is
/// what makes the old rows disappear instead of lingering as invisible money.
///
/// The `event_rollup.origin` column (machine scope) did NOT bump this: the
/// rollup is a cache, so its shape change is handled by [`drop_legacy_rollup`]
/// + the lazy rebuild, and a bump here would cost a full re-ingest of every
/// source tree — and worse, the `sync:` memos in `meta` survive a wipe and
/// would make every known bundle skip as "already imported" on the way back.
pub const INDEX_SCHEMA_VERSION: &str = "4";
const SCHEMA_VERSION: &str = INDEX_SCHEMA_VERSION;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS event(
    id          INTEGER PRIMARY KEY,
    tool        TEXT    NOT NULL,
    ts_ms       INTEGER NOT NULL,
    session     TEXT    NOT NULL,
    project     TEXT,
    model       TEXT,
    meter       TEXT    NOT NULL,
    in_tok      REAL,
    cc_tok      REAL,
    cr_tok      REAL,
    out_tok     REAL,
    reason_tok  REAL,
    credits     REAL,
    dedupe_key  TEXT,
    source      TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS event_ts      ON event(ts_ms);
CREATE INDEX IF NOT EXISTS event_tool_ts ON event(tool, ts_ms);
-- Partial unique index: `dedupe_key` is only set when the source carries a
-- stable record id (Claude message uuid, OpenCode message id), so dedupe is a
-- plain `INSERT OR IGNORE`. Codex records have no id and store NULL, and SQLite
-- treats every NULL as distinct, so two identical-looking Codex rows coexist
-- instead of silently swallowing real spend. Their idempotency comes from the
-- byte cursor in `file_state` plus purge-on-shrink in `ingest`: a rewritten or
-- truncated log has its own rows deleted before it is read again, so a replay
-- can never double count.
CREATE UNIQUE INDEX IF NOT EXISTS event_dedupe ON event(dedupe_key) WHERE dedupe_key IS NOT NULL;
CREATE TABLE IF NOT EXISTS call(
    event_id INTEGER NOT NULL,
    kind     TEXT    NOT NULL,
    name     TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS call_name  ON call(kind, name);
-- Loads an event's calls in `id` order during a single stitched pass.
CREATE INDEX IF NOT EXISTS call_event ON call(event_id);
-- The frozen `event` schema has no place for a source-reported quota sample
-- (Codex `rate_limits`), and `report::QuotaView` needs it, so samples live in
-- this side table keyed by their event instead of widening `event`.
CREATE TABLE IF NOT EXISTS quota(
    event_id       INTEGER PRIMARY KEY,
    used_percent   REAL    NOT NULL,
    window_minutes INTEGER NOT NULL,
    resets_at_ms   INTEGER NOT NULL,
    label          TEXT
);
CREATE TABLE IF NOT EXISTS file_state(
    source_key TEXT    PRIMARY KEY,
    tool       TEXT,
    size       INTEGER,
    mtime_ms   INTEGER,
    cursor     INTEGER,
    events     INTEGER,
    updated_ms INTEGER
);
-- P1 publish path: one row per local (day, origin, tool, session, project,
-- model, meter) group, maintained in the same transaction as the event
-- inserts, so a report can fold ~2.2k of these instead of ~390k events.
-- `project`/`model` are NOT NULL with `''` standing in for the event table's
-- NULL (a NOT NULL group key keeps the upsert simple); the fold maps `''`
-- back to `None`. `origin` is the machine the rows came from — `''` for this
-- machine, the bundle origin for merged imports — derived from the event's
-- `source` prefix by `ORIGIN_SQL`; the panel's scope filter reads it.
-- Additive to the frozen `event` schema: if it ever disagrees with the events,
-- the reads below wipe and rebuild it lazily rather than losing data.
CREATE TABLE IF NOT EXISTS event_rollup(
    day        TEXT    NOT NULL,
    origin     TEXT    NOT NULL DEFAULT '',
    tool       TEXT    NOT NULL,
    session    TEXT    NOT NULL,
    project    TEXT    NOT NULL DEFAULT '',
    model      TEXT    NOT NULL DEFAULT '',
    meter      TEXT    NOT NULL,
    in_tok     REAL    NOT NULL,
    cc_tok     REAL    NOT NULL,
    cr_tok     REAL    NOT NULL,
    out_tok    REAL    NOT NULL,
    reason_tok REAL    NOT NULL,
    credits    REAL    NOT NULL,
    n          INTEGER NOT NULL,
    nonzero    INTEGER NOT NULL,
    min_ts     INTEGER NOT NULL,
    max_ts     INTEGER NOT NULL,
    PRIMARY KEY(day, origin, tool, session, project, model, meter)
);
-- Synced copies of a peer's own rollup rows (rollup-level device sync):
-- one row set per source origin, written by the sync's full-snapshot
-- apply, read by the report fold alongside event_rollup. Never touched
-- by recompute_network_days — the source device owns their truth.
CREATE TABLE IF NOT EXISTS remote_rollup(
    day        TEXT    NOT NULL,
    origin     TEXT    NOT NULL DEFAULT '',
    tool       TEXT    NOT NULL,
    session    TEXT    NOT NULL,
    project    TEXT    NOT NULL DEFAULT '',
    model      TEXT    NOT NULL DEFAULT '',
    meter      TEXT    NOT NULL,
    in_tok     REAL    NOT NULL,
    cc_tok     REAL    NOT NULL,
    cr_tok     REAL    NOT NULL,
    out_tok    REAL    NOT NULL,
    reason_tok REAL    NOT NULL,
    credits    REAL    NOT NULL,
    n          INTEGER NOT NULL,
    nonzero    INTEGER NOT NULL,
    min_ts     INTEGER NOT NULL,
    max_ts     INTEGER NOT NULL,
    PRIMARY KEY(day, origin, tool, session, project, model, meter)
);
-- Serves the per-session correlated lookups (first project / last model) and
-- the newest-event probe without a full scan.
CREATE INDEX IF NOT EXISTS event_tool_session ON event(tool, session);
-- The lifetime total already billed for every `Cumulative` source's key. Such a
-- source re-emits one session's whole-to-date totals on every pass, so the
-- ledger is what turns that projection into growth: only the difference since
-- the recorded total enters `event`, stamped with the day the growth was seen
-- (see `persist`'s slice branch). Component-wise, never backwards, and derived
-- from `event` — a purge or rebuild that drops the rows must drop the row here
-- too, or the re-read would bill nothing.
CREATE TABLE IF NOT EXISTS cumulative_state(
    dedupe_key TEXT    PRIMARY KEY,
    tool       TEXT    NOT NULL,
    source     TEXT    NOT NULL,
    in_tok     REAL    NOT NULL,
    cc_tok     REAL    NOT NULL,
    cr_tok     REAL    NOT NULL,
    out_tok    REAL    NOT NULL,
    reason_tok REAL    NOT NULL,
    credits    REAL    NOT NULL,
    ts_ms      INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS cumulative_state_source ON cumulative_state(source);
-- Keys: `schema_version`, `index_id`, `last_ingest_ms`, and `index_claim` — the
-- single-row ingest lease that keeps two processes off the same cursors.
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
-- Cycle-estimation sums (sum_tokens / sum_tokens_by_model) run per quota row
-- on every publish. This partial covering index turns those 300ms range
-- scans over the table into index-only scans: seek (tool, ts_ms), aggregate
-- the token columns straight from the index. Partial on the exact predicates
-- the cycle filter applies, so most rows never enter the index at all.
CREATE INDEX IF NOT EXISTS event_cycle_tokens ON event(tool, ts_ms, model, in_tok, cc_tok, cr_tok, out_tok)
    WHERE meter = 'tokens' AND source NOT LIKE 'linux:%';
"#;

pub(crate) const INSERT_EVENT: &str = "INSERT OR IGNORE INTO event(\
    tool, ts_ms, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, \
    reason_tok, credits, dedupe_key, source) \
    VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)";

const SELECT_EVENT_COLS: &str = "id, tool, ts_ms, session, project, model, meter, in_tok, cc_tok, \
    cr_tok, out_tok, reason_tok, credits, dedupe_key, source";

pub(crate) fn sql_err(e: rusqlite::Error) -> Error {
    match e {
        rusqlite::Error::SqliteFailure(code, message) => Error::Sqlite(format!(
            "{} (sqlite_code={:?}, sqlite_extended_code={})",
            message.unwrap_or_else(|| "SQLite failure".into()),
            code.code,
            code.extended_code,
        )),
        other => Error::Sqlite(other.to_string()),
    }
}

/// The origin of a row's `source`, in SQL. The Rust twin is
/// [`usage_core::origin_of`]; both must agree. Interpreted: everything a
/// bundle merge wrote (`linux:<origin>:<their key>` — see `sync.rs`) shadows
/// under its origin, and everything else is this machine (`''`). A `linux:`
/// without a second colon was never written by a merge, so it stays local
/// rather than inventing an origin.
pub const ORIGIN_SQL: &str = "\
CASE WHEN substr(source, 1, 6) = 'linux:' AND instr(substr(source, 7), ':') > 0 \
     THEN substr(source, 7, instr(substr(source, 7), ':') - 1) \
     ELSE '' END";

/// Moves a corrupted index and its sidecars aside, keeping them for forensics.
/// The connection must already be closed.
///
/// The sidecars move first: a live peer (an app instance running an older
/// build, say) can hold `-wal`/`-shm` open, and if only the main file were
/// renamed away its next write would recreate them around the fresh database —
/// two databases sharing one WAL path. If any rename fails the already-moved
/// files are restored and the caller's error stands unchanged.
fn quarantine(path: &Path) -> bool {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    let candidates: Vec<PathBuf> = ["-shm", "-wal", "-journal", ""]
        .iter()
        .filter_map(|suffix| {
            let from = dir.join(format!("{name}{suffix}"));
            from.exists().then_some(from)
        })
        .collect();
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for from in &candidates {
        let to = from.with_file_name(format!(
            "{}.corrupt-{stamp}",
            from.file_name().and_then(|n| n.to_str()).unwrap_or_default()
        ));
        match std::fs::rename(from, &to) {
            Ok(()) => moved.push((from.clone(), to)),
            Err(_) => {
                // Half-moved is worse than not moved: a live peer would keep
                // writing through the old handles while the fresh database
                // recreates the sidecars — two databases, one WAL path.
                for (from, to) in &moved {
                    let _ = std::fs::rename(to, from);
                }
                return false;
            }
        }
    }
    true
}

fn sync_state_marker_path(path: &Path) -> PathBuf {
    path.with_extension("device-sync-state")
}

fn sync_state_marker_exists(path: &Path) -> bool {
    sync_state_marker_path(path).exists()
}

pub(crate) fn now_ms() -> i64 {
    usage_core::report::now_ms()
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageEvent> {
    let input: Option<f64> = row.get(7)?;
    let cache_creation: Option<f64> = row.get(8)?;
    let cache_read: Option<f64> = row.get(9)?;
    let output: Option<f64> = row.get(10)?;
    let reasoning: Option<f64> = row.get(11)?;
    let credits: Option<f64> = row.get(12)?;
    let meter: String = row.get(6)?;
    Ok(UsageEvent {
        tool: row.get(1)?, ts_ms: row.get(2)?, session: row.get(3)?,
        project: row.get(4)?, model: row.get(5)?,
        counts: TokenCounts {
            input: input.unwrap_or(0.0), cache_creation: cache_creation.unwrap_or(0.0),
            cache_read: cache_read.unwrap_or(0.0), output: output.unwrap_or(0.0),
            reasoning: reasoning.unwrap_or(0.0), credits: credits.unwrap_or(0.0),
        },
        meter: meter_of(&meter), dedupe_key: row.get(13)?, calls: Vec::new(), quota: None,
        source: row.get(14)?,
    })
}

/// Drops a pre-`origin` `event_rollup` so the new SCHEMA recreates it.
///
/// The rollup is a pure cache of `event`, so the migration is a drop, not a
/// rebuild: the next report read finds it empty, rebuilds from the events on
/// disk, and no row is lost. Detection is by column shape, not by
/// `SCHEMA_VERSION` — a version bump would re-ingest every source tree, and
/// the `sync:file:` memos in `meta` (which a wipe does not clear) would then
/// make every imported bundle skip as already-known and lose the merged rows
/// for good.
fn drop_legacy_rollup(conn: &Connection) -> Result<()> {
    let table: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'event_rollup')",
            [],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    if !table {
        return Ok(());
    }
    let has_origin: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('event_rollup') WHERE name = 'origin')",
            [],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    if !has_origin {
        conn.execute("DROP TABLE event_rollup", []).map_err(sql_err)?;
    }
    Ok(())
}

pub(crate) fn meter_str(m: Meter) -> &'static str {
    match m {
        Meter::Tokens => "tokens",
        Meter::Credits => "credits",
    }
}

pub(crate) fn meter_of(s: &str) -> Meter {
    match s {
        "credits" => Meter::Credits,
        _ => Meter::Tokens,
    }
}

pub(crate) fn call_kind_str(k: CallKind) -> &'static str {
    match k {
        CallKind::Mcp => "mcp",
        CallKind::Skill => "skill",
    }
}

pub(crate) fn call_kind_of(s: &str) -> CallKind {
    match s {
        "skill" => CallKind::Skill,
        _ => CallKind::Mcp,
    }
}

/// Owned SQLite connection. `Connection` is `Send` but not `Sync`, so all writes
/// happen on the calling thread and only reads are fanned out to workers.
pub struct Index {
    pub(crate) conn: Connection,
    pub(crate) path: Option<PathBuf>,
    pub(crate) max_workers: usize,
    pub(crate) errors: Vec<String>,
    /// Identifies *this* connection's row in the ingest-claim protocol.
    pub(crate) claim_token: String,
    /// Per-source last emission of the "changed but read emitted 0 events"
    /// note. A busy source whose writes legitimately touch non-usage tables
    /// would otherwise repeat the note on every pass; one line per hour keeps
    /// the signal without the flood. In-memory: a restart re-anchors it.
    pub(crate) note_gate: std::collections::HashMap<String, std::time::Instant>,
}

impl Index {
    /// Opens (creating if needed) the index at `path`.
    ///
    /// A file that cannot be opened at all (damaged header, a torn WAL) is
    /// quarantined and recreated empty: every event is re-derivable from the
    /// source logs, so the next ingest rebuilds the whole index instead of every
    /// read failing forever with garbage-row errors. A file that opens but is
    /// internally damaged is caught by [`Index::verify_integrity`], which is
    /// deliberately a separate call — it reads the whole file, and a panel that
    /// is trying to show numbers should not wait for that.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match Self::open_connection_with_busy_timeout(&path, 5_000) {
            Ok(index) => Ok(index),
            Err(err) if err.is_corruption() => {
                // A device-sync index is no longer disposable cache: it can
                // contain remote rows, receipts, trust state and unACKed
                // journal data.  The marker is intentionally outside SQLite
                // so even a malformed header fails closed before quarantine.
                if sync_state_marker_exists(&path) {
                    return Err(Error::Sqlite(format!(
                        "device-sync index is corrupt; original database was preserved for recovery: {err}"
                    )));
                }
                if quarantine(&path) {
                    Self::open_connection(&path)
                } else {
                    // Something still holds the file (a live peer, an antivirus
                    // scan); retrying now would just fail again — the next
                    // open heals it.
                    Err(err)
                }
            }
            Err(err) => Err(err),
        }
    }

    /// Opens the index for a foreground owner that can rebuild local usage.
    ///
    /// Device sync normally makes corruption fail closed because replacing the
    /// file could discard remote history, receipts, or trust state. That policy
    /// is correct for a sync transaction, but it must not leave the panel with
    /// an empty report and an endless "indexing" state. This entry point keeps
    /// the damaged image and its sidecars under a `.corrupt-*` name, then
    /// creates a new schema so the source adapters can rebuild local usage.
    /// Callers must surface the recovery in their log/UI; the preserved image is
    /// the recovery input for a later sync-aware salvage tool.
    pub fn open_recovering_corruption(path: impl Into<PathBuf>) -> Result<(Self, bool)> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match Self::open_connection(&path) {
            Ok(index) => Ok((index, false)),
            Err(err) if err.is_corruption() => {
                if !quarantine(&path) {
                    return Err(err);
                }
                Self::open_connection(&path).map(|index| (index, true))
            }
            Err(err) => Err(err),
        }
    }

    /// Opens an index without the legacy corruption quarantine.  SyncStore
    /// uses this entry point because replacing a damaged database would lose
    /// remote history, receipts, trust and pending outbox state.
    pub fn open_preserving_corruption(path: impl Into<PathBuf>) -> Result<Self> {
        Self::open_preserving_corruption_with_busy_timeout(path, 5_000)
    }

    pub fn open_preserving_corruption_with_busy_timeout(path: impl Into<PathBuf>, busy_timeout_ms: i64) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::open_connection_with_busy_timeout(&path, busy_timeout_ms)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| Error::Sqlite(e.to_string()))?;
        Self::setup(conn, None)
    }

    fn open_connection(path: &Path) -> Result<Self> {
        Self::open_connection_with_busy_timeout(path, 5_000)
    }

    fn open_connection_with_busy_timeout(path: &Path, busy_timeout_ms: i64) -> Result<Self> {
        let conn = Connection::open(path).map_err(|e| Error::io(path, std::io::Error::other(e)))?;
        conn.busy_timeout(std::time::Duration::from_millis(busy_timeout_ms as u64)).map_err(sql_err)?;
        let index = Self::setup(conn, Some(path.to_path_buf()))?;
        Ok(index)
    }

    /// `PRAGMA quick_check` — the whole file, read once.
    fn quick_check_ok(conn: &Connection) -> bool {
        conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
            .map(|row| row == "ok")
            .unwrap_or(false)
    }

    /// Read the image once and, if it is damaged, move it aside and rebuild the
    /// schema in place so the next ingest refills it.
    ///
    /// This is deliberately *not* part of [`Index::open`]: quick_check touches
    /// every page, which on a 170 MB index measured 0.4 s warm and 3.6 s cold —
    /// exactly the time a menu-bar panel spends showing its loading card on a
    /// cold start. The check still runs on every launch, just after the panel has
    /// its numbers and before anything is written, so a damaged file still heals
    /// as a re-ingest instead of failing every read with garbage errors forever.
    ///
    /// Returns the index — rebuilt in place when the image was damaged — and
    /// whether that replacement happened.
    pub fn verify_integrity(self) -> Result<(Self, bool)> {
        if Self::quick_check_ok(&self.conn) {
            return Ok((self, false));
        }
        if self.has_device_sync_state() || self.path().is_some_and(sync_state_marker_exists) {
            return Err(Error::Sqlite(
                "device-sync index failed integrity; original database was preserved for recovery".into(),
            ));
        }
        let Some(path) = self.path().map(Path::to_path_buf) else {
            return Err(Error::Sqlite("in-memory index failed quick_check".into()));
        };
        // Close the handle before moving the image aside. Windows keeps the
        // database and its WAL/SHM sidecars locked while this connection is
        // alive; quarantining first therefore failed and left the panel
        // retrying the same malformed image forever.
        drop(self);
        let moved = quarantine(&path);
        if !moved {
            // Something still holds the file (a live peer, an antivirus scan);
            // retrying now would just fail again — the next open heals it.
            return Err(Error::Sqlite("database disk image is malformed (quick_check)".into()));
        }
        let conn = Connection::open(&path).map_err(|e| Error::io(&path, std::io::Error::other(e)))?;
        Ok((Self::setup(conn, Some(path))?, true))
    }

    /// `~/.local/share/tokenme/index.db` style location, or `None` when the
    /// platform exposes no data dir.
    pub fn default_path() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("tokenme").join("index.db"))
    }

    pub(crate) fn setup(conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        // WAL + NORMAL: a reporting query never blocks behind an ingest write,
        // and a crash at worst loses the last ingest (which is re-derivable).
        // The busy timeout itself is chosen by the opener before setup runs, so
        // a sync-owned connection keeps its short budget even inside schema
        // migration; this batch only fixes durability and journaling.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;",
        )
        .map_err(sql_err)?;
        drop_legacy_rollup(&conn)?;
        // The row-era sync projection is shed on open: the aggregate-snapshot
        // era has no readers for these tables, so an existing database drops
        // them (and their meta keys) once here, and a fresh one never creates
        // them — SCHEMA below no longer declares any of it.
        conn.execute_batch(
            "DROP TABLE IF EXISTS sync_dirty;
             DROP TABLE IF EXISTS sync_event_identity;
             DROP TABLE IF EXISTS sync_event_occurrence;
             DROP TABLE IF EXISTS sync_occurrence;",
        ).map_err(sql_err)?;
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
        // Their meta keys go with the tables — after SCHEMA, so a fresh index
        // has a meta table to delete from.
        conn.execute_batch(
            "DELETE FROM meta WHERE key IN (
                 'device_sync_dirty_revision', 'device_sync_dirty_json_bytes',
                 'device_sync_occurrence_init_event_id', 'device_sync_initial_coverage_lo');",
        ).map_err(sql_err)?;
        let stored: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0))
            .optional()
            .map_err(sql_err)?;
        if stored.as_deref() != Some(SCHEMA_VERSION) {
            // Dropping (not just deleting) is required: `CREATE TABLE IF NOT
            // EXISTS` would otherwise keep the previous column set, so a schema
            // change that adds a column — e.g. `quota.label` — would silently
            // survive as a table that cannot answer the new queries.
            conn.execute_batch(
                "DROP TABLE IF EXISTS event; DROP TABLE IF EXISTS call; DROP TABLE IF EXISTS quota; \
                 DROP TABLE IF EXISTS file_state; DROP TABLE IF EXISTS event_rollup;",
            )
            .map_err(sql_err)?;
            conn.execute_batch(SCHEMA).map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', ?1)",
                params![SCHEMA_VERSION],
            )
            .map_err(sql_err)?;
        }
        let index_id: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'index_id'", [], |r| r.get(0))
            .optional()
            .map_err(sql_err)?;
        if index_id.is_none() {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let id = format!("{:x}-{:x}", nanos, std::process::id());
            conn.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('index_id', ?1)", params![id])
                .map_err(sql_err)?;
        }
        // One-shot repair for rows first-written before replace-on-grow landed
        // (77f8d95, v0.1.5 build 105): a cumulative session indexed mid-flight
        // back then kept its first-write snapshot forever — the finished
        // projection never changes again, so the replace path never sees a
        // re-emit to lift it. Dropping the read cursors of the cumulative
        // adapters makes the next pass re-read every projection from byte 0
        // and re-emit its final totals; replace-on-grow raises the frozen rows
        // to truth, a ledger that shrank meanwhile simply stays, and
        // single-shot re-emits dedupe away. The list is exactly the adapters
        // whose audit pinned this shape AND whose dedupe-key space is
        // unchanged since 77f8d95 — a changed key space would leave the old
        // rows stranded beside the new ones (double-count), which is why
        // antigravity is NOT here: 61c4f96 redrew its buckets. This is
        // deliberately not a SCHEMA_VERSION bump: the `sync:` memos in meta
        // survive a wipe and would strand every merged bundle as "already
        // imported" (see that constant).
        const REPAIR_CUMULATIVE_RESCAN: &str = "repair:cumulative-rescan-1";
        let repaired: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_CUMULATIVE_RESCAN],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if repaired.is_none() {
            conn.execute(
                "DELETE FROM file_state WHERE tool IN \
                 ('dsh', 'hermes', 'funide', 'joycode', 'claude', 'atomcode')",
                [],
            )
            .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_CUMULATIVE_RESCAN],
            )
            .map_err(sql_err)?;
        }
        // One-shot re-seed for antigravity, which the cursor drop above cannot
        // fix: 61c4f96 redrew its buckets (retry boxes no longer sum into their
        // parent), so pre-61c4f96 rows hold inflated totals the replace path
        // will never accept (a redrawn key strands the old rows beside the new
        // ones; a same-key total only ever shrinks and loses to grow-on-write).
        // The local rows go and the next pass re-reads the conversation
        // databases wholesale under the new semantics. Merged rows are
        // another machine's truth and stay — `source NOT LIKE 'linux:%'` — as
        // does their rollup (`origin = ''` is this machine).
        const REPAIR_ANTIGRAVITY_RESEED: &str = "repair:antigravity-reseed-1";
        let reseeded: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_ANTIGRAVITY_RESEED],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if reseeded.is_none() {
            conn.execute(
                "DELETE FROM call WHERE event_id IN \
                 (SELECT id FROM event WHERE tool = 'antigravity' AND source NOT LIKE 'linux:%')",
                [],
            )
            .map_err(sql_err)?;
            conn.execute(
                "DELETE FROM quota WHERE event_id IN \
                 (SELECT id FROM event WHERE tool = 'antigravity' AND source NOT LIKE 'linux:%')",
                [],
            )
            .map_err(sql_err)?;
            conn.execute(
                "DELETE FROM event WHERE tool = 'antigravity' AND source NOT LIKE 'linux:%'",
                [],
            )
            .map_err(sql_err)?;
            conn.execute(
                "DELETE FROM event_rollup WHERE tool = 'antigravity' AND origin = ''",
                [],
            )
            .map_err(sql_err)?;
            conn.execute("DELETE FROM file_state WHERE tool = 'antigravity'", [])
                .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_ANTIGRAVITY_RESEED],
            )
            .map_err(sql_err)?;
        }
        // One-shot split for the Antigravity IDE: rows read from the IDE's own
        // store (`~/.gemini/antigravity-ide`) used to land under `antigravity`
        // beside the desktop app's — one login's numbers under a row the other
        // login owns (the trae split's shape). The loader now prefixes every
        // key with the tool id, because the dedupe index is one global unique
        // list, so both sides re-key: the IDE rows re-home first, then
        // everything left under `antigravity` takes the prefixed spelling.
        // The rollup is wiped wholesale — the moved rows cannot be un-summed
        // from their day groups in place, and the cache rebuilds lazily from
        // `event` on the next report read.
        const REPAIR_ANTIGRAVITY_IDE_SPLIT: &str = "repair:antigravity-ide-split-1";
        let ide_split: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_ANTIGRAVITY_IDE_SPLIT],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if ide_split.is_none() {
            // New-key twins go first: a build that already re-read a store
            // under the prefixed loader holds `antigravity*#<session>#<id>`
            // beside the old spelling, and re-keying the old spelling would
            // hit the unique dedupe index (the trae split's verified lesson).
            const IDE_STORE: &str = "instr(source, 'antigravity-ide') > 0";
            let ide_twin = format!(
                "SELECT id FROM event e WHERE e.tool = 'antigravity' AND {IDE_STORE} AND EXISTS \
                 (SELECT 1 FROM event t WHERE t.tool = 'antigravity_ide' \
                  AND t.dedupe_key = 'antigravity_ide#' || e.dedupe_key)"
            );
            let desktop_twin = format!(
                "SELECT id FROM event e WHERE e.tool = 'antigravity' AND NOT {IDE_STORE} AND EXISTS \
                 (SELECT 1 FROM event t WHERE t.tool = 'antigravity' \
                  AND t.dedupe_key = 'antigravity#' || e.dedupe_key)"
            );
            for twin in [ide_twin, desktop_twin] {
                for table in ["call", "quota"] {
                    conn.execute(&format!("DELETE FROM {table} WHERE event_id IN ({twin})"), [])
                        .map_err(sql_err)?;
                }
                conn.execute(&format!("DELETE FROM event WHERE id IN ({twin})"), [])
                    .map_err(sql_err)?;
            }
            conn.execute(
                "UPDATE event SET tool = 'antigravity_ide', \
                     dedupe_key = 'antigravity_ide#' || dedupe_key \
                 WHERE tool = 'antigravity' AND instr(source, 'antigravity-ide') > 0",
                [],
            )
            .map_err(sql_err)?;
            conn.execute(
                "UPDATE event SET dedupe_key = 'antigravity#' || dedupe_key \
                 WHERE tool = 'antigravity'",
                [],
            )
            .map_err(sql_err)?;
            conn.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
            conn.execute("DELETE FROM file_state WHERE tool = 'antigravity_ide'", [])
                .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_ANTIGRAVITY_IDE_SPLIT],
            )
            .map_err(sql_err)?;
        }
        // One-shot re-homing for the Trae split: rows read from a CN edition's
        // store used to land under `trae` with a `trae#<turn>` key; they are
        // their own tool now. The rollup is wiped wholesale — the moved rows
        // cannot be un-summed from their day groups in place, and the cache
        // rebuilds lazily from `event` on the next report read.
        const REPAIR_TRAE_SPLIT: &str = "repair:trae-split-1";
        let split: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_TRAE_SPLIT],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if split.is_none() {
            // instr, not a slash-pinned LIKE: Windows source keys carry
            // backslashes, and the split happened on those machines too.
            // The new-key twins go first: a build that already re-read the CN
            // store holds `trae_cn#<turn>` beside the old `trae#<turn>`, and
            // re-keying the old spelling would hit the unique dedupe index —
            // verified against a live index copy (UNIQUE constraint failed).
            // Those rows are pure duplicates; the rest re-home in place.
            const CN_STORE: &str = "(instr(source, 'Trae CN') > 0 OR instr(source, 'TRAE SOLO CN') > 0)";
            let twin = format!(
                "SELECT id FROM event e WHERE e.tool = 'trae' AND {CN_STORE} AND EXISTS \
                 (SELECT 1 FROM event t WHERE t.tool = 'trae_cn' \
                  AND t.dedupe_key = 'trae_cn#' || substr(e.dedupe_key, 6))"
            );
            conn.execute(&format!("DELETE FROM call WHERE event_id IN ({twin})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM quota WHERE event_id IN ({twin})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM event WHERE id IN ({twin})"), [])
                .map_err(sql_err)?;
            conn.execute(
                "UPDATE event SET tool = 'trae_cn', \
                     dedupe_key = 'trae_cn#' || substr(dedupe_key, 6) \
                 WHERE tool = 'trae' AND (instr(source, 'Trae CN') > 0 \
                   OR instr(source, 'TRAE SOLO CN') > 0)",
                [],
            )
            .map_err(sql_err)?;
            conn.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_TRAE_SPLIT],
            )
            .map_err(sql_err)?;
        }
        // One-shot retirement of the dsh v4 projection rows: the projection is
        // the session's LIFETIME cumulative stamped with `lastPromptAt`, so a
        // session continued across days dragged its whole total into the
        // newest day — 12M billed yesterday read as 12M again today, and
        // yesterday went to zero (reported 2026-10-10, session d2b4cf00). The
        // v4 stream carries the per-call events itself (`assistant/message`,
        // whose sums equal the projection's totals exactly) and bills now, so
        // the cumulative rows would sit beside them as double money. The local
        // rows go and the next pass re-reads the streams wholesale under the
        // day-true attribution; merged rows are another machine's truth and
        // stay (`source NOT LIKE 'linux:%'`, the antigravity rule), and the
        // tool's rollup rows rebuild lazily from `event` on the next report
        // read (the trae-split precedent).
        const REPAIR_DSH_PROJECTION_RETIRED: &str = "repair:dsh-projection-retired-1";
        let dsh_proj: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_DSH_PROJECTION_RETIRED],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if dsh_proj.is_none() {
            const DSH_PROJ_ROWS: &str =
                "(SELECT id FROM event WHERE tool = 'dsh' AND dedupe_key LIKE '%#proj' \
                 AND source NOT LIKE 'linux:%')";
            conn.execute(&format!("DELETE FROM call WHERE event_id IN ({DSH_PROJ_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM quota WHERE event_id IN ({DSH_PROJ_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM event WHERE id IN ({DSH_PROJ_ROWS})"), [])
                .map_err(sql_err)?;
            // Every dsh source re-reads from byte 0: the streams' own events
            // re-emit under unchanged dedupe keys (replace no-ops), and the
            // retired projections' stale cursors go with it.
            conn.execute("DELETE FROM file_state WHERE tool = 'dsh'", []).map_err(sql_err)?;
            conn.execute("DELETE FROM event_rollup WHERE tool = 'dsh' AND origin = ''", [])
                .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_DSH_PROJECTION_RETIRED],
            )
            .map_err(sql_err)?;
        }
        // One-shot retirement of the FunIDE lifetime projection, the same shape
        // as the dsh one above: the adapter stamped one event per session with
        // the session's `updatedAt`, so a session continued across a day
        // boundary dragged its whole total into the newest day (measured
        // 2026-10-21, session smvf8pvrm7qrg: 477M prompt tokens read on 10-21
        // while 10-19 and 10-20 — the days its 13 turns ran — went to zero). The
        // session items carry that money per turn with the turn's own start ms
        // in `turnId`, so the adapter now emits those day-true turn events.
        // The session-shaped rows must go, not stay: their key is
        // `funide#<session>` and the turn rows are `funide#<session>#<turn>`, so
        // a re-emit would leave the lifetime total standing beside its own turns
        // as double money. Turn-shaped rows are the new shape and stay, merged
        // rows are another machine's truth and stay (`source NOT LIKE 'linux:%'`,
        // the antigravity rule), and the tool's rollup rows refold when the
        // re-read lands (the dsh precedent).
        const REPAIR_FUNIDE_PROJECTION_RETIRED: &str = "repair:funide-projection-retired-1";
        let funide_proj: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_FUNIDE_PROJECTION_RETIRED],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if funide_proj.is_none() {
            const FUNIDE_PROJ_ROWS: &str = "(SELECT id FROM event WHERE tool = 'funide' \
                 AND dedupe_key LIKE 'funide#%' AND dedupe_key NOT LIKE 'funide#%#%' \
                 AND source NOT LIKE 'linux:%')";
            conn.execute(&format!("DELETE FROM call WHERE event_id IN ({FUNIDE_PROJ_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM quota WHERE event_id IN ({FUNIDE_PROJ_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM event WHERE id IN ({FUNIDE_PROJ_ROWS})"), [])
                .map_err(sql_err)?;
            // Every session file re-reads from byte 0: the turn events re-emit
            // under keys no past pass wrote for these files.
            conn.execute("DELETE FROM file_state WHERE tool = 'funide'", []).map_err(sql_err)?;
            conn.execute("DELETE FROM event_rollup WHERE tool = 'funide' AND origin = ''", [])
                .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_FUNIDE_PROJECTION_RETIRED],
            )
            .map_err(sql_err)?;
        }
        // One-shot re-billing of the Hermes rows: the dedupe key was
        // `hermes#<session>#<model>#<provider>#<task>` — four of the vendor's
        // six primary-key fields — so two slices differing only in billing
        // endpoint or mode collided into one row and undercounted (2026-10-21).
        // The key now carries all six. Every old row therefore stands under a
        // key no future pass will touch, and worse: the new key's first
        // projection would seed its ledger at zero and re-bill the slice's
        // whole lifetime beside the frozen old row. The rows must go, not stay:
        // when the vendor store re-reads, each slice's lifetime lands on its
        // own last-seen day, and the ledger goes with the rows so the re-read
        // seeds from what is actually on the books — nothing. Merged rows are
        // another machine's truth and stay (`source NOT LIKE 'linux:%'`, the
        // antigravity rule); the tool's rollup rows refold when the re-read
        // lands (the dsh precedent).
        const REPAIR_HERMES_SLICE_KEY: &str = "repair:hermes-slice-key-1";
        let hermes_key: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_HERMES_SLICE_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if hermes_key.is_none() {
            const HERMES_LOCAL_ROWS: &str =
                "(SELECT id FROM event WHERE tool = 'hermes' AND source NOT LIKE 'linux:%')";
            conn.execute(&format!("DELETE FROM call WHERE event_id IN ({HERMES_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM quota WHERE event_id IN ({HERMES_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM event WHERE id IN ({HERMES_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute("DELETE FROM cumulative_state WHERE tool = 'hermes'", [])
                .map_err(sql_err)?;
            // Every vendor store re-reads whole: the projections re-emit under
            // keys no past pass wrote for these files.
            conn.execute("DELETE FROM file_state WHERE tool = 'hermes'", []).map_err(sql_err)?;
            conn.execute("DELETE FROM event_rollup WHERE tool = 'hermes' AND origin = ''", [])
                .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_HERMES_SLICE_KEY],
            )
            .map_err(sql_err)?;
        }
        // One-shot re-key of the Kimi Code rows: the dedupe key carried the
        // restated amounts (`…#{input}#{creation}#{read}#{output}`), and a
        // transcript restating its accumulated totals is this tool's normal
        // heartbeat — every restatement stranded the old row under its old
        // key and billed the call again under the new one (2026-10-21). The
        // key is now the call's identity (`who#stamp#model`) and a
        // restatement updates in place. Every stored row stands under an
        // old-shape key no future pass will touch and must go; the re-read
        // re-bills each call once under its identity key. Merged rows are
        // another machine's truth and stay (`source NOT LIKE 'linux:%'`, the
        // antigravity rule); the rollup rows refold when the re-read lands
        // (the dsh precedent).
        const REPAIR_KIMICODE_CALL_KEY: &str = "repair:kimicode-call-key-1";
        let kimicode_key: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_KIMICODE_CALL_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if kimicode_key.is_none() {
            const KIMICODE_LOCAL_ROWS: &str =
                "(SELECT id FROM event WHERE tool = 'kimicode' AND source NOT LIKE 'linux:%')";
            conn.execute(&format!("DELETE FROM call WHERE event_id IN ({KIMICODE_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM quota WHERE event_id IN ({KIMICODE_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM event WHERE id IN ({KIMICODE_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            // Every transcript re-reads whole: the calls re-emit under keys
            // no past pass wrote for these files.
            conn.execute("DELETE FROM file_state WHERE tool = 'kimicode'", []).map_err(sql_err)?;
            conn.execute("DELETE FROM event_rollup WHERE tool = 'kimicode' AND origin = ''", [])
                .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_KIMICODE_CALL_KEY],
            )
            .map_err(sql_err)?;
        }
        // One-shot re-key of the Cline rows without a vendor message id: the
        // fallback key was positional (`{session}#idx{idx}`), so dropping an
        // earlier row of the transcript renumbered every survivor and
        // re-billed each under its new key (2026-10-21). The fallback now
        // keys on the row's own stamp with a first-in-order ordinal.
        // Fallback rows stand under keys no future pass will touch and must
        // go; id-bearing rows re-emit under their unchanged keys and
        // re-insert as they were. Merged rows are another machine's truth
        // and stay (`source NOT LIKE 'linux:%'`, the antigravity rule); the
        // rollup rows refold when the re-read lands (the dsh precedent).
        const REPAIR_CLINE_MESSAGE_KEY: &str = "repair:cline-message-key-1";
        let cline_key: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_CLINE_MESSAGE_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if cline_key.is_none() {
            const CLINE_LOCAL_ROWS: &str =
                "(SELECT id FROM event WHERE tool = 'cline' AND source NOT LIKE 'linux:%')";
            conn.execute(&format!("DELETE FROM call WHERE event_id IN ({CLINE_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM quota WHERE event_id IN ({CLINE_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM event WHERE id IN ({CLINE_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            // Every transcript re-reads whole: see the kimicode repair.
            conn.execute("DELETE FROM file_state WHERE tool = 'cline'", []).map_err(sql_err)?;
            conn.execute("DELETE FROM event_rollup WHERE tool = 'cline' AND origin = ''", [])
                .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_CLINE_MESSAGE_KEY],
            )
            .map_err(sql_err)?;
        }
        // One-shot re-key of the Qoder cache-db rows: the key of a request's
        // call depended on how many rows the request had *at read time* — a
        // single-call request keyed bare `qoder#<request>`, the same request
        // grown to a second call re-keyed its first row to
        // `qoder#<request>#<message>` and billed it again under the new key
        // (2026-10-21). The key is now fixed by row order: the first call
        // claims the bare key forever, later calls carry their message id.
        // Both editions share the rule and the repair. Merged rows are
        // another machine's truth and stay (`source NOT LIKE 'linux:%'`, the
        // antigravity rule); the rollup rows refold when the re-read lands
        // (the dsh precedent).
        const REPAIR_QODER_STABLE_REQUEST_KEY: &str = "repair:qoder-stable-request-key-1";
        let qoder_key: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_QODER_STABLE_REQUEST_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if qoder_key.is_none() {
            const QODER_LOCAL_ROWS: &str =
                "(SELECT id FROM event WHERE tool IN ('qoder', 'qoder_cn') AND source NOT LIKE 'linux:%')";
            conn.execute(&format!("DELETE FROM call WHERE event_id IN ({QODER_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM quota WHERE event_id IN ({QODER_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM event WHERE id IN ({QODER_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            // Both stores re-read whole: the transcript tree and the cache db
            // re-emit under keys no past pass wrote for these files.
            conn.execute("DELETE FROM file_state WHERE tool IN ('qoder', 'qoder_cn')", [])
                .map_err(sql_err)?;
            conn.execute(
                "DELETE FROM event_rollup WHERE tool IN ('qoder', 'qoder_cn') AND origin = ''",
                [],
            )
            .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_QODER_STABLE_REQUEST_KEY],
            )
            .map_err(sql_err)?;
        }
        // One-shot re-key of the CatPaw rows without a message id: they
        // emitted keyless, and the index bills every keyless event — the
        // renamed store's `updated_at_ms` cursor re-reads a re-touched row,
        // so an id-less turn re-billed on every pass that saw it (2026-10-21;
        // the legacy store's rowid cursor only ever read such a row once, but
        // keyless rows cannot be told from duplicates after the fact). Id-less
        // rows now key on their immutable rowid. Merged rows are another
        // machine's truth and stay (`source NOT LIKE 'linux:%'`, the
        // antigravity rule); the rollup rows refold when the re-read lands
        // (the dsh precedent).
        const REPAIR_CATPAW_ROWID_KEY: &str = "repair:catpaw-rowid-key-1";
        let catpaw_key: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_CATPAW_ROWID_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if catpaw_key.is_none() {
            const CATPAW_LOCAL_ROWS: &str =
                "(SELECT id FROM event WHERE tool = 'catpaw' AND source NOT LIKE 'linux:%')";
            conn.execute(&format!("DELETE FROM call WHERE event_id IN ({CATPAW_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM quota WHERE event_id IN ({CATPAW_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM event WHERE id IN ({CATPAW_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            // Both stores re-read whole: the id-less rows re-emit under keys
            // no past pass wrote for these files.
            conn.execute("DELETE FROM file_state WHERE tool = 'catpaw'", []).map_err(sql_err)?;
            conn.execute("DELETE FROM event_rollup WHERE tool = 'catpaw' AND origin = ''", [])
                .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_CATPAW_ROWID_KEY],
            )
            .map_err(sql_err)?;
        }
        // One-shot re-key of the Trae events: the adapter stamped every event's
        // session with the turn id, and the panel folds 会话 by that key — one
        // chat conversation read as N sessions, each one turn (3 messages of a
        // single Trae CN chat showed as "3 sessions" on 2026-10-11; the
        // international store carries the same shape, 2 turns / 1 session).
        // The rows must go, not update: the turn→session mapping lives in the
        // encrypted store, and the re-read re-emits under the SAME dedupe keys,
        // which INSERT OR IGNORE would keep ignoring while the old rows stand.
        // `file_state` goes with them — an unchanged store skips its read, and
        // Trae idle for days would leave the tools empty that whole time. The
        // rollup rows for the two tools refold when the re-read lands (the dsh
        // precedent); merged rows are another machine's truth and stay
        // (`source NOT LIKE 'linux:%'`, the antigravity rule).
        const REPAIR_TRAE_TURN_SESSION: &str = "repair:trae-turn-session-1";
        let trae_key: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_TRAE_TURN_SESSION],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if trae_key.is_none() {
            const TRAE_LOCAL_ROWS: &str = "(SELECT id FROM event \
                 WHERE tool IN ('trae', 'trae_cn') AND source NOT LIKE 'linux:%')";
            conn.execute(&format!("DELETE FROM call WHERE event_id IN ({TRAE_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM quota WHERE event_id IN ({TRAE_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute(&format!("DELETE FROM event WHERE id IN ({TRAE_LOCAL_ROWS})"), [])
                .map_err(sql_err)?;
            conn.execute("DELETE FROM file_state WHERE tool IN ('trae', 'trae_cn')", [])
                .map_err(sql_err)?;
            conn.execute(
                "DELETE FROM event_rollup WHERE tool IN ('trae', 'trae_cn') AND origin = ''",
                [],
            )
            .map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_TRAE_TURN_SESSION],
            )
            .map_err(sql_err)?;
        }
        // The former WorkBuddy adapter walked both editions under one id.
        // Re-home the domestic store's local rows and replay its cursors: an
        // identical call id in the two stores may have lost a row to dedupe.
        // Sync imports and every other tool stay intact. This repair is atomic
        // and does not trigger a workspace-wide re-ingest.
        const REPAIR_WORKBUDDY_SPLIT: &str = "repair:workbuddy-split-1";
        let buddy_split: Option<String> = conn.query_row(
            "SELECT value FROM meta WHERE key = ?1", params![REPAIR_WORKBUDDY_SPLIT], |r| r.get(0),
        ).optional().map_err(sql_err)?;
        if buddy_split.is_none() {
            let tx = conn.unchecked_transaction().map_err(sql_err)?;
            const CN_STORE: &str = "source NOT LIKE 'linux:%' AND instr(replace(source, char(92), '/'), '/.workbuddy/') > 0";
            let twins = format!(
                "SELECT id FROM event e WHERE e.tool = 'workbuddy' AND {CN_STORE} AND EXISTS \
                 (SELECT 1 FROM event t WHERE t.tool = 'workbuddy_cn' \
                  AND t.dedupe_key = 'workbuddy_cn#' || substr(e.dedupe_key, 11))"
            );
            tx.execute(&format!("DELETE FROM call WHERE event_id IN ({twins})"), []).map_err(sql_err)?;
            tx.execute(&format!("DELETE FROM quota WHERE event_id IN ({twins})"), []).map_err(sql_err)?;
            let removed = tx.execute(&format!("DELETE FROM event WHERE id IN ({twins})"), []).map_err(sql_err)?;
            let moved = tx.execute(&format!(
                "UPDATE event SET tool = 'workbuddy_cn', dedupe_key = 'workbuddy_cn#' || substr(dedupe_key, 11) \
                 WHERE tool = 'workbuddy' AND {CN_STORE}"
            ), []).map_err(sql_err)?;
            tx.execute(
                "DELETE FROM file_state WHERE tool = 'workbuddy' \
                 AND source_key NOT LIKE 'linux:%' \
                 AND instr(replace(source_key, char(92), '/'), '/.workbuddy/') > 0", [],
            ).map_err(sql_err)?;
            // The lazy rebuild detects an empty rollup, not missing groups.
            // Rebuild the entire cache only when events actually moved.
            if moved + removed > 0 { tx.execute("DELETE FROM event_rollup", []).map_err(sql_err)?; }
            tx.execute("INSERT INTO meta(key, value) VALUES(?1, '1')", params![REPAIR_WORKBUDDY_SPLIT]).map_err(sql_err)?;
            tx.commit().map_err(sql_err)?;
        }
        // Older Qoder discovery could pick the CN store as its fallback.
        // Move only identifiable local CN sources; replay their cursors to
        // recover requests that collided with the international namespace.
        const REPAIR_QODER_SPLIT: &str = "repair:qoder-split-1";
        let qoder_split: Option<String> = conn.query_row(
            "SELECT value FROM meta WHERE key = ?1", params![REPAIR_QODER_SPLIT], |r| r.get(0),
        ).optional().map_err(sql_err)?;
        if qoder_split.is_none() {
            let tx = conn.unchecked_transaction().map_err(sql_err)?;
            let cn_path = |col: &str| format!(
                "{col} NOT LIKE 'linux:%' AND (instr(lower(replace({col}, char(92), '/')), '/.qoder-cn/') > 0 \
                 OR instr(lower(replace({col}, char(92), '/')), '/qodercn/') > 0 \
                 OR instr(lower(replace({col}, char(92), '/')), '/qoder cn/') > 0 \
                 OR instr(lower(replace({col}, char(92), '/')), '/qoder cn ide/') > 0)"
            );
            const CN_KEY: &str = "CASE WHEN substr(dedupe_key, 1, 6) = 'qoder#' \
                THEN 'qoder_cn#' || substr(dedupe_key, 7) ELSE 'qoder_cn#' || dedupe_key END";
            let twins = format!(
                "SELECT e.id FROM event e WHERE e.tool = 'qoder' AND {} AND EXISTS \
                 (SELECT 1 FROM event t WHERE t.tool = 'qoder_cn' AND t.dedupe_key = {})",
                cn_path("e.source"), CN_KEY.replace("dedupe_key", "e.dedupe_key")
            );
            tx.execute(&format!("DELETE FROM call WHERE event_id IN ({twins})"), []).map_err(sql_err)?;
            tx.execute(&format!("DELETE FROM quota WHERE event_id IN ({twins})"), []).map_err(sql_err)?;
            let removed = tx.execute(&format!("DELETE FROM event WHERE id IN ({twins})"), []).map_err(sql_err)?;
            let moved = tx.execute(&format!(
                "UPDATE event SET tool = 'qoder_cn', dedupe_key = {CN_KEY} WHERE tool = 'qoder' AND {}", cn_path("source")
            ), []).map_err(sql_err)?;
            tx.execute(&format!("DELETE FROM file_state WHERE tool = 'qoder' AND {}", cn_path("source_key")), []).map_err(sql_err)?;
            if moved + removed > 0 { tx.execute("DELETE FROM event_rollup", []).map_err(sql_err)?; }
            tx.execute("INSERT INTO meta(key, value) VALUES(?1, '1')", params![REPAIR_QODER_SPLIT]).map_err(sql_err)?;
            tx.commit().map_err(sql_err)?;
        }
        // One-shot re-homing for Linux-synced dedupe keys: previously, keyed
        // events imported from a Linux collector kept their raw bare key
        // (`trae#1`, `1`, etc.), causing cross-server collisions on the unique
        // `event_dedupe` index. Scoping them with `linux:<origin>:<key>` ensures
        // multiple Linux servers coexist idempotently without clobbering each other.
        const REPAIR_LINUX_SCOPED_KEYS: &str = "repair:linux-scoped-keys-1";
        let scoped_keys: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![REPAIR_LINUX_SCOPED_KEYS],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if scoped_keys.is_none() {
            conn.execute(
                &format!(
                    "UPDATE event SET dedupe_key = 'linux:' || ({ORIGIN_SQL}) || ':' || dedupe_key \
                     WHERE ({ORIGIN_SQL}) <> '' \
                       AND dedupe_key IS NOT NULL \
                       AND dedupe_key NOT LIKE 'linux:%' \
                       AND dedupe_key NOT LIKE 'mix:%'"
                ),
                [],
            )
            .map_err(sql_err)?;
            conn.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')",
                params![REPAIR_LINUX_SCOPED_KEYS],
            )
            .map_err(sql_err)?;
        }
        let max_workers = std::thread::available_parallelism()
            .map(|n| n.get().clamp(1, 16))
            .unwrap_or(4);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let claim_token = format!("{:x}-{:x}", std::process::id(), nanos);
        Ok(Self { conn, path, max_workers, errors: Vec::new(), claim_token, note_gate: std::collections::HashMap::new() })
    }

    /// Caps the read pool; a menu-bar process wants a small number even on a
    /// many-core machine. Values below 1 are clamped up.
    pub fn set_max_workers(&mut self, n: usize) {
        self.max_workers = n.max(1);
    }

    pub fn max_workers(&self) -> usize {
        self.max_workers
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Creates the durable sidecar which makes all later index maintenance
    /// fail closed if SQLite itself becomes unreadable.
    pub fn mark_device_sync_state(&self) -> Result<()> {
        let Some(path) = self.path.as_deref() else { return Ok(()) };
        let marker = sync_state_marker_path(path);
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        match std::fs::symlink_metadata(&marker) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                return Err(Error::Sync(format!("device-sync marker is not a regular file: {}", marker.display())));
            }
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&marker, e)),
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&marker) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = std::fs::symlink_metadata(&marker).map_err(|e| Error::io(&marker, e))?;
                if meta.file_type().is_symlink() || !meta.is_file() {
                    return Err(Error::Sync(format!("device-sync marker is not a regular file: {}", marker.display())));
                }
                return Ok(())
            }
            Err(error) => return Err(Error::io(&marker, error)),
        };
        use std::io::Write;
        file.write_all(b"tokenme-device-sync-state-v1\n").map_err(|e| Error::io(&marker, e))?;
        file.sync_all().map_err(|e| Error::io(&marker, e))?;
        #[cfg(unix)]
        if let Some(parent) = marker.parent() {
            std::fs::File::open(parent).and_then(|directory| directory.sync_all()).map_err(|e| Error::io(parent, e))?;
        }
        Ok(())
    }

    /// Per-file failures from the last ingest/rebuild pass. One bad file must
    /// never abort the pass, so they surface here instead of as `Err`.
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    fn has_device_sync_state(&self) -> bool {
        self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name IN ('device_sync_meta', 'device_registry', 'device_sync_secondary_config'))",
            [], |row| row.get::<_, bool>(0),
        ).unwrap_or(false)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Adjusts this connection's lock wait without changing other Index
    /// connections. Device sync uses a short budget so UI actions fail fast;
    /// foreground ingest keeps its normal five-second budget.
    pub fn set_busy_timeout_ms(&self, timeout_ms: i64) -> Result<()> {
        if timeout_ms < 0 { return Err(Error::Sqlite("busy timeout must be non-negative".into())); }
        self.conn
            .execute_batch(&format!("PRAGMA busy_timeout={timeout_ms};"))
            .map_err(sql_err)
    }

    pub fn with_network_transaction<T, E, F>(&mut self, f: F) -> std::result::Result<T, E>
    where
        E: From<Error>,
        F: FnOnce(&rusqlite::Transaction<'_>) -> std::result::Result<T, E>,
    {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| E::from(sql_err(e)))?;
        match f(&tx) {
            Ok(value) => tx.commit().map(|()| value).map_err(|e| E::from(sql_err(e))),
            Err(error) => {
                let _ = tx.rollback();
                Err(error)
            }
        }
    }

    pub fn meta_value(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .map_err(sql_err)
    }

    // ---- ingestion ------------------------------------------------------

    /// Ingest with the claim policy spelled out: the app's timer uses the
    /// defaults (skip when a peer holds the lease), a manual refresh may force.
    pub fn ingest_with(
        &mut self,
        adapters: &[Box<dyn SourceAdapter>],
        filter: &DateFilter,
        opts: &IngestOptions,
    ) -> Result<IngestReport> {
        ingest::run(self, adapters.iter().map(|a| &**a), filter, opts, false)
    }

    pub fn ingest_adapter(
        &mut self,
        adapter: &dyn SourceAdapter,
        filter: &DateFilter,
    ) -> Result<IngestReport> {
        self.ingest_adapter_with(adapter, filter, &IngestOptions::default())
    }

    pub fn ingest_adapter_with(
        &mut self,
        adapter: &dyn SourceAdapter,
        filter: &DateFilter,
        opts: &IngestOptions,
    ) -> Result<IngestReport> {
        ingest::run(self, std::iter::once(adapter), filter, opts, false)
    }

    /// Drops local source rows and re-reads the retention window. Remote
    /// device-sync rows and their receipts are retained across a rebuild.
    /// The reset sits behind the same lease as an incremental pass.
    pub fn rebuild(&mut self, adapters: &[Box<dyn SourceAdapter>]) -> Result<IngestReport> {
        self.rebuild_with(adapters, false)
    }

    /// `force` is the escape hatch for a wedged lease on the manual path.
    pub fn rebuild_with(
        &mut self,
        adapters: &[Box<dyn SourceAdapter>],
        force: bool,
    ) -> Result<IngestReport> {
        let opts = IngestOptions { force, claim_wait: std::time::Duration::from_secs(5) };
        let filter = DateFilter::new(Some(retention_cutoff(now_ms())), None);
        let adapters = adapters.iter().map(|a| &**a);
        ingest::run(self, adapters, &filter, &opts, true)
    }

    pub(crate) fn clear_local_for_rebuild(&mut self) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let local_ids = format!("SELECT id FROM event WHERE ({ORIGIN_SQL}) = ''");
        tx.execute(&format!("DELETE FROM call WHERE event_id IN ({local_ids})"), []).map_err(sql_err)?;
        tx.execute(&format!("DELETE FROM quota WHERE event_id IN ({local_ids})"), []).map_err(sql_err)?;
        tx.execute(&format!("DELETE FROM event WHERE ({ORIGIN_SQL}) = ''"), []).map_err(sql_err)?;
        // The ledger is a cache of what `event` already billed: with the local
        // rows gone for the re-read, a surviving ledger would make that re-read
        // compute a zero growth and rebuild the tool empty.
        tx.execute(&format!("DELETE FROM cumulative_state WHERE ({ORIGIN_SQL}) = ''"), [])
            .map_err(sql_err)?;
        tx.execute("DELETE FROM file_state", []).map_err(sql_err)?;
        tx.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
        tx.commit().map_err(sql_err)
    }

    /// Empties the data tables but keeps the schema and `index_id`.
    pub fn clear(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                "DELETE FROM event; DELETE FROM call; DELETE FROM quota; \
                 DELETE FROM file_state; DELETE FROM event_rollup; \
                 DELETE FROM cumulative_state;",
            )
            .map_err(sql_err)?;
        Ok(())
    }

    // ---- queries --------------------------------------------------------

    pub fn all_events(&self) -> Result<Vec<UsageEvent>> {
        self.fetch(None)
    }

    pub fn events_since(&self, since_ms: i64) -> Result<Vec<UsageEvent>> {
        self.fetch(Some(since_ms))
    }

    fn fetch(&self, since_ms: Option<i64>) -> Result<Vec<UsageEvent>> {
        let (event_sql, ts_filter) = match since_ms {
            Some(_) => (
                format!("SELECT {SELECT_EVENT_COLS} FROM event WHERE ts_ms >= ?1 ORDER BY id"),
                "WHERE event_id IN (SELECT id FROM event WHERE ts_ms >= ?1) ORDER BY event_id",
            ),
            None => (
                format!("SELECT {SELECT_EVENT_COLS} FROM event ORDER BY id"),
                "ORDER BY event_id",
            ),
        };
        let mut ids: Vec<i64> = Vec::new();
        let mut events: Vec<UsageEvent> = Vec::new();
        {
            let mut stmt = self.conn.prepare(&event_sql).map_err(sql_err)?;
            // event_from_row is the one decoder for SELECT_EVENT_COLS; a
            // hand-rolled copy here used to drift whenever the column list
            // changed.
            let rows = stmt
                .query_map(rusqlite::params_from_iter(since_ms), |r| {
                    Ok((r.get::<_, i64>(0)?, event_from_row(r)?))
                })
                .map_err(sql_err)?;
            for row in rows {
                let (id, ev) = row.map_err(sql_err)?;
                ids.push(id);
                events.push(ev);
            }
        }
        if events.is_empty() {
            return Ok(events);
        }

        {
            let sql = format!("SELECT event_id, kind, name FROM call {ts_filter}");
            let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
            let items = stmt
                .query_map(rusqlite::params_from_iter(since_ms), |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        Call { kind: call_kind_of(&r.get::<_, String>(1)?), name: r.get(2)? },
                    ))
                })
                .map_err(sql_err)?;
            attach(&ids, &mut events, items.map(|r| r.map_err(sql_err)), |ev, c| ev.calls.push(c))?;
        }
        {
            let sql = format!("SELECT event_id, used_percent, window_minutes, resets_at_ms, label FROM quota {ts_filter}");
            let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
            let items = stmt
                .query_map(rusqlite::params_from_iter(since_ms), |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        QuotaSample {
                            used_percent: r.get(1)?,
                            window_minutes: r.get(2)?,
                            resets_at_ms: r.get(3)?,
                            label: r.get::<_, Option<String>>(4)?,
                            id: None,
                        },
                    ))
                })
                .map_err(sql_err)?;
            attach(&ids, &mut events, items.map(|r| r.map_err(sql_err)), |ev, q| ev.quota = Some(q))?;
        }
        Ok(events)
    }

    pub fn event_count(&self) -> Result<u64> {
        self.conn
            .query_row("SELECT count(*) FROM event", [], |r| r.get::<_, i64>(0))
            .map(|n| n.max(0) as u64)
            .map_err(sql_err)
    }

    /// Whether the index holds anything at all. `event_count` scans the whole
    /// table, which is fine when the number is the answer and wrong when only
    /// "is it empty" is — the boot path asks the second question.
    pub fn has_events(&self) -> Result<bool> {
        match self
            .conn
            .query_row("SELECT 1 FROM event LIMIT 1", [], |_| Ok(()))
        {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            // A broken schema is not "empty": saying so keeps the boot publish
            // honest instead of quietly skipping it.
            Err(e) => Err(sql_err(e)),
        }
    }

    /// Event totals per tool, currently in the index.
    pub fn per_tool_counts(&self) -> Result<BTreeMap<String, u64>> {
        // The rollup carries each group's event count (`n`) maintained in the
        // same transaction as the inserts (sync merge and purge-repair both
        // keep it aligned), so per-tool totals fold ~2k rollup rows instead of
        // GROUP BY-ing ~500k events on every ingest pass — the sampled
        // hotspot of the whole panel loop.
        let mut stmt = self
            .conn
            .prepare("SELECT tool, sum(n) FROM event_rollup GROUP BY tool ORDER BY tool")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(sql_err)?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (tool, n) = row.map_err(sql_err)?;
            out.insert(tool, n.max(0) as u64);
        }
        Ok(out)
    }

    /// Newest event timestamp for one tool, if any.
    pub fn newest_ts_ms(&self, tool: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT max(ts_ms) FROM event WHERE tool = ?1",
                params![tool],
                |r| r.get::<_, Option<i64>>(0),
            )
            .map_err(sql_err)
    }

    /// One tool's events in insertion order, null tokens read as 0 exactly like
    /// [`Index::all_events`]. The narrow shape is what session analytics need;
    /// `all_events` stays the report path's full-width fetch.
    pub fn tool_events(&self, tool: &str) -> Result<Vec<ToolEventRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session, model, ts_ms, in_tok, cc_tok, cr_tok, out_tok \
                 FROM event WHERE tool = ?1 ORDER BY id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![tool], |r| {
                Ok(ToolEventRow {
                    session: r.get(0)?,
                    model: r.get(1)?,
                    ts_ms: r.get(2)?,
                    input: r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                    cache_creation: r.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                    cache_read: r.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                    output: r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
                })
            })
            .map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)
    }

    /// Per-session token totals folded from `event_rollup` — the same shape
    /// as [`Self::tool_session_totals`] at ~2k rollup rows instead of the
    /// full event table. Semantics mirror `tool_events`/`tool_session_totals`:
    /// no origin filter, so merged remote rows of the same tool count too,
    /// exactly as the event-scan version did.
    pub fn tool_session_totals_from_rollup(&self, tool: &str) -> Result<Vec<ToolSessionTotals>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session, IFNULL(sum(in_tok), 0), IFNULL(sum(cc_tok), 0), \
                        IFNULL(sum(cr_tok), 0), IFNULL(sum(out_tok), 0), sum(n) \
                 FROM event_rollup WHERE tool = ?1 GROUP BY session ORDER BY session",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![tool], |r| {
                Ok(ToolSessionTotals {
                    session: r.get(0)?,
                    input: r.get(1)?,
                    cache_creation: r.get(2)?,
                    cache_read: r.get(3)?,
                    output: r.get(4)?,
                    events: r.get::<_, i64>(5)?.max(0) as u64,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)
    }

    /// Per-session token totals for one tool, ordered by session. `IFNULL`
    /// matches `fetch`: a sum over all-NULL columns is 0, not NULL.
    pub fn tool_session_totals(&self, tool: &str) -> Result<Vec<ToolSessionTotals>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session, IFNULL(sum(in_tok), 0), IFNULL(sum(cc_tok), 0), \
                        IFNULL(sum(cr_tok), 0), IFNULL(sum(out_tok), 0), count(*) \
                 FROM event WHERE tool = ?1 GROUP BY session ORDER BY session",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![tool], |r| {
                Ok(ToolSessionTotals {
                    session: r.get(0)?,
                    input: r.get(1)?,
                    cache_creation: r.get(2)?,
                    cache_read: r.get(3)?,
                    output: r.get(4)?,
                    events: r.get::<_, i64>(5)?.max(0) as u64,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)
    }

    pub fn files_tracked(&self) -> Result<usize> {
        self.conn
            .query_row("SELECT count(*) FROM file_state", [], |r| r.get::<_, i64>(0))
            .map(|n| n.max(0) as usize)
            .map_err(sql_err)
    }

    /// Reclaims the file space a `prune` or `rebuild` left behind. `VACUUM`
    /// cannot run inside a transaction, so it is its own call: the caller
    /// decides when a few hundred ms of pause is affordable.
    pub fn vacuum(&mut self) -> Result<()> {
        self.conn.execute_batch("VACUUM;").map_err(sql_err)
    }

    /// Drops events older than `cutoff_ms` and returns how many went away.
    pub fn prune(&mut self, cutoff_ms: i64) -> Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        // One pin holds local rows against retention, and it is
        // pairing-wide: while the meta mirror `device_sync_uplink_paired`
        // reads '1' (SyncStore maintains it beside the capture switch — a
        // secondary config exists and is enabled), this machine's own
        // history is the source the uplink snapshot re-ships every pass, so
        // pruning a day here would drop it from every later snapshot and,
        // after a re-pair, from the peer too. Mirror absent or '0' — never
        // paired through a SyncStore, or unpaired/disabled — prunes
        // normally. (The row-era sync_dirty hold is gone with the journal.)
        let pruneable = format!(
            "e.ts_ms < ?1 \
             AND NOT (COALESCE((SELECT value FROM meta WHERE key = 'device_sync_uplink_paired'), '') = '1' \
                      AND ({ORIGIN_SQL}) = '')"
        );
        // Compute the doomed id set once into a temp table: evaluating
        // the predicate per side table re-ran the retention logic five times
        // over `event` in the same transaction. The temp table lives and dies
        // with this connection, so nothing leaks between passes.
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS doomed_event(id INTEGER PRIMARY KEY);
             DELETE FROM doomed_event;",
        ).map_err(sql_err)?;
        tx.execute(
            &format!("INSERT INTO doomed_event(id) SELECT e.id FROM event e WHERE {pruneable}"),
            params![cutoff_ms],
        ).map_err(sql_err)?;
        // The doomed rows' ts span decides which rollup days need recomputing;
        // read it before the deletes, inside the same transaction.
        let doomed: (Option<i64>, Option<i64>) = tx
            .query_row(
                "SELECT min(ts_ms), max(ts_ms) FROM event WHERE id IN (SELECT id FROM doomed_event)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql_err)?;
        for table in ["call", "quota"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE event_id IN (SELECT id FROM doomed_event)"),
                [],
            )
            .map_err(sql_err)?;
        }
        let removed = tx
            .execute("DELETE FROM event WHERE id IN (SELECT id FROM doomed_event)", [])
            .map_err(sql_err)?;
        // Nothing removed, nothing to fix; and absent doomed rows there is no
        // span to iterate.
        if removed > 0 {
            if let (Some(lo), Some(hi)) = doomed {
                ingest::recompute_span_tx(&tx, lo, hi)?;
            }
        }
        tx.commit().map_err(sql_err)?;
        Ok(removed as u64)
    }

    /// The query planner's own statistics refresh. It scans enough of the index
    /// that running it on every ingest pass was pure overhead; the engine calls
    /// this once per idle cadence pass instead.
    pub fn optimize(&self) {
        let _ = self.conn.execute_batch("PRAGMA optimize;");
    }

    /// Merges what the adapters can see with what is actually indexed, so the UI
    /// can tell "not installed" apart from "installed but never ingested".
    pub fn source_statuses(&self, detected: &[DetectedSource]) -> Result<Vec<SourceStatus>> {
        let counts = self.per_tool_counts()?;
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out: Vec<SourceStatus> = Vec::new();
        for d in detected {
            seen.insert(d.id.as_str());
            out.push(SourceStatus {
                id: d.id.clone(),
                display: d.display.clone(),
                detected: true,
                roots: d.roots.clone(),
                hint: d.hint.clone(),
                events_ingested: counts.get(&d.id).copied().unwrap_or(0),
            });
        }
        // Rows left over from a source that stopped being detected are still
        // spend the user paid for; report them as undetected rather than vanish.
        let known: BTreeMap<String, String> =
            out.iter().map(|s| (s.id.clone(), s.display.clone())).collect();
        for (tool, n) in counts {
            if seen.contains(tool.as_str()) {
                continue;
            }
            let display = known.get(&tool).cloned().unwrap_or_else(|| tool.clone());
            out.push(SourceStatus {
                id: tool,
                display,
                detected: false,
                roots: Vec::new(),
                hint: None,
                events_ingested: n,
            });
        }
        Ok(out)
    }
}

/// Narrow per-event row for session analytics ([`Index::tool_events`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolEventRow {
    pub session: String,
    pub model: Option<String>,
    pub ts_ms: i64,
    pub input: f64,
    pub cache_creation: f64,
    pub cache_read: f64,
    pub output: f64,
}

/// One session's summed tokens for one tool ([`Index::tool_session_totals`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSessionTotals {
    pub session: String,
    pub input: f64,
    pub cache_creation: f64,
    pub cache_read: f64,
    pub output: f64,
    pub events: u64,
}

/// `events` and `ids` are ascending by rowid and the incoming items are ordered
/// by `event_id`, so one forward pointer stitches the side table in O(n).
fn attach<T, I>(ids: &[i64], events: &mut [UsageEvent], items: I, mut apply: impl FnMut(&mut UsageEvent, T)) -> Result<()>
where
    I: Iterator<Item = std::result::Result<(i64, T), Error>>,
{
    let mut pos = 0usize;
    for item in items {
        let (id, val) = item?;
        while pos < ids.len() && ids[pos] < id {
            pos += 1;
        }
        if pos < ids.len() && ids[pos] == id {
            apply(&mut events[pos], val);
        }
    }
    Ok(())
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index").field("path", &self.path).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_index_starts_empty_and_versioned() {
        let mut idx = Index::open_in_memory().unwrap();
        assert_eq!(idx.event_count().unwrap(), 0);
        assert_eq!(idx.meta_value("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
        assert!(idx.meta_value("index_id").unwrap().is_some());
        assert_eq!(idx.conn.query_row::<i64, _, _>("PRAGMA synchronous", [], |row| row.get(0)).unwrap(), 1);
        idx.with_network_transaction(|_| Ok::<_, Error>(())).unwrap();
        assert_eq!(idx.conn.query_row::<i64, _, _>("PRAGMA synchronous", [], |row| row.get(0)).unwrap(), 1);
    }

    #[test]
    fn paired_uplink_pins_local_history_against_prune_until_unpaired() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.conn.execute_batch(
            "INSERT INTO event(tool, ts_ms, session, meter, in_tok, source, dedupe_key)
             VALUES('codex', 10, 'session', 'tokens', 1, 'local.jsonl', 'mine'),
                    ('codex', 11, 'session', 'tokens', 1, 'linux:peer:their-key', 'theirs');
             INSERT INTO meta(key, value) VALUES('device_sync_uplink_paired', '1');",
        ).unwrap();
        assert_eq!(idx.prune(2_000).unwrap(), 1,
            "while the uplink snapshot is the shared truth, this machine's own history must not age out");
        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM event WHERE dedupe_key = 'mine'", [], |row| row.get(0)).unwrap(), 1);
        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM event WHERE dedupe_key = 'theirs'", [], |row| row.get(0)).unwrap(), 0,
            "a foreign-origin copy is not the snapshot's source and ages out");
        idx.conn.execute("UPDATE meta SET value = '0' WHERE key = 'device_sync_uplink_paired'", []).unwrap();
        assert_eq!(idx.prune(2_000).unwrap(), 1, "unpaired prunes normally");
        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM event", [], |row| row.get(0)).unwrap(), 0);
    }

    #[test]
    fn null_dedupe_rows_never_collide_but_keyed_ones_do() {
        let idx = Index::open_in_memory().unwrap();
        let tx = idx.conn.unchecked_transaction().unwrap();
        for key in [Some("k"), None, None] {
            tx.execute(
                "INSERT OR IGNORE INTO event(tool, ts_ms, session, meter, dedupe_key, source) \
                 VALUES('t', 1, 's', 'tokens', ?1, '/x')",
                params![key],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        assert_eq!(idx.event_count().unwrap(), 3, "two NULLs plus one keyed row");
    }

    #[test]
    fn rebuild_preserves_remote_rows() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.conn
            .execute(
                "INSERT INTO event(tool, ts_ms, session, meter, source) VALUES('local', 1, 'local', 'tokens', '/local')",
                [],
            )
            .unwrap();
        let remote_id = idx
            .conn
            .execute(
                "INSERT INTO event(tool, ts_ms, session, meter, source) VALUES('remote', 2, 'remote', 'tokens', 'linux:secondary:row-1')",
                [],
            )
            .map(|_| idx.conn.last_insert_rowid())
            .unwrap();
        idx.conn
            .execute("INSERT INTO call(event_id, kind, name) VALUES(?1, 'mcp', 'remote-call')", params![remote_id])
            .unwrap();
        let adapters: Vec<Box<dyn SourceAdapter>> = Vec::new();
        idx.rebuild_with(&adapters, false).unwrap();

        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM event WHERE source = '/local'", [], |r| r.get(0)).unwrap(), 0);
        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM event WHERE source = 'linux:secondary:row-1'", [], |r| r.get(0)).unwrap(), 1);
        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM call WHERE event_id = ?1", params![remote_id], |r| r.get(0)).unwrap(), 1);
    }

    /// A damaged index must not wedge every future read into garbage-row errors:
    /// `verify_integrity` quarantines the file and hands back an empty one, and
    /// the next ingest rebuilds everything from the source logs. Opening alone
    /// does not do that work — opening is the panel's first-render path.
    #[test]
    fn a_corrupted_index_is_quarantined_and_reopened_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let idx = Index::open(&path).unwrap();
            idx.conn
                .execute(
                    "INSERT INTO event(tool, ts_ms, session, meter, source) VALUES('t', 1, 's', 'tokens', '/x')",
                    [],
                )
                .unwrap();
        }
        // Trash the first table root (page 2): the schema on page 1 still opens,
        // but the b-trees no longer parse. The connection above must be closed
        // first so the WAL is checkpointed into the main file.
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(4096)).unwrap();
            file.write_all(&[0xFF; 64]).unwrap();
        }
        let (idx, healed) = Index::open(&path).unwrap().verify_integrity().unwrap();
        assert!(healed, "verify reported that it replaced the image");
        assert_eq!(idx.event_count().unwrap(), 0, "the damaged index was replaced, not reused");
        let moved: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".corrupt-"))
            .collect();
        assert!(
            moved.iter().any(|n| n.starts_with("index.db.corrupt-")),
            "the damaged file is kept for forensics: {moved:?}"
        );
        assert!(
            !moved.is_empty(),
            "the damaged image or its sidecars must be kept for forensics: {moved:?}"
        );
    }

    /// A sync marker must not turn an unreadable image into an endless panel
    /// boot. The foreground recovery path keeps the original image and starts
    /// a clean local index; the marker remains so the sync layer can require a
    /// deliberate re-pair/recovery decision instead of silently trusting it.
    #[test]
    fn a_corrupted_sync_index_is_preserved_and_reopened_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let idx = Index::open(&path).unwrap();
            idx.conn
                .execute(
                    "INSERT INTO event(tool, ts_ms, session, meter, source) VALUES('t', 1, 's', 'tokens', '/x')",
                    [],
                )
                .unwrap();
            idx.conn.execute_batch("PRAGMA writable_schema=ON; INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql) VALUES('table', 'workbuddy_cn', 'workbuddy_cn', 0, 'CREATE TABLE workbuddy_cn(');").unwrap();
        }
        std::fs::write(sync_state_marker_path(&path), b"tokenme-device-sync-state-v1\n").unwrap();

        let (idx, recovered) = Index::open_recovering_corruption(&path).unwrap();
        assert!(recovered, "the malformed schema must take the recovery path");
        assert_eq!(idx.event_count().unwrap(), 0, "the replacement starts empty");
        assert!(sync_state_marker_path(&path).exists(), "the sync marker is retained");
        let moved: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".corrupt-"))
            .collect();
        assert!(moved.iter().any(|n| n.starts_with("index.db.corrupt-")), "the original image is preserved: {moved:?}");
    }

    /// The common case: a healthy index verifies without touching the file, so
    /// the guard costs nothing but the read it has to do.
    #[test]
    fn sync_connection_busy_timeout_can_be_lowered_without_changing_the_index_api() {
        let idx = Index::open_in_memory().unwrap();
        idx.set_busy_timeout_ms(500).unwrap();
        let timeout: i64 = idx.conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0)).unwrap();
        assert_eq!(timeout, 500);
    }

    #[test]
    fn a_healthy_index_verifies_without_replacing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let idx = Index::open(&path).unwrap();
            idx.conn
                .execute(
                    "INSERT INTO event(tool, ts_ms, session, meter, source) VALUES('t', 1, 's', 'tokens', '/x')",
                    [],
                )
                .unwrap();
        }
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let (idx, healed) = Index::open(&path).unwrap().verify_integrity().unwrap();
        assert!(!healed, "a clean image is left alone");
        assert_eq!(idx.event_count().unwrap(), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
        assert_eq!(idx.path(), Some(path.as_path()));
    }
}

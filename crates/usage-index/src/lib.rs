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
const SCHEMA_VERSION: &str = "4";

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
-- Serves the per-session correlated lookups (first project / last model) and
-- the newest-event probe without a full scan.
CREATE INDEX IF NOT EXISTS event_tool_session ON event(tool, session);
-- Keys: `schema_version`, `index_id`, `last_ingest_ms`, and `index_claim` — the
-- single-row ingest lease that keeps two processes off the same cursors.
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
"#;

pub(crate) const INSERT_EVENT: &str = "INSERT OR IGNORE INTO event(\
    tool, ts_ms, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, \
    reason_tok, credits, dedupe_key, source) \
    VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)";

const SELECT_EVENT_COLS: &str = "id, tool, ts_ms, session, project, model, meter, in_tok, cc_tok, \
    cr_tok, out_tok, reason_tok, credits, dedupe_key, source";

pub(crate) fn sql_err(e: rusqlite::Error) -> Error {
    Error::Sqlite(e.to_string())
}

/// The origin of a row's `source`, in SQL. The Rust twin is
/// [`usage_core::origin_of`]; both must agree. Interpreted: everything a
/// bundle merge wrote (`linux:<origin>:<their key>` — see `sync.rs`) shadows
/// under its origin, and everything else is this machine (`''`). A `linux:`
/// without a second colon was never written by a merge, so it stays local
/// rather than inventing an origin.
pub(crate) const ORIGIN_SQL: &str = "\
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

pub(crate) fn now_ms() -> i64 {
    usage_core::report::now_ms()
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
        match Self::open_connection(&path) {
            Ok(index) => Ok(index),
            Err(err) if err.is_corruption() => {
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

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| Error::Sqlite(e.to_string()))?;
        Self::setup(conn, None)
    }

    fn open_connection(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|e| Error::io(path, std::io::Error::other(e)))?;
        Self::setup(conn, Some(path.to_path_buf()))
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
        let Some(path) = self.path().map(Path::to_path_buf) else {
            return Err(Error::Sqlite("in-memory index failed quick_check".into()));
        };
        let moved = quarantine(&path);
        // Close the handle on the image that was just moved aside before opening
        // the fresh one: a live connection keeps reading through the renamed
        // file, and on Windows it would hold the sidecars open behind the rebuild.
        drop(self);
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
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
        )
        .map_err(sql_err)?;
        drop_legacy_rollup(&conn)?;
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
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
        let max_workers = std::thread::available_parallelism()
            .map(|n| n.get().clamp(1, 16))
            .unwrap_or(4);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let claim_token = format!("{:x}-{:x}", std::process::id(), nanos);
        Ok(Self { conn, path, max_workers, errors: Vec::new(), claim_token })
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

    /// Per-file failures from the last ingest/rebuild pass. One bad file must
    /// never abort the pass, so they surface here instead of as `Err`.
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn meta_value(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .map_err(sql_err)
    }

    // ---- ingestion ------------------------------------------------------

    pub fn ingest(
        &mut self,
        adapters: &[Box<dyn SourceAdapter>],
        filter: &DateFilter,
    ) -> Result<IngestReport> {
        self.ingest_with(adapters, filter, &IngestOptions::default())
    }

    /// `ingest` with the claim policy spelled out: the app's timer uses the
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

    /// Drops everything and re-reads the retention window. Correct after a
    /// semantics change, a schema bump, or when the totals stop trusting you.
    /// The wipe sits behind the same lease as an incremental pass.
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

    /// Empties the data tables but keeps the schema and `index_id`.
    pub fn clear(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                "DELETE FROM event; DELETE FROM call; DELETE FROM quota; \
                 DELETE FROM file_state; DELETE FROM event_rollup;",
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
            let rows = stmt
                .query_map(rusqlite::params_from_iter(since_ms), |r| {
                    let id: i64 = r.get(0)?;
                    let counts = TokenCounts {
                        input: r.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
                        cache_creation: r.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
                        cache_read: r.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
                        output: r.get::<_, Option<f64>>(10)?.unwrap_or(0.0),
                        reasoning: r.get::<_, Option<f64>>(11)?.unwrap_or(0.0),
                        credits: r.get::<_, Option<f64>>(12)?.unwrap_or(0.0),
                    };
                    let ev = UsageEvent {
                        tool: r.get(1)?,
                        ts_ms: r.get(2)?,
                        session: r.get(3)?,
                        project: r.get(4)?,
                        model: r.get(5)?,
                        counts,
                        meter: meter_of(&r.get::<_, String>(6)?),
                        dedupe_key: r.get(13)?,
                        calls: Vec::new(),
                        quota: None,
                        source: r.get(14)?,
                    };
                    Ok((id, ev))
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

    /// Event totals per tool, currently in the index.
    pub fn per_tool_counts(&self) -> Result<BTreeMap<String, u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tool, count(*) FROM event GROUP BY tool ORDER BY tool")
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
        // The doomed rows' ts span decides which rollup days need recomputing;
        // read it before the deletes, inside the same transaction.
        let doomed: (Option<i64>, Option<i64>) = tx
            .query_row(
                "SELECT min(ts_ms), max(ts_ms) FROM event WHERE ts_ms < ?1",
                params![cutoff_ms],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM call WHERE event_id IN (SELECT id FROM event WHERE ts_ms < ?1)",
            params![cutoff_ms],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM quota WHERE event_id IN (SELECT id FROM event WHERE ts_ms < ?1)",
            params![cutoff_ms],
        )
        .map_err(sql_err)?;
        let removed =
            tx.execute("DELETE FROM event WHERE ts_ms < ?1", params![cutoff_ms]).map_err(sql_err)?;
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
        let idx = Index::open_in_memory().unwrap();
        assert_eq!(idx.event_count().unwrap(), 0);
        assert_eq!(idx.meta_value("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
        assert!(idx.meta_value("index_id").unwrap().is_some());
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
        assert_eq!(
            moved.len(),
            3,
            "the image goes with its own -wal and -shm: a rebuild must not inherit the \
             write-ahead log of the file that just failed its check: {moved:?}"
        );
    }

    /// The common case: a healthy index verifies without touching the file, so
    /// the guard costs nothing but the read it has to do.
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

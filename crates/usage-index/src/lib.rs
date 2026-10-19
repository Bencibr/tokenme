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

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};

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

#[derive(Debug, Clone)]
pub struct SyncDirtyEvent {
    pub identity: String,
    pub occurrence_id: Option<String>,
    pub event: UsageEvent,
    pub revision: u64,
}

#[derive(Debug, Clone)]
pub struct SyncSnapshotEvent {
    pub identity: String,
    pub occurrence_id: Option<String>,
    pub event: UsageEvent,
}

#[derive(Debug, Clone)]
pub struct SyncSnapshotPage {
    pub coverage_lo_ms: i64,
    pub coverage_hi_ms: i64,
    pub events: Vec<SyncSnapshotEvent>,
    pub dirty_events: Vec<SyncDirtyEvent>,
    pub next_identity: Option<String>,
    pub next_dirty_revision: u64,
    pub events_done: bool,
    pub dirty_done: bool,
}

pub const SYNC_DIRTY_PAGE_SIZE: usize = 256;
pub const SYNC_SNAPSHOT_MAX_EVENTS: usize = 500_000;
pub const SYNC_SNAPSHOT_MAX_DIRTY: usize = 65_536;
pub const SYNC_SNAPSHOT_PAGE_ROWS: usize = 256;
pub const SYNC_SNAPSHOT_PAGE_BYTES: usize = 8 * 1024 * 1024;
pub const SYNC_SNAPSHOT_MAX_ROW_BYTES: usize = 1024 * 1024;
pub const SYNC_DIRTY_MAX_BYTES: usize = 512 * 1024 * 1024;

/// A read-consistent export view. Event pages and dirty pages are decoded from
/// the same SQLite read transaction, so a package cannot observe the event
/// before its calls/labels while the journal observes a later revision.
#[derive(Debug, Clone)]
pub struct SyncSnapshot {
    pub coverage_lo_ms: i64,
    pub coverage_hi_ms: i64,
    pub events: Vec<UsageEvent>,
    pub dirty_events: Vec<SyncDirtyEvent>,
}

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
-- Serves the per-session correlated lookups (first project / last model) and
-- the newest-event probe without a full scan.
CREATE INDEX IF NOT EXISTS event_tool_session ON event(tool, session);
-- Keys: `schema_version`, `index_id`, `last_ingest_ms`, and `index_claim` — the
-- single-row ingest lease that keeps two processes off the same cursors.
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
-- Additive device-sync state. These rows are independent of SQLite rowid, so
-- rebuilds can recreate event rows without changing an occurrence identity.
CREATE TABLE IF NOT EXISTS sync_occurrence(
    source_key   TEXT NOT NULL,
    base_hash    BLOB NOT NULL,
    ordinal      INTEGER NOT NULL,
    occurrence_id TEXT NOT NULL UNIQUE,
    state_hash   BLOB NOT NULL,
    updated_ms   INTEGER NOT NULL,
    PRIMARY KEY(source_key, base_hash, ordinal)
);
CREATE INDEX IF NOT EXISTS sync_occurrence_lookup ON sync_occurrence(source_key, base_hash, ordinal);
CREATE TABLE IF NOT EXISTS sync_event_occurrence(
    event_id      INTEGER PRIMARY KEY,
    occurrence_id TEXT NOT NULL UNIQUE
);
-- The page cursor must be an indexed protocol identity, not a computed CASE
-- expression over the whole event table.  This is intentionally separate from
-- the occurrence table: keyed events also need a durable, rebuild-independent
-- cursor row, while keyless events retain their occurrence mapping here.
CREATE TABLE IF NOT EXISTS sync_event_identity(
    event_id      INTEGER PRIMARY KEY,
    identity      TEXT NOT NULL UNIQUE,
    occurrence_id TEXT
);
CREATE INDEX IF NOT EXISTS sync_event_identity_cursor ON sync_event_identity(identity, event_id);
CREATE TABLE IF NOT EXISTS sync_dirty(
    identity       TEXT PRIMARY KEY,
    source_key     TEXT NOT NULL,
    occurrence_id  TEXT,
    event_json     BLOB NOT NULL,
    event_id       INTEGER,
    event_ts_ms    INTEGER NOT NULL,
    revision       INTEGER NOT NULL DEFAULT 1,
    dirty_since_ms INTEGER NOT NULL,
    updated_ms     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sync_dirty_source ON sync_dirty(source_key, event_ts_ms);
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

fn sync_state_marker_path(path: &Path) -> PathBuf {
    path.with_extension("device-sync-state")
}

fn sync_state_marker_exists(path: &Path) -> bool {
    sync_state_marker_path(path).exists()
}

pub(crate) fn now_ms() -> i64 {
    usage_core::report::now_ms()
}

fn sync_append_part(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u64).to_be_bytes());
    out.extend_from_slice(value);
}

fn sync_identity_bytes(domain: &[u8], fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(domain.len() + fields.iter().map(|field| field.len() + 8).sum::<usize>() + 8);
    sync_append_part(&mut out, domain);
    for field in fields { sync_append_part(&mut out, field); }
    out
}

fn sync_optional_bytes(value: Option<&str>) -> Vec<u8> {
    match value {
        Some(value) => {
            let mut out = vec![1];
            sync_append_part(&mut out, value.as_bytes());
            out
        }
        None => vec![0],
    }
}

pub(crate) fn sync_base_hash(source_key: &str, event: &UsageEvent) -> [u8; 32] {
    let ts = event.ts_ms.to_be_bytes();
    let project = sync_optional_bytes(event.project.as_deref());
    let model = sync_optional_bytes(event.model.as_deref());
    Sha256::digest(sync_identity_bytes(b"tokenme-index/sync-occurrence-base/v1", &[
        source_key.as_bytes(), event.tool.as_bytes(), &ts, event.session.as_bytes(),
        &project, &model, meter_str(event.meter).as_bytes(),
    ])).into()
}

pub(crate) fn sync_state_hash(event: &UsageEvent) -> [u8; 32] {
    let mut counts = Vec::with_capacity(6 * 8);
    for value in [
        event.counts.input, event.counts.cache_creation, event.counts.cache_read,
        event.counts.output, event.counts.reasoning, event.counts.credits,
    ] { counts.extend_from_slice(&value.to_bits().to_be_bytes()); }
    let mut calls = event.calls.iter().map(|call| {
        let kind = call_kind_str(call.kind);
        (kind.as_bytes().to_vec(), call.name.as_bytes().to_vec())
    }).collect::<Vec<_>>();
    calls.sort();
    let mut call_bytes = Vec::new();
    for (kind, name) in calls {
        sync_append_part(&mut call_bytes, &kind);
        sync_append_part(&mut call_bytes, &name);
    }
    let mut quota = Vec::new();
    if let Some(sample) = event.quota.as_ref() {
        quota.push(1);
        quota.extend_from_slice(&sample.used_percent.to_bits().to_be_bytes());
        quota.extend_from_slice(&sample.window_minutes.to_be_bytes());
        quota.extend_from_slice(&sample.resets_at_ms.to_be_bytes());
        let label = sync_optional_bytes(sample.label.as_deref());
        sync_append_part(&mut quota, &label);
    } else {
        quota.push(0);
    }
    Sha256::digest(sync_identity_bytes(b"tokenme-index/sync-occurrence-state/v1", &[
        &counts, &call_bytes, &quota,
    ])).into()
}

pub(crate) fn sync_occurrence_for_ingest(
    tx: &Transaction<'_>, source_key: &str, event: &UsageEvent, full_rescan: bool,
    used: &mut BTreeMap<[u8; 32], BTreeSet<i64>>, updated_ms: i64,
) -> Result<String> {
    let base_hash = sync_base_hash(source_key, event);
    let state_hash = sync_state_hash(event);
    let used_ordinals = used.entry(base_hash).or_default();
    let rows: Vec<(i64, Vec<u8>, String)> = {
        let mut stmt = tx.prepare(
            "SELECT ordinal, state_hash, occurrence_id FROM sync_occurrence
              WHERE source_key = ?1 AND base_hash = ?2 ORDER BY ordinal",
        ).map_err(sql_err)?;
        let rows = stmt.query_map(params![source_key, base_hash.as_slice()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        }).map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?
    };
    let mapped: BTreeSet<String> = {
        let mut stmt = tx.prepare(
            "SELECT occurrence_id FROM sync_event_occurrence WHERE occurrence_id IN
             (SELECT occurrence_id FROM sync_occurrence WHERE source_key = ?1 AND base_hash = ?2)",
        ).map_err(sql_err)?;
        let rows = stmt.query_map(params![source_key, base_hash.as_slice()], |row| row.get::<_, String>(0)).map_err(sql_err)?;
        rows.collect::<std::result::Result<BTreeSet<_>, _>>().map_err(sql_err)?
    };
    let selected = if full_rescan {
        rows.iter().find(|(ordinal, stored, occurrence_id)| !used_ordinals.contains(ordinal) && !mapped.contains(occurrence_id) && stored.as_slice() == state_hash.as_slice())
            .or_else(|| rows.iter().find(|(ordinal, _, occurrence_id)| !used_ordinals.contains(ordinal) && !mapped.contains(occurrence_id)))
            .map(|(ordinal, _, occurrence_id)| (*ordinal, occurrence_id.clone()))
    } else {
        None
    };
    let (ordinal, occurrence_id) = if let Some(selected) = selected {
        selected
    } else {
        let ordinal = rows.iter().map(|(ordinal, _, _)| *ordinal).max().unwrap_or(-1)
            .checked_add(1).ok_or_else(|| Error::Sync("keyless occurrence ordinal overflow".into()))?;
        let occurrence_id = format!(
            "tokenme-occ-v2:{}:{}",
            hex_bytes(&Sha256::digest(sync_identity_bytes(b"tokenme-index/sync-occurrence-id/v1", &[source_key.as_bytes(), &base_hash]))),
            ordinal,
        );
        tx.execute(
            "INSERT INTO sync_occurrence(source_key, base_hash, ordinal, occurrence_id, state_hash, updated_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![source_key, base_hash.as_slice(), ordinal, occurrence_id, state_hash.as_slice(), updated_ms],
        ).map_err(sql_err)?;
        (ordinal, occurrence_id)
    };
    used_ordinals.insert(ordinal);
    tx.execute(
        "UPDATE sync_occurrence SET state_hash = ?1, updated_ms = ?2 WHERE source_key = ?3 AND base_hash = ?4 AND ordinal = ?5",
        params![state_hash.as_slice(), updated_ms, source_key, base_hash.as_slice(), ordinal],
    ).map_err(sql_err)?;
    Ok(occurrence_id)
}

type SyncOccurrenceRows = Vec<(i64, Vec<u8>, String)>;

#[derive(Default)]
struct SyncOccurrencePageCache {
    rows: BTreeMap<(String, [u8; 32]), SyncOccurrenceRows>,
    mapped: BTreeMap<(String, [u8; 32]), BTreeSet<String>>,
}

/// Historical identity initialization processes a page in one transaction.
/// Cache the two stable occurrence lookups for that page; live ingest keeps
/// the uncached helper above because its transaction scope is already small.
fn sync_occurrence_for_initialization(
    tx: &Transaction<'_>, source_key: &str, event: &UsageEvent,
    used: &mut BTreeMap<[u8; 32], BTreeSet<i64>>,
    cache: &mut SyncOccurrencePageCache, updated_ms: i64,
) -> Result<String> {
    let base_hash = sync_base_hash(source_key, event);
    let cache_key = (source_key.to_string(), base_hash);
    if !cache.rows.contains_key(&cache_key) {
        let mut stmt = tx.prepare(
            "SELECT ordinal, state_hash, occurrence_id FROM sync_occurrence
              WHERE source_key = ?1 AND base_hash = ?2 ORDER BY ordinal",
        ).map_err(sql_err)?;
        let rows = stmt.query_map(params![source_key, cache_key.1.as_slice()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        }).map_err(sql_err)?;
        cache.rows.insert(cache_key.clone(), rows.collect::<std::result::Result<_, _>>().map_err(sql_err)?);
    }
    if !cache.mapped.contains_key(&cache_key) {
        let mut stmt = tx.prepare(
            "SELECT occurrence_id FROM sync_event_occurrence WHERE occurrence_id IN
             (SELECT occurrence_id FROM sync_occurrence WHERE source_key = ?1 AND base_hash = ?2)",
        ).map_err(sql_err)?;
        let rows = stmt.query_map(params![source_key, cache_key.1.as_slice()], |row| row.get::<_, String>(0)).map_err(sql_err)?;
        cache.mapped.insert(cache_key.clone(), rows.collect::<std::result::Result<_, _>>().map_err(sql_err)?);
    }
    let used_ordinals = used.entry(cache_key.1).or_default();
    let rows = cache.rows.get(&cache_key).expect("occurrence rows cache entry");
    let mapped = cache.mapped.get(&cache_key).expect("mapped occurrence cache entry");
    let selected = rows.iter()
        .find(|(ordinal, stored, occurrence_id)| !used_ordinals.contains(ordinal) && !mapped.contains(occurrence_id) && stored.as_slice() == sync_state_hash(event).as_slice())
        .or_else(|| rows.iter().find(|(ordinal, _, occurrence_id)| !used_ordinals.contains(ordinal) && !mapped.contains(occurrence_id)))
        .map(|(ordinal, _, occurrence_id)| (*ordinal, occurrence_id.clone()));
    let (ordinal, occurrence_id) = if let Some(selected) = selected {
        selected
    } else {
        let ordinal = rows.iter().map(|(ordinal, _, _)| *ordinal).max().unwrap_or(-1)
            .checked_add(1).ok_or_else(|| Error::Sync("keyless occurrence ordinal overflow".into()))?;
        let occurrence_id = format!(
            "tokenme-occ-v2:{}:{}",
            hex_bytes(&Sha256::digest(sync_identity_bytes(b"tokenme-index/sync-occurrence-id/v1", &[source_key.as_bytes(), &cache_key.1]))),
            ordinal,
        );
        tx.execute(
            "INSERT INTO sync_occurrence(source_key, base_hash, ordinal, occurrence_id, state_hash, updated_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![source_key, cache_key.1.as_slice(), ordinal, occurrence_id, sync_state_hash(event).as_slice(), updated_ms],
        ).map_err(sql_err)?;
        cache.rows.get_mut(&cache_key).expect("occurrence rows cache entry").push((ordinal, sync_state_hash(event).to_vec(), occurrence_id.clone()));
        (ordinal, occurrence_id)
    };
    used_ordinals.insert(ordinal);
    let state_hash = sync_state_hash(event);
    tx.execute(
        "UPDATE sync_occurrence SET state_hash = ?1, updated_ms = ?2 WHERE source_key = ?3 AND base_hash = ?4 AND ordinal = ?5",
        params![state_hash.as_slice(), updated_ms, source_key, cache_key.1.as_slice(), ordinal],
    ).map_err(sql_err)?;
    if let Some(row) = cache.rows.get_mut(&cache_key).expect("occurrence rows cache entry").iter_mut().find(|row| row.0 == ordinal) {
        row.1 = state_hash.to_vec();
    }
    Ok(occurrence_id)
}

pub(crate) fn sync_event_identity(event: &UsageEvent, occurrence_id: Option<&str>) -> Result<String> {
    if let Some(key) = event.dedupe_key.as_deref().filter(|key| !key.is_empty()) {
        return Ok(format!("key:{key}"));
    }
    let occurrence_id = occurrence_id.filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Sync("keyless event has no stable occurrence identity".into()))?;
    Ok(format!("occ:{occurrence_id}"))
}

pub(crate) fn persist_sync_event_identity(
    tx: &Transaction<'_>, event_id: i64, event: &UsageEvent, occurrence_id: Option<&str>,
) -> Result<()> {
    let identity = sync_event_identity(event, occurrence_id)?;
    // `sync_event_identity` is an additive projection of `event`. Older
    // one-shot re-homing repairs changed or removed event rows without also
    // clearing this table, leaving a stale row that could claim the incoming
    // identity.  Do the repair in the same transaction as the projection
    // write so one bad row cannot abort an otherwise valid ingest pass.
    let conflicting_event_id: Option<i64> = tx.query_row(
        "SELECT event_id FROM sync_event_identity WHERE identity = ?1 AND event_id <> ?2",
        params![identity, event_id],
        |row| row.get(0),
    ).optional().map_err(sql_err)?;
    if conflicting_event_id.is_some() {
        tx.execute(
            "DELETE FROM sync_event_identity WHERE identity = ?1 AND event_id <> ?2",
            params![identity, event_id],
        ).map_err(sql_err)?;
    }
    tx.execute(
        "INSERT INTO sync_event_identity(event_id, identity, occurrence_id)
         VALUES(?1, ?2, ?3)
         ON CONFLICT(event_id) DO UPDATE SET identity = excluded.identity,
             occurrence_id = excluded.occurrence_id",
        params![event_id, identity, occurrence_id],
    ).map_err(sql_err)?;
    // Keep a pending journal row attached to the repaired event as well.  The
    // JSON payload (when present) remains the durable update; only its local
    // row pointer was stale.
    if conflicting_event_id.is_some() {
        tx.execute(
            "UPDATE sync_dirty SET event_id = ?1, source_key = ?3, occurrence_id = ?4,
                 event_ts_ms = ?5, updated_ms = ?6
             WHERE identity = ?2",
            params![event_id, identity, event.source, occurrence_id, event.ts_ms, now_ms()],
        ).map_err(sql_err)?;
    }
    Ok(())
}

/// Sync's durable event mirror is opt-in.  The table is created by the
/// device-sync migration, so a normal/unpaired index keeps the old ingest
/// footprint and never serializes every event just in case pairing happens.
pub(crate) fn sync_dirty_enabled(tx: &Transaction<'_>) -> Result<bool> {
    let table_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'device_sync_secondary_config')",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    if !table_exists {
        return Ok(false);
    }
    // `enabled` is the network scheduler switch.  Once a secondary is paired,
    // capture must remain on while that switch is false, otherwise stop/resume
    // can lose changes made before the old identity cursor.  The fallback keeps
    // hand-built/older test tables compatible until the additive store
    // migration has added `capture_enabled`.
    let has_capture_enabled = {
        let mut statement = tx.prepare("PRAGMA table_info(device_sync_secondary_config)").map_err(sql_err)?;
        let names = statement.query_map([], |row| row.get::<_, String>(1)).map_err(sql_err)?;
        names.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?.iter().any(|name| name == "capture_enabled")
    };
    let column = if has_capture_enabled { "capture_enabled" } else { "enabled" };
    let enabled: Option<i64> = tx.query_row(
        &format!("SELECT {column} FROM device_sync_secondary_config WHERE config_key = 'secondary'"),
        [], |row| row.get(0),
    ).optional().map_err(sql_err)?;
    Ok(enabled == Some(1))
}

pub(crate) fn mark_sync_dirty(
    tx: &Transaction<'_>, identity: &str, source_key: &str, occurrence_id: Option<&str>,
    event_id: i64, event_ts_ms: i64, now_ms: i64,
) -> Result<()> {
    if !sync_dirty_enabled(tx)? {
        return Ok(());
    }
    if event_id <= 0 {
        return Err(Error::Sync("sync dirty event id is invalid".into()));
    }
    let current: Option<String> = tx.query_row(
        "SELECT value FROM meta WHERE key = 'device_sync_dirty_revision'",
        [], |row| row.get(0),
    ).optional().map_err(sql_err)?;
    let next = current.as_deref().unwrap_or("0").parse::<i64>()
        .map_err(|_| Error::Sync("device sync dirty revision is corrupt".into()))?
        .checked_add(1)
        .ok_or_else(|| Error::Sync("device sync dirty revision overflow".into()))?;
    if next <= 0 {
        return Err(Error::Sync("device sync dirty revision is invalid".into()));
    }
    tx.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES('device_sync_dirty_revision', ?1)",
        params![next.to_string()],
    ).map_err(sql_err)?;
    let old_json_bytes: i64 = tx.query_row(
        "SELECT COALESCE(length(event_json), 0) FROM sync_dirty WHERE identity = ?1",
        params![identity], |row| row.get(0),
    ).optional().map_err(sql_err)?.unwrap_or(0);
    tx.execute(
        "INSERT INTO sync_dirty(identity, source_key, occurrence_id, event_json, event_id, event_ts_ms, revision, dirty_since_ms, updated_ms)
         VALUES(?1, ?2, ?3, X'', ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT(identity) DO UPDATE SET source_key = excluded.source_key,
             occurrence_id = excluded.occurrence_id, event_json = excluded.event_json,
             event_id = excluded.event_id, event_ts_ms = excluded.event_ts_ms,
             revision = excluded.revision, updated_ms = excluded.updated_ms",
        params![identity, source_key, occurrence_id, event_id, event_ts_ms, next, now_ms],
    ).map_err(sql_err)?;
    if old_json_bytes > 0 {
        adjust_sync_dirty_json_bytes(tx, -old_json_bytes)?;
    }
    Ok(())
}

fn adjust_sync_dirty_json_bytes(tx: &Transaction<'_>, delta: i64) -> Result<()> {
    let current: i64 = tx.query_row(
        "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'device_sync_dirty_json_bytes'), 0)",
        [], |row| row.get(0),
    ).map_err(sql_err)?;
    let next = match current.checked_add(delta).filter(|value| *value >= 0) {
        Some(next) => next,
        None => tx.query_row(
            "SELECT COALESCE(sum(length(event_json)), 0) FROM sync_dirty",
            [], |row| row.get(0),
        ).map_err(sql_err)?,
    };
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('device_sync_dirty_json_bytes', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    ).map_err(sql_err)?;
    Ok(())
}

/// Removes stale rows from the additive sync identity projection.  A few
/// historical re-homing repairs predate this table and could delete/re-key an
/// event without touching its projection row.  Keeping the check here makes
/// opening an existing index self-healing, while keyed rows can still be
/// repaired lazily by the ingest path.
fn prune_stale_sync_event_identities(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM sync_event_identity AS i
          WHERE NOT EXISTS(SELECT 1 FROM event e WHERE e.id = i.event_id)
             OR EXISTS(
                SELECT 1 FROM event e
                 WHERE e.id = i.event_id
                   AND ((e.dedupe_key IS NOT NULL AND e.dedupe_key <> ''
                         AND i.identity <> 'key:' || e.dedupe_key)
                     OR ((e.dedupe_key IS NULL OR e.dedupe_key = '')
                         AND NOT EXISTS(
                            SELECT 1 FROM sync_event_occurrence o
                             WHERE o.event_id = e.id
                               AND i.identity = 'occ:' || o.occurrence_id))))",
        [],
    ).map_err(sql_err)?;
    Ok(())
}

pub(crate) fn load_event_tx(tx: &Transaction<'_>, event_id: i64) -> Result<UsageEvent> {
    let (tool, ts_ms, session, project, model, meter, input, cache_creation, cache_read, output, reasoning, credits, dedupe_key, source):
        (String, i64, String, Option<String>, Option<String>, String, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<String>, String) = tx.query_row(
        &format!("SELECT {SELECT_EVENT_COLS} FROM event WHERE id = ?1"), params![event_id],
        |row| Ok((
            row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?,
            row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?, row.get(11)?, row.get(12)?,
            row.get(13)?, row.get(14)?,
        )),
    ).optional().map_err(sql_err)?.ok_or_else(|| Error::Sync(format!("event {event_id} disappeared while marking sync dirty")))?;
    let mut calls_stmt = tx.prepare("SELECT kind, name FROM call WHERE event_id = ?1 ORDER BY kind, name").map_err(sql_err)?;
    let calls = calls_stmt.query_map(params![event_id], |row| Ok(Call { kind: call_kind_of(&row.get::<_, String>(0)?), name: row.get(1)? }))
        .map_err(sql_err)?.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?;
    let quota: Option<QuotaSample> = tx.query_row(
        "SELECT used_percent, window_minutes, resets_at_ms, label FROM quota WHERE event_id = ?1",
        params![event_id],
        |row| Ok(QuotaSample { used_percent: row.get(0)?, window_minutes: row.get(1)?, resets_at_ms: row.get(2)?, label: row.get(3)?, id: None }),
    ).optional().map_err(sql_err)?;
    Ok(UsageEvent {
        tool, ts_ms, session, project, model,
        counts: TokenCounts {
            input: input.unwrap_or(0.0), cache_creation: cache_creation.unwrap_or(0.0), cache_read: cache_read.unwrap_or(0.0),
            output: output.unwrap_or(0.0), reasoning: reasoning.unwrap_or(0.0), credits: credits.unwrap_or(0.0),
        },
        meter: meter_of(&meter), dedupe_key, calls, quota, source,
    })
}

fn load_sync_dirty_event_tx(
    tx: &Transaction<'_>, identity: String, occurrence_id: Option<String>,
    event_id: Option<i64>, event_json: Vec<u8>, revision: i64,
) -> Result<SyncDirtyEvent> {
    if revision <= 0 {
        return Err(Error::Sync("sync dirty revision is invalid".into()));
    }
    let event = if let Some(event_id) = event_id {
        load_event_tx(tx, event_id)?
    } else if !event_json.is_empty() {
        serde_json::from_slice(&event_json)
            .map_err(|error| Error::Sync(format!("decode legacy sync dirty event: {error}")))?
    } else {
        return Err(Error::Sync(format!("sync dirty row {identity} has no event reference")));
    };
    Ok(SyncDirtyEvent { identity, occurrence_id, event, revision: revision as u64 })
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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
        match Self::open_connection(&path) {
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
        let path = path.into();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::open_connection(&path)
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
        ensure_sync_dirty_columns(&conn)?;
        // Additive sync identity rows are a projection of event.  Old repair
        // paths may delete local events without knowing this newer table; trim
        // only orphan mappings, never the stable occurrence registry.
        conn.execute(
            "DELETE FROM sync_event_identity WHERE NOT EXISTS(SELECT 1 FROM event WHERE event.id = sync_event_identity.event_id)",
            [],
        ).map_err(sql_err)?;
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
        // All compatibility repairs above may have removed or re-keyed event
        // rows.  Reconcile the additive identity projection after those
        // mutations, rather than waiting for the next ingest to encounter a
        // UNIQUE(identity) error.
        prune_stale_sync_event_identities(&conn)?;
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

    /// Runs an ACK-bearing device-sync write on this same SQLite connection.
    /// The generic error keeps protocol errors distinguishable from SQLite
    /// failures while receipt, event, call and rollup writes share one commit.
    pub fn with_network_transaction<T, E, F>(&mut self, f: F) -> std::result::Result<T, E>
    where
        E: From<Error>,
        F: FnOnce(&rusqlite::Transaction<'_>) -> std::result::Result<T, E>,
    {
        self.conn
            .execute_batch("PRAGMA synchronous=FULL;")
            .map_err(|e| E::from(sql_err(e)))?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| E::from(sql_err(e)))?;
        let result = match f(&tx) {
            Ok(value) => tx.commit().map(|()| value).map_err(|e| E::from(sql_err(e))),
            Err(error) => {
                let _ = tx.rollback();
                Err(error)
            }
        };
        // Restore the reporting connection's normal local-ingest durability.
        if let Err(error) = self.conn.execute_batch("PRAGMA synchronous=NORMAL;") {
            if result.is_ok() {
                return Err(E::from(sql_err(error)));
            }
        }
        result
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
        tx.execute(&format!("DELETE FROM sync_event_identity WHERE event_id IN ({local_ids})"), []).map_err(sql_err)?;
        tx.execute(&format!("DELETE FROM sync_event_occurrence WHERE event_id IN ({local_ids})"), []).map_err(sql_err)?;
        tx.execute(&format!("DELETE FROM call WHERE event_id IN ({local_ids})"), []).map_err(sql_err)?;
        tx.execute(&format!("DELETE FROM quota WHERE event_id IN ({local_ids})"), []).map_err(sql_err)?;
        tx.execute(&format!("DELETE FROM event WHERE ({ORIGIN_SQL}) = ''"), []).map_err(sql_err)?;
        tx.execute("DELETE FROM file_state", []).map_err(sql_err)?;
        tx.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
        tx.execute("DELETE FROM meta WHERE key = 'device_sync_occurrence_init_event_id'", []).map_err(sql_err)?;
        tx.commit().map_err(sql_err)
    }

    /// Empties the data tables but keeps the schema and `index_id`.
    pub fn clear(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                "DELETE FROM event; DELETE FROM call; DELETE FROM quota; \
                 DELETE FROM file_state; DELETE FROM event_rollup; \
                 DELETE FROM sync_event_occurrence; DELETE FROM sync_event_identity; \
                 DELETE FROM meta WHERE key = 'device_sync_occurrence_init_event_id';",
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

    /// Dirty local-origin events are recorded by ingest in the same SQLite
    /// transaction as the event/call/quota mutation. The JSON is deliberately
    /// retained until the sync outbox ACKs the corresponding journal row, so a
    /// later prune cannot erase an offline update.
    pub fn sync_dirty_events(&self) -> Result<Vec<SyncDirtyEvent>> {
        let mut out = Vec::new();
        let mut after = 0_u64;
        loop {
            let page = self.sync_dirty_events_page(after, SYNC_DIRTY_PAGE_SIZE)?;
            if page.is_empty() { break; }
            after = page.last().map(|event| event.revision).ok_or_else(|| Error::Sync("empty sync dirty page".into()))?;
            out.extend(page);
        }
        Ok(out)
    }

    /// Reads a bounded dirty page under one SQLite read transaction. Repeated
    /// pages advance by the global revision, which never resets after ACK.
    pub fn sync_dirty_events_page(&self, after_revision: u64, limit: usize) -> Result<Vec<SyncDirtyEvent>> {
        if limit == 0 || limit > SYNC_DIRTY_PAGE_SIZE {
            return Err(Error::Sync(format!("sync dirty page must be in 1..={SYNC_DIRTY_PAGE_SIZE}")));
        }
        let after = i64::try_from(after_revision).map_err(|_| Error::Sync("sync dirty revision is too large".into()))?;
        let tx = self.conn.unchecked_transaction().map_err(sql_err)?;
        let mut stmt = tx.prepare(
            "SELECT identity, occurrence_id, event_id, event_json, revision FROM sync_dirty
              WHERE revision > ?1 ORDER BY revision, identity LIMIT ?2",
        ).map_err(sql_err)?;
        let rows = stmt.query_map(params![after, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<i64>>(2)?, row.get::<_, Vec<u8>>(3)?, row.get::<_, i64>(4)?))
        }).map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (identity, occurrence_id, event_id, event_json, revision) = row.map_err(sql_err)?;
            out.push(load_sync_dirty_event_tx(&tx, identity, occurrence_id, event_id, event_json, revision)?);
        }
        drop(stmt);
        tx.commit().map_err(sql_err)?;
        Ok(out)
    }

    /// Reads one stable export page. The cursor is the protocol identity, not
    /// SQLite rowid; keyless rows use the persisted occurrence mapping. Event
    /// and dirty rows share this read transaction, and both row count and
    /// serialized bytes are bounded before the page is returned.
    pub fn sync_snapshot_page(
        &self,
        after_identity: Option<&str>,
        after_dirty_revision: u64,
        now_ms: i64,
        days: i64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<SyncSnapshotPage> {
        self.sync_snapshot_page_mode(after_identity, after_dirty_revision, now_ms, days, max_rows, max_bytes, true)
    }

    /// Reads a bounded dirty-only page while historical identities are still
    /// being initialized. It deliberately does not inspect the full event
    /// projection, so new paired changes can upload before old history is mapped.
    pub fn sync_dirty_page(
        &self,
        after_dirty_revision: u64,
        now_ms: i64,
        days: i64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<SyncSnapshotPage> {
        self.sync_snapshot_page_mode(None, after_dirty_revision, now_ms, days, max_rows, max_bytes, false)
    }

    fn sync_snapshot_page_mode(
        &self,
        after_identity: Option<&str>,
        after_dirty_revision: u64,
        now_ms: i64,
        days: i64,
        max_rows: usize,
        max_bytes: usize,
        include_events: bool,
    ) -> Result<SyncSnapshotPage> {
        if days <= 0 || days > 3650 { return Err(Error::Sync("sync snapshot days must be in 1..=3650".into())); }
        if max_rows == 0 || max_rows > SYNC_SNAPSHOT_PAGE_ROWS {
            return Err(Error::Sync(format!("sync snapshot page rows must be in 1..={SYNC_SNAPSHOT_PAGE_ROWS}")));
        }
        if max_bytes == 0 || max_bytes > SYNC_SNAPSHOT_PAGE_BYTES {
            return Err(Error::Sync(format!("sync snapshot page bytes must be in 1..={SYNC_SNAPSHOT_PAGE_BYTES}")));
        }
        let day_ms = 86_400_000_i64;
        let coverage_hi_ms = now_ms.div_euclid(day_ms).saturating_add(1).saturating_mul(day_ms);
        let fallback = coverage_hi_ms.saturating_sub(days.saturating_mul(day_ms));
        let tx = self.conn.unchecked_transaction().map_err(sql_err)?;
        let coverage_lo_ms: i64 = tx.query_row(
            "SELECT value FROM meta WHERE key = 'device_sync_initial_coverage_lo'",
            [], |row| row.get::<_, String>(0),
        ).optional().map_err(sql_err)?.and_then(|value| value.parse().ok()).unwrap_or(fallback);

        let missing_identities: i64 = if include_events && after_identity.is_none() {
            tx.query_row(
                &format!("SELECT count(*) FROM event e LEFT JOIN sync_event_identity m ON m.event_id = e.id
                    WHERE ({ORIGIN_SQL}) = '' AND m.event_id IS NULL
                      AND e.ts_ms >= ?1 AND e.ts_ms < ?2"),
                params![coverage_lo_ms, coverage_hi_ms], |row| row.get(0),
            ).map_err(sql_err)?
        } else {
            0
        };
        if include_events && missing_identities != 0 {
            return Err(Error::Sync("event is missing its persisted sync identity".into()));
        }

        let mut bytes_used = 0usize;
        let mut dirty_events = Vec::new();
        let mut dirty_cursor = after_dirty_revision;
        {
            let after = i64::try_from(after_dirty_revision).map_err(|_| Error::Sync("sync dirty revision is too large".into()))?;
            let mut stmt = tx.prepare(
                "SELECT identity, occurrence_id, event_id, event_json, revision FROM sync_dirty
                   WHERE revision > ?1 ORDER BY revision, identity LIMIT ?2",
            ).map_err(sql_err)?;
            let rows = stmt.query_map(params![after, max_rows as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<i64>>(2)?, row.get::<_, Vec<u8>>(3)?, row.get::<_, i64>(4)?))
            }).map_err(sql_err)?;
            for row in rows {
                let (identity, occurrence_id, event_id, event_json, revision) = row.map_err(sql_err)?;
                let dirty = load_sync_dirty_event_tx(&tx, identity, occurrence_id, event_id, event_json, revision)?;
                let encoded_len = serde_json::to_vec(&dirty.event).map_err(|error| Error::Sync(format!("encode sync dirty event: {error}")))?.len();
                if encoded_len > SYNC_SNAPSHOT_MAX_ROW_BYTES {
                    return Err(Error::Sync(format!("sync dirty row exceeds {SYNC_SNAPSHOT_MAX_ROW_BYTES} bytes")));
                }
                let row_bytes = encoded_len.saturating_add(dirty.identity.len()).saturating_add(64);
                if dirty_events.is_empty() && bytes_used.saturating_add(row_bytes) > max_bytes {
                    return Err(Error::Sync("sync dirty row does not fit the snapshot page byte budget".into()));
                }
                if bytes_used.saturating_add(row_bytes) > max_bytes { break; }
                bytes_used = bytes_used.saturating_add(row_bytes);
                dirty_cursor = revision as u64;
                dirty_events.push(dirty);
            }
        }
        let dirty_done: bool = tx.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM sync_dirty WHERE revision > ?1)",
            params![i64::try_from(dirty_cursor).map_err(|_| Error::Sync("sync dirty revision is too large".into()))?],
            |row| row.get(0),
        ).map_err(sql_err)?;

        // All protocol identities are non-empty (`key:`/`occ:` prefixes), so
        // the first page can use an empty lower bound and every later page
        // remains a real indexed range seek.  An `IS NULL OR` predicate made
        // SQLite scan and sort the whole projection on every page.
        let identity_cursor = if include_events { after_identity.map(str::to_string) } else { None };
        let identity_lower_bound = identity_cursor.as_deref().unwrap_or("");
        let mut events = Vec::new();
        let mut event_cursor = identity_cursor.clone();
        let event_limit = max_rows.saturating_sub(dirty_events.len());
        let event_sql = format!(
            "SELECT m.identity, m.occurrence_id, m.event_id
               FROM sync_event_identity m INDEXED BY sync_event_identity_cursor
               CROSS JOIN event e ON e.id = m.event_id
               WHERE ({ORIGIN_SQL}) = '' AND e.ts_ms >= ?1 AND e.ts_ms < ?2
                AND m.identity > ?3
              ORDER BY m.identity, m.event_id LIMIT ?4"
        );
        if include_events && bytes_used < max_bytes && event_limit > 0 {
            let mut stmt = tx.prepare(&event_sql).map_err(sql_err)?;
            let rows = stmt.query_map(
                params![coverage_lo_ms, coverage_hi_ms, identity_lower_bound, event_limit as i64],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, i64>(2)?)),
            ).map_err(sql_err)?;
            for row in rows {
                let (identity, occurrence_id, event_id) = row.map_err(sql_err)?;
                let event = load_event_tx(&tx, event_id)?;
                let encoded_len = serde_json::to_vec(&event).map_err(|error| Error::Sync(format!("encode sync event: {error}")))?.len();
                if encoded_len > SYNC_SNAPSHOT_MAX_ROW_BYTES {
                    return Err(Error::Sync(format!("sync event row exceeds {SYNC_SNAPSHOT_MAX_ROW_BYTES} bytes")));
                }
                let row_bytes = encoded_len.saturating_add(identity.len()).saturating_add(64);
                if events.is_empty() && dirty_events.is_empty() && bytes_used.saturating_add(row_bytes) > max_bytes {
                    return Err(Error::Sync("sync event row does not fit the snapshot page byte budget".into()));
                }
                if bytes_used.saturating_add(row_bytes) > max_bytes { break; }
                bytes_used = bytes_used.saturating_add(row_bytes);
                event_cursor = Some(identity.clone());
                events.push(SyncSnapshotEvent { identity, occurrence_id, event });
            }
        }
        let event_done: bool = if include_events {
            tx.query_row(
                &format!("SELECT NOT EXISTS(
                    SELECT 1
                      FROM sync_event_identity m INDEXED BY sync_event_identity_cursor
                      CROSS JOIN event e ON e.id = m.event_id
                     WHERE ({ORIGIN_SQL}) = '' AND e.ts_ms >= ?1 AND e.ts_ms < ?2
                       AND m.identity > ?3
                )"),
                params![coverage_lo_ms, coverage_hi_ms, event_cursor.as_deref().unwrap_or("" )], |row| row.get(0),
            ).map_err(sql_err)?
        } else {
            false
        };
        tx.commit().map_err(sql_err)?;
        Ok(SyncSnapshotPage {
            coverage_lo_ms, coverage_hi_ms, events, dirty_events,
            next_identity: event_cursor, next_dirty_revision: dirty_cursor,
            events_done: event_done, dirty_done,
        })
    }

    /// Reads the local export projection and all outstanding dirty rows in one
    /// bounded-page SQLite snapshot. The returned vectors are capped rather
    /// than allowed to grow with an offline machine forever; the caller can
    /// surface the limit as a paused sync and retry after reducing the source
    /// backlog.
    pub fn sync_snapshot(&self, now_ms: i64, days: i64) -> Result<SyncSnapshot> {
        if days <= 0 || days > 3650 {
            return Err(Error::Sync("sync snapshot days must be in 1..=3650".into()));
        }
        let day_ms = 86_400_000_i64;
        let coverage_hi_ms = now_ms.div_euclid(day_ms).saturating_add(1).saturating_mul(day_ms);
        let fallback = coverage_hi_ms.saturating_sub(days.saturating_mul(day_ms));
        let tx = self.conn.unchecked_transaction().map_err(sql_err)?;
        let coverage_lo_ms: i64 = tx.query_row(
            "SELECT value FROM meta WHERE key = 'device_sync_initial_coverage_lo'",
            [], |row| row.get::<_, String>(0),
        ).optional().map_err(sql_err)?.and_then(|value| value.parse().ok()).unwrap_or(fallback);

        let mut dirty_events = Vec::new();
        let mut after_revision = 0_i64;
        loop {
            let mut stmt = tx.prepare(
                "SELECT identity, occurrence_id, event_id, event_json, revision FROM sync_dirty
                   WHERE revision > ?1 ORDER BY revision, identity LIMIT ?2",
            ).map_err(sql_err)?;
            let rows = stmt.query_map(params![after_revision, SYNC_DIRTY_PAGE_SIZE as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<i64>>(2)?, row.get::<_, Vec<u8>>(3)?, row.get::<_, i64>(4)?))
            }).map_err(sql_err)?;
            let mut page = Vec::new();
            for row in rows {
                let (identity, occurrence_id, event_id, event_json, revision) = row.map_err(sql_err)?;
                page.push(load_sync_dirty_event_tx(&tx, identity, occurrence_id, event_id, event_json, revision)?);
            }
            drop(stmt);
            if page.is_empty() { break; }
            after_revision = page.last().map(|event| event.revision as i64).ok_or_else(|| Error::Sync("empty sync dirty page".into()))?;
            dirty_events.extend(page);
            if dirty_events.len() > SYNC_SNAPSHOT_MAX_DIRTY {
                return Err(Error::Sync(format!("sync dirty backlog exceeds {} rows", SYNC_SNAPSHOT_MAX_DIRTY)));
            }
        }

        let mut events = Vec::new();
        let mut after_id = 0_i64;
        loop {
            let event_ids: Vec<i64> = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT id FROM event WHERE ({ORIGIN_SQL}) = '' AND ts_ms >= ?1 AND ts_ms < ?2 AND id > ?3 ORDER BY id LIMIT ?4"
                )).map_err(sql_err)?;
                let rows = stmt.query_map(params![coverage_lo_ms, coverage_hi_ms, after_id, SYNC_DIRTY_PAGE_SIZE as i64], |row| row.get::<_, i64>(0)).map_err(sql_err)?;
                rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?
            };
            if event_ids.is_empty() { break; }
            after_id = *event_ids.last().ok_or_else(|| Error::Sync("empty sync event page".into()))?;
            for id in event_ids {
                events.push(load_event_tx(&tx, id)?);
                if events.len() > SYNC_SNAPSHOT_MAX_EVENTS {
                    return Err(Error::Sync(format!("sync snapshot exceeds {} events", SYNC_SNAPSHOT_MAX_EVENTS)));
                }
            }
        }
        tx.commit().map_err(sql_err)?;
        Ok(SyncSnapshot { coverage_lo_ms, coverage_hi_ms, events, dirty_events })
    }

    /// Seeds durable keyless occurrence identities for local history that
    /// predates pairing. It is intentionally called only after the secondary
    /// gate is enabled; ordinary users do not pay for this mirror.
    pub fn ensure_sync_occurrences_page(&mut self, limit: usize) -> Result<bool> {
        if limit == 0 || limit > 256 {
            return Err(Error::Sync("sync occurrence page must be in 1..=256".into()));
        }
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql_err)?;
        let after_id: i64 = tx.query_row(
            "SELECT value FROM meta WHERE key = 'device_sync_occurrence_init_event_id'",
            [], |row| row.get::<_, String>(0),
        ).optional().map_err(sql_err)?.and_then(|value| value.parse().ok()).unwrap_or(0);
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT e.id FROM event e
                   LEFT JOIN sync_event_identity i ON i.event_id = e.id
                  WHERE e.id > ?1 AND ({ORIGIN_SQL}) = '' AND i.event_id IS NULL
                  ORDER BY e.id LIMIT ?2"
            )).map_err(sql_err)?;
            let rows = stmt.query_map(params![after_id, limit as i64], |row| row.get::<_, i64>(0)).map_err(sql_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?
        };
        if ids.is_empty() {
            tx.commit().map_err(sql_err)?;
            return Ok(true);
        }
        let mut used = BTreeMap::<[u8; 32], BTreeSet<i64>>::new();
        let mut cache = SyncOccurrencePageCache::default();
        for id in &ids {
            let event = load_event_tx(&tx, *id)?;
            let occurrence_id = if event.dedupe_key.as_deref().is_none_or(|key| key.is_empty()) {
                if let Some(existing) = tx.query_row(
                    "SELECT occurrence_id FROM sync_event_occurrence WHERE event_id = ?1",
                    params![id], |row| row.get::<_, String>(0),
                ).optional().map_err(sql_err)? {
                    Some(existing)
                } else {
                    let occurrence = sync_occurrence_for_initialization(&tx, &event.source, &event, &mut used, &mut cache, now_ms())?;
                    tx.execute(
                        "INSERT INTO sync_event_occurrence(event_id, occurrence_id) VALUES(?1, ?2)",
                        params![id, occurrence],
                    ).map_err(sql_err)?;
                    Some(occurrence)
                }
            } else {
                None
            };
            persist_sync_event_identity(&tx, *id, &event, occurrence_id.as_deref())?;
        }
        let last_id = *ids.last().ok_or_else(|| Error::Sync("empty sync occurrence page".into()))?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('device_sync_occurrence_init_event_id', ?1)",
            params![last_id.to_string()],
        ).map_err(sql_err)?;
        let more: bool = tx.query_row(
            &format!("SELECT EXISTS(
                SELECT 1 FROM event e
                 LEFT JOIN sync_event_identity i ON i.event_id = e.id
                WHERE e.id > ?1 AND ({ORIGIN_SQL}) = '' AND i.event_id IS NULL
            )"),
            params![last_id], |row| row.get(0),
        ).map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(!more)
    }

    pub fn ensure_sync_occurrences(&mut self) -> Result<()> {
        // Do not revisit already mapped rows: sync_occurrence_for_ingest is a
        // discovery helper for missing occurrences, not a rebuild operation.
        // Small write transactions keep startup responsive on large indexes and
        // make a cancelled initialization restart from the next missing page.
        loop {
            if self.ensure_sync_occurrences_page(256)? { break; }
        }
        Ok(())
    }

    /// Returns persisted stable occurrence IDs for the supplied current event
    /// projection. `None` is reserved for legacy keyless rows that predate the
    /// additive occurrence table; callers may use a full-projection fallback
    /// until that source is ingested once under the new code.
    pub fn sync_occurrence_ids(&self, events: &[UsageEvent]) -> Result<Vec<Option<String>>> {
        let mut used = BTreeMap::<[u8; 32], BTreeSet<i64>>::new();
        let mut out = Vec::with_capacity(events.len());
        for event in events {
            if event.dedupe_key.as_deref().is_some_and(|key| !key.is_empty()) {
                out.push(None);
                continue;
            }
            let base_hash = sync_base_hash(&event.source, event);
            let state_hash = sync_state_hash(event);
            let mut stmt = self.conn.prepare(
                "SELECT ordinal, state_hash, occurrence_id FROM sync_occurrence
                  WHERE source_key = ?1 AND base_hash = ?2 ORDER BY ordinal",
            ).map_err(sql_err)?;
            let rows = stmt.query_map(params![event.source, base_hash.as_slice()], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, String>(2)?))
            }).map_err(sql_err)?;
            let entries = rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?;
            let used_ordinals = used.entry(base_hash).or_default();
            let selected = entries.iter().find(|(ordinal, state, _)| !used_ordinals.contains(ordinal) && state.as_slice() == state_hash.as_slice())
                .or_else(|| entries.iter().find(|(ordinal, _, _)| !used_ordinals.contains(ordinal)));
            if let Some((ordinal, _, occurrence_id)) = selected {
                used_ordinals.insert(*ordinal);
                out.push(Some(occurrence_id.clone()));
            } else {
                out.push(None);
            }
        }
        Ok(out)
    }

    pub fn sync_initial_coverage_lo(&self, now_ms: i64, days: i64) -> Result<i64> {
        if days <= 0 { return Err(Error::Sync("sync coverage days must be positive".into())); }
        let fallback = now_ms.saturating_sub(days.saturating_mul(86_400_000));
        Ok(self.meta_value("device_sync_initial_coverage_lo")?.and_then(|value| value.parse().ok()).unwrap_or(fallback))
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
        // An offline secondary may have the only durable copy of a late
        // update in sync_dirty.  Keep that event/call row until its journal is
        // ACKed; after ACK the row becomes ordinary retention data again.
        const PRUNEABLE: &str = "e.ts_ms < ?1 AND NOT EXISTS(
            SELECT 1 FROM sync_dirty d
             WHERE d.event_id = e.id OR (d.event_id IS NULL AND d.source_key = e.source AND d.event_ts_ms = e.ts_ms
               AND (e.dedupe_key IS NULL OR d.identity = 'key:' || e.dedupe_key))
        )";
        // The doomed rows' ts span decides which rollup days need recomputing;
        // read it before the deletes, inside the same transaction.
        let doomed: (Option<i64>, Option<i64>) = tx
            .query_row(
                &format!("SELECT min(e.ts_ms), max(e.ts_ms) FROM event e WHERE {PRUNEABLE}"),
                params![cutoff_ms],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql_err)?;
        tx.execute(
            &format!("DELETE FROM sync_event_identity WHERE event_id IN (SELECT e.id FROM event e WHERE {PRUNEABLE})"),
            params![cutoff_ms],
        )
        .map_err(sql_err)?;
        tx.execute(
            &format!("DELETE FROM sync_event_occurrence WHERE event_id IN (SELECT e.id FROM event e WHERE {PRUNEABLE})"),
            params![cutoff_ms],
        )
        .map_err(sql_err)?;
        tx.execute(
            &format!("DELETE FROM call WHERE event_id IN (SELECT e.id FROM event e WHERE {PRUNEABLE})"),
            params![cutoff_ms],
        )
        .map_err(sql_err)?;
        tx.execute(
            &format!("DELETE FROM quota WHERE event_id IN (SELECT e.id FROM event e WHERE {PRUNEABLE})"),
            params![cutoff_ms],
        )
        .map_err(sql_err)?;
        let removed =
            tx.execute(&format!("DELETE FROM event AS e WHERE {PRUNEABLE}"), params![cutoff_ms]).map_err(sql_err)?;
        // Nothing removed, nothing to fix; and absent doomed rows there is no
        // span to iterate.
        if removed > 0 {
            // SQLite may reuse a deleted maximum rowid. Restart the bounded
            // identity scan so a reused event id cannot sit behind the cursor.
            tx.execute("DELETE FROM meta WHERE key = 'device_sync_occurrence_init_event_id'", []).map_err(sql_err)?;
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

fn ensure_sync_dirty_columns(conn: &Connection) -> Result<()> {
    let has_event_id = {
        let mut stmt = conn.prepare("PRAGMA table_info(sync_dirty)").map_err(sql_err)?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1)).map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?.iter().any(|name| name == "event_id")
    };
    if !has_event_id {
        conn.execute("ALTER TABLE sync_dirty ADD COLUMN event_id INTEGER", []).map_err(sql_err)?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS sync_dirty_event ON sync_dirty(event_id);
         INSERT OR IGNORE INTO meta(key, value)
         SELECT 'device_sync_dirty_json_bytes', COALESCE(sum(length(event_json)), 0) FROM sync_dirty;",
    ).map_err(sql_err)?;
    // A legacy JSON row can often be upgraded to the exact event reference
    // without changing its payload.  Rows with no mapping keep the old JSON
    // fallback until the next export/ingest touches them.
    conn.execute(
        "UPDATE sync_dirty
            SET event_id = (SELECT MIN(m.event_id) FROM sync_event_identity m WHERE m.identity = sync_dirty.identity)
          WHERE event_id IS NULL AND EXISTS(SELECT 1 FROM sync_event_identity m WHERE m.identity = sync_dirty.identity)",
        [],
    ).map_err(sql_err)?;
    Ok(())
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
    fn old_json_dirty_table_migrates_before_the_event_reference_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE sync_dirty(
                 identity TEXT PRIMARY KEY, source_key TEXT NOT NULL,
                 occurrence_id TEXT, event_json BLOB NOT NULL,
                 event_ts_ms INTEGER NOT NULL, revision INTEGER NOT NULL DEFAULT 1,
                 dirty_since_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL
             );
             INSERT INTO sync_dirty(identity, source_key, event_json, event_ts_ms, dirty_since_ms, updated_ms)
             VALUES('key:legacy', 'legacy.jsonl', '{}', 1, 1, 1);",
        ).unwrap();
        let idx = Index::setup(conn, None).unwrap();
        let has_event_id: bool = idx.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sync_dirty') WHERE name = 'event_id')",
            [], |row| row.get(0),
        ).unwrap();
        let has_index: bool = idx.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'sync_dirty_event')",
            [], |row| row.get(0),
        ).unwrap();
        assert!(has_event_id);
        assert!(has_index);
        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT length(event_json) FROM sync_dirty", [], |row| row.get(0)).unwrap(), 2);
    }

    #[test]
    fn sync_identity_page_plan_seeks_identity_before_window_filter() {
        let idx = Index::open_in_memory().unwrap();
        for id in 1..=1024_i64 {
            idx.conn.execute(
                "INSERT INTO event(id, tool, ts_ms, session, meter, source) VALUES(?1, 'codex', ?2, 'session', 'tokens', 'local.jsonl')",
                params![id, 1_800_000_000_000_i64 + id],
            ).unwrap();
            idx.conn.execute(
                "INSERT INTO sync_event_identity(event_id, identity, occurrence_id) VALUES(?1, ?2, NULL)",
                params![id, format!("key:page-{id:04}")],
            ).unwrap();
        }
        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT m.identity, m.occurrence_id, m.event_id
               FROM sync_event_identity m INDEXED BY sync_event_identity_cursor
               CROSS JOIN event e ON e.id = m.event_id
              WHERE ({ORIGIN_SQL}) = '' AND e.ts_ms >= ?1 AND e.ts_ms < ?2
                AND m.identity > ?3
              ORDER BY m.identity, m.event_id LIMIT ?4"
        );
        let window_lo = 1_800_000_000_000_i64;
        let window_hi = window_lo + 2_000;
        let matching_rows: i64 = idx.conn.query_row(
            &format!("SELECT count(*) FROM sync_event_identity m
                CROSS JOIN event e ON e.id = m.event_id
               WHERE ({ORIGIN_SQL}) = '' AND e.ts_ms >= ?1 AND e.ts_ms < ?2
                 AND m.identity > ?3"),
            params![window_lo, window_hi, ""], |row| row.get(0),
        ).unwrap();
        assert_eq!(matching_rows, 1024, "the plan test must exercise a populated time window");
        let mut stmt = idx.conn.prepare(&sql).unwrap();
        let details = stmt.query_map(params![window_lo, window_hi, "", 256_i64], |row| row.get::<_, String>(3)).unwrap()
            .collect::<std::result::Result<Vec<_>, _>>().unwrap();
        assert!(details.iter().any(|detail| detail.contains("sync_event_identity_cursor") && detail.contains("identity>")), "identity range was not selected: {details:?}");
        assert!(details.iter().any(|detail| detail.contains("SEARCH e USING INTEGER PRIMARY KEY")), "event lookup was not driven by the identity row: {details:?}");
        assert!(!details.iter().any(|detail| detail.contains("event_ts")), "window pagination must not drive from the event timestamp index: {details:?}");
        assert!(!details.iter().any(|detail| detail.contains("TEMP B-TREE")), "window pagination must not sort a full temporary result: {details:?}");

        let done_sql = format!(
            "EXPLAIN QUERY PLAN SELECT NOT EXISTS(
                SELECT 1
                  FROM sync_event_identity m INDEXED BY sync_event_identity_cursor
                  CROSS JOIN event e ON e.id = m.event_id
                 WHERE ({ORIGIN_SQL}) = '' AND e.ts_ms >= ?1 AND e.ts_ms < ?2
                   AND m.identity > ?3
            )"
        );
        let mut stmt = idx.conn.prepare(&done_sql).unwrap();
        let done_details = stmt.query_map(params![window_lo, window_hi, ""], |row| row.get::<_, String>(3)).unwrap()
            .collect::<std::result::Result<Vec<_>, _>>().unwrap();
        assert!(done_details.iter().any(|detail| detail.contains("sync_event_identity_cursor") && detail.contains("identity>")), "event_done did not use the identity range: {done_details:?}");
        assert!(done_details.iter().any(|detail| detail.contains("SEARCH e USING INTEGER PRIMARY KEY")), "event_done was not driven by the identity row: {done_details:?}");
        assert!(!done_details.iter().any(|detail| detail.contains("event_ts")), "event_done must not drive from the event timestamp index: {done_details:?}");
        assert!(!done_details.iter().any(|detail| detail.contains("TEMP B-TREE")), "event_done must not materialize a temporary sort: {done_details:?}");
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
    fn stale_sync_identity_is_repaired_before_it_can_wedge_ingest() {
        let idx = Index::open_in_memory().unwrap();
        idx.conn.execute(
            "INSERT INTO event(tool, ts_ms, session, meter, dedupe_key, source) VALUES
                ('funide', 1, 'old', 'tokens', 'funide#old', '/old.json'),
                ('funide', 2, 'new', 'tokens', 'funide#new', '/new.json')",
            [],
        ).unwrap();
        // Simulate the historical repair bug: the identity row survived while
        // its event was re-keyed, so the new event now wants an identity that
        // is already claimed by another event id.
        idx.conn.execute(
            "INSERT INTO sync_event_identity(event_id, identity, occurrence_id) VALUES(1, 'key:funide#new', NULL)",
            [],
        ).unwrap();
        let event = idx.all_events().unwrap().into_iter().find(|e| e.session == "new").unwrap();
        let tx = idx.conn.unchecked_transaction().unwrap();
        persist_sync_event_identity(&tx, 2, &event, None).unwrap();
        tx.commit().unwrap();
        let rows: Vec<(i64, String)> = idx.conn.prepare(
            "SELECT event_id, identity FROM sync_event_identity ORDER BY event_id",
        ).unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
            .collect::<std::result::Result<_, _>>().unwrap();
        assert_eq!(rows, vec![(2, "key:funide#new".to_string())]);
    }

    #[test]
    fn stale_sync_identity_cleanup_drops_rekeyed_rows() {
        let idx = Index::open_in_memory().unwrap();
        idx.conn.execute(
            "INSERT INTO event(tool, ts_ms, session, meter, dedupe_key, source) VALUES('funide', 1, 's', 'tokens', 'funide#new', '/s.json')",
            [],
        ).unwrap();
        idx.conn.execute(
            "INSERT INTO sync_event_identity(event_id, identity, occurrence_id) VALUES(1, 'key:funide#old', NULL)",
            [],
        ).unwrap();
        prune_stale_sync_event_identities(&idx.conn).unwrap();
        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM sync_event_identity", [], |r| r.get(0)).unwrap(), 0);
    }

    #[test]
    fn rebuild_preserves_remote_rows_and_receipt_sidecars() {
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
        idx.conn
            .execute_batch(
                "CREATE TABLE device_receipt(device_id TEXT, stream_id TEXT, generation INTEGER);
                 INSERT INTO device_receipt VALUES('dev-1', 'stream-1', 1);",
            )
            .unwrap();

        let adapters: Vec<Box<dyn SourceAdapter>> = Vec::new();
        idx.rebuild_with(&adapters, false).unwrap();

        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM event WHERE source = '/local'", [], |r| r.get(0)).unwrap(), 0);
        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM event WHERE source = 'linux:secondary:row-1'", [], |r| r.get(0)).unwrap(), 1);
        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM call WHERE event_id = ?1", params![remote_id], |r| r.get(0)).unwrap(), 1);
        assert_eq!(idx.conn.query_row::<i64, _, _>("SELECT count(*) FROM device_receipt", [], |r| r.get(0)).unwrap(), 1);
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

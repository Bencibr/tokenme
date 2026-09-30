//! The ingest pass: discover → diff against `file_state` → parallel read →
//! one transaction per file.

use std::collections::{BTreeMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDate, TimeZone};
use rusqlite::{params, OptionalExtension, Transaction};
use usage_core::{local_day_of, DateFilter, ReadCursor, Result, SourceAdapter, SourceFile, UsageEvent};

use crate::{call_kind_str, meter_str, now_ms, sql_err, Index, INSERT_EVENT};

/// Nothing older than this stays in the index, which is also how far `rebuild`
/// re-reads. `report`'s heatmap spans 371 days, so 400 keeps it full.
pub const RETENTION_DAYS: i64 = 400;

const RETENTION_MS: i64 = RETENTION_DAYS * 86_400_000;

/// Start of the ingest window for a pass taken at `now_ms`.
pub fn retention_cutoff(now_ms: i64) -> i64 {
    now_ms.saturating_sub(RETENTION_MS)
}

/// Guarantees termination for an adapter whose cursor never stops advancing.
const MAX_BATCHES_PER_FILE: usize = 100_000;

/// Single-row ingest lease. Two processes sharing this file (the menu-bar app
/// and a CLI run) must never interleave cursor reads, because the sources that
/// have no `dedupe_key` — Codex — are only idempotent through their cursor.
const CLAIM_KEY: &str = "index_claim";

/// A claim without a refresh for this long belongs to a dead process, so a
/// killed peer cannot wedge the index.
pub const CLAIM_TTL_MS: i64 = 120_000;

// ---- event_rollup write path -------------------------------------------------
//
// Every event that survives dedupe also lands in `event_rollup`, inside the
// same transaction as its `event` row: the rollup is a *cache*, and a cache
// updated out of step with its source is worse than none. Reads that find the
// cache empty (first publish after the upgrade, a fail-safe wipe) rebuild it
// from `event` and go on — see `facts.rs`.

/// Per-event upsert: sums the new event into its (day, tool, session, project,
/// model, meter) group. `project`/`model` arrive as the `''` NULL sentinel and
/// `n` is the literal 1 — one row per event before conflict resolution.
pub(crate) const ROLLUP_UPSERT: &str = "\
INSERT INTO event_rollup(day, tool, session, project, model, meter, \
    in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts) \
VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1, ?13, ?14, ?14) \
ON CONFLICT(day, tool, session, project, model, meter) DO UPDATE SET \
    in_tok     = in_tok     + excluded.in_tok, \
    cc_tok     = cc_tok     + excluded.cc_tok, \
    cr_tok     = cr_tok     + excluded.cr_tok, \
    out_tok    = out_tok    + excluded.out_tok, \
    reason_tok = reason_tok + excluded.reason_tok, \
    credits    = credits    + excluded.credits, \
    n          = n          + excluded.n, \
    nonzero    = nonzero    + excluded.nonzero, \
    min_ts     = min(min_ts, excluded.min_ts), \
    max_ts     = max(max_ts, excluded.max_ts)";

/// One rollup day, rebuilt from `event` inside the caller's transaction. `lo`
/// and `hi` are the day's half-open ms bounds, computed by the caller in chrono
/// — SQL must never derive a local day itself, or the rollup's bucketing and
/// the report fold's could disagree at a DST edge.
pub(crate) fn recompute_day_tx(tx: &Transaction<'_>, day_key: &str, lo: i64, hi: i64) -> Result<()> {
    tx.execute("DELETE FROM event_rollup WHERE day = ?1", params![day_key]).map_err(sql_err)?;
    tx.execute(
        "INSERT INTO event_rollup(day, tool, session, project, model, meter, \
             in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts) \
         SELECT ?1, tool, session, IFNULL(project, ''), IFNULL(model, ''), meter, \
             IFNULL(sum(in_tok), 0), IFNULL(sum(cc_tok), 0), IFNULL(sum(cr_tok), 0), \
             IFNULL(sum(out_tok), 0), IFNULL(sum(reason_tok), 0), IFNULL(sum(credits), 0), \
             count(*), \
             sum(COALESCE(in_tok, 0) + COALESCE(cc_tok, 0) + COALESCE(cr_tok, 0) \
                 + COALESCE(out_tok, 0) + COALESCE(credits, 0) > 0), \
             min(ts_ms), max(ts_ms) \
         FROM event \
         WHERE ts_ms >= ?2 AND ts_ms < ?3 \
         GROUP BY tool, session, project, model, meter",
        params![day_key, lo, hi],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Half-open `[lo, hi)` ms bounds of one local calendar day, resolved the same
/// way the report's day boundaries are. `None` only where no local midnight
/// can be resolved at all — callers treat that as "wipe the rollup" rather
/// than guess.
pub(crate) fn local_day_bounds(day: NaiveDate) -> Option<(i64, i64)> {
    fn midnight(d: NaiveDate) -> Option<i64> {
        d.and_hms_opt(0, 0, 0)
            .and_then(|n| Local.from_local_datetime(&n).single())
            .map(|dt| dt.timestamp_millis())
    }
    let lo = midnight(day)?;
    let hi = midnight(day + chrono::Duration::days(1))?;
    Some((lo, hi))
}

/// Recomputes the rollup across a contiguous deletion (prune's whole-prefix
/// delete, purge's per-file delete) inside the caller's transaction.
///
/// This never fails: any problem — absurd timestamps with no local day, a day
/// whose bounds cannot be resolved, an error mid-recompute — ends in an EMPTY
/// rollup, which the next read rebuilds from `event`. A stale rollup would be
/// silently wrong money; an empty one costs one lazy rebuild and nothing else.
pub(crate) fn recompute_span_tx(tx: &Transaction<'_>, lo_ts: i64, hi_ts: i64) -> Result<()> {
    let clear = |tx: &Transaction<'_>| -> Result<()> {
        tx.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
        Ok(())
    };
    let (Some(first), Some(last)) = (local_day_of(lo_ts), local_day_of(hi_ts)) else {
        // An unrepresentable timestamp was among the doomed rows: its rollup
        // group lives in the `""` bucket, which only a full rebuild fills.
        return clear(tx);
    };
    // More doomed days than the retention window can hold means the timestamps
    // are junk (retention itself deletes a 400-day window); a per-day sweep
    // would take minutes for data the next rebuild refills in seconds.
    if (last - first).num_days() > RETENTION_DAYS {
        return clear(tx);
    }
    let mut day = first;
    loop {
        let day_key = day.format("%Y-%m-%d").to_string();
        match local_day_bounds(day) {
            Some((lo, hi)) => {
                if recompute_day_tx(tx, &day_key, lo, hi).is_err() {
                    return clear(tx);
                }
            }
            None => return clear(tx),
        }
        if day == last {
            return Ok(());
        }
        day += chrono::Duration::days(1);
    }
}

/// Same maintenance as [`recompute_span_tx`], but the affected days come from
/// the doomed rows' own timestamps (a purge touches scattered days, not a
/// range). One unrepresentable ts wipes the whole rollup — its `""` bucket is
/// only ever filled correctly by the full lazy rebuild.
pub(crate) fn recompute_days_tx(tx: &Transaction<'_>, doomed_ts: &[i64]) -> Result<()> {
    if doomed_ts.iter().any(|t| local_day_of(*t).is_none()) {
        tx.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
        return Ok(());
    }
    let mut days: Vec<NaiveDate> =
        doomed_ts.iter().filter_map(|t| local_day_of(*t)).collect();
    days.sort_unstable();
    days.dedup();
    if days.len() > RETENTION_DAYS as usize {
        tx.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
        return Ok(());
    }
    for day in days {
        let day_key = day.format("%Y-%m-%d").to_string();
        let Some((lo, hi)) = local_day_bounds(day) else {
            tx.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
            return Ok(());
        };
        if recompute_day_tx(tx, &day_key, lo, hi).is_err() {
            tx.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
            return Ok(());
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct IngestReport {
    pub files_scanned: usize,
    pub files_changed: usize,
    pub new_events: u64,
    pub deduped: u64,
    pub purged: u64,
    pub total_events: u64,
    pub took_ms: i64,
    /// Events currently indexed per tool after the pass — the sources list reads
    /// this, so a steady-state pass with nothing new still reports real counts.
    pub per_tool: BTreeMap<String, u64>,
}

#[derive(Default)]
pub(crate) struct Acc {
    files_scanned: usize,
    files_changed: usize,
    new_events: u64,
    deduped: u64,
    purged: u64,
    errors: Vec<String>,
}

struct Job {
    file: SourceFile,
    /// Byte offset (JSONL) or rowid (SQLite) to resume from; 0 after a purge.
    cursor: i64,
    prev_events: i64,
}

struct FileRead {
    file: SourceFile,
    cursor: i64,
    prev_events: i64,
    events: Vec<UsageEvent>,
    error: Option<String>,
}

/// The slice of `file_state` needed to decide what to re-read.
struct Stored {
    size: i64,
    mtime_ms: i64,
    cursor: i64,
    events: i64,
}

#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// Take the claim even while a live peer holds it. The manual path's escape
    /// hatch: a wedged claim must not make `tokenme index --rebuild` useless.
    pub force: bool,
    /// How long to wait for a peer's lease to free before skipping the pass.
    pub claim_wait: Duration,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self { force: false, claim_wait: Duration::from_millis(500) }
    }
}

/// Pre-discovered file lists keyed by adapter id, handed in by a caller that
/// already knows which roots changed: adapters whose roots were not touched
/// reuse the previous snapshot (already restatted fresh) instead of re-walking
/// their whole tree. `None` for an adapter falls back to its full `discover`.
pub type DiscoverHint<'a> = dyn Fn(&str) -> Option<Vec<SourceFile>> + 'a;

pub(crate) fn run<'a>(
    idx: &mut Index,
    adapters: impl Iterator<Item = &'a dyn SourceAdapter>,
    filter: &DateFilter,
    opts: &IngestOptions,
    clear_first: bool,
) -> Result<IngestReport> {
    run_with_hints(idx, adapters, filter, opts, clear_first, &|_| None)
}

pub(crate) fn run_with_hints<'a>(
    idx: &mut Index,
    adapters: impl Iterator<Item = &'a dyn SourceAdapter>,
    filter: &DateFilter,
    opts: &IngestOptions,
    clear_first: bool,
    hints: &DiscoverHint<'_>,
) -> Result<IngestReport> {
    if !idx.claim(opts)? {
        // A peer owns the lease: report the index exactly as it stands rather
        // than racing its cursor, which would double-insert cursor-only sources.
        let per_tool = idx.per_tool_counts()?;
        idx.errors = vec![
            "another tokenme process is ingesting this index; reported it as-is".into(),
        ];
        return Ok(IngestReport {
            total_events: per_tool.values().sum(),
            per_tool,
            ..Default::default()
        });
    }
    let outcome = pass(idx, adapters, filter, clear_first, hints);
    idx.release_claim();
    outcome
}

fn pass<'a>(
    idx: &mut Index,
    adapters: impl Iterator<Item = &'a dyn SourceAdapter>,
    filter: &DateFilter,
    clear_first: bool,
    hints: &DiscoverHint<'_>,
) -> Result<IngestReport> {
    let started = Instant::now();
    let mut acc = Acc::default();
    if clear_first {
        idx.clear()?;
    }
    for adapter in adapters {
        // One broken adapter must degrade to "that tool is missing this pass",
        // never to a failed report for everything else.
        if let Err(e) = one_adapter(idx, adapter, filter, &mut acc, hints) {
            acc.errors.push(format!("{}: {e}", adapter.id()));
        }
    }
    idx.conn
        .execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('last_ingest_ms', ?1)",
            params![now_ms().to_string()],
        )
        .map_err(sql_err)?;
    idx.errors = std::mem::take(&mut acc.errors);
    let per_tool = idx.per_tool_counts()?;
    Ok(IngestReport {
        files_scanned: acc.files_scanned,
        files_changed: acc.files_changed,
        new_events: acc.new_events,
        deduped: acc.deduped,
        purged: acc.purged,
        total_events: per_tool.values().sum(),
        took_ms: started.elapsed().as_millis().min(i64::MAX as u128) as i64,
        per_tool,
    })
}

impl Index {
    /// Waits up to `opts.claim_wait` for the single-row lease in `meta`, then
    /// takes it. Re-entering with our own live claim succeeds.
    pub(crate) fn claim(&mut self, opts: &IngestOptions) -> Result<bool> {
        let deadline = Instant::now() + opts.claim_wait;
        loop {
            if self.try_claim(opts.force)? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn try_claim(&mut self, force: bool) -> Result<bool> {
        let token = self.claim_token.clone();
        let value = self.claim_value();
        // IMMEDIATE so two processes starting at once cannot both read "free".
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let held = live_claim(&tx)?;
        let mine = held.as_deref().is_some_and(|v| v.starts_with(&token));
        if held.is_some() && !mine && !force {
            return Ok(false);
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
            params![CLAIM_KEY, value],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(true)
    }

    /// Drops the claim if we still hold it (a stolen one belongs to the thief).
    pub(crate) fn release_claim(&mut self) {
        let like = format!("{}:%", self.claim_token.clone());
        let _ = self
            .conn
            .execute("DELETE FROM meta WHERE key = ?1 AND value LIKE ?2", params![CLAIM_KEY, like]);
    }

    /// True while a *different* live process holds the ingest lease.
    pub fn is_locked(&self) -> Result<bool> {
        Ok(live_claim(&self.conn)?
            .as_deref()
            .is_some_and(|v| !v.starts_with(&self.claim_token)))
    }

    pub(crate) fn claim_value(&self) -> String {
        format!("{}:{}", self.claim_token, now_ms())
    }
}

/// The claim row, or `None` when it is absent or older than the lease.
fn live_claim(conn: &rusqlite::Connection) -> Result<Option<String>> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![CLAIM_KEY], |r| r.get(0))
        .optional()
        .map_err(sql_err)?;
    Ok(value.filter(|v| {
        let taken = v.rsplit(':').next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        now_ms() - taken < CLAIM_TTL_MS
    }))
}

fn one_adapter(
    idx: &mut Index,
    adapter: &dyn SourceAdapter,
    filter: &DateFilter,
    acc: &mut Acc,
    hints: &DiscoverHint<'_>,
) -> Result<()> {
    let tool = adapter.id();
    let files = match hints(tool) {
        // The caller materialized this list moments ago with fresh stats, so
        // neither the tree walk nor the panic guard around it is needed; what
        // matters downstream (diff vs `file_state`, `unchanged_since`) is that
        // the numbers are current, which the hint contract guarantees.
        Some(files) => files,
        None => match catch_unwind(AssertUnwindSafe(|| adapter.discover(filter))) {
            Ok(files) => files,
            Err(_) => {
                acc.errors.push(format!("{tool}: discover panicked"));
                return Ok(());
            }
        },
    };
    let stored = stored_state(idx)?;

    let mut jobs: Vec<Job> = Vec::new();
    for file in files {
        acc.files_scanned += 1;
        let key = file.key();
        let Some(prev) = stored.get(&key) else {
            acc.files_changed += 1;
            jobs.push(Job { file, cursor: 0, prev_events: 0 });
            continue;
        };
        if file.unchanged_since(prev.size.max(0) as u64, prev.mtime_ms) {
            continue;
        }
        acc.files_changed += 1;
        // A shrink, or an mtime that moved backwards, means the bytes behind our
        // cursor are no longer the bytes we indexed: delete this file's rows and
        // start over, or a rewritten log would be counted twice.
        let rewritten = (file.size as i64) < prev.size || file.mtime_ms < prev.mtime_ms;
        if rewritten {
            acc.purged += idx.purge_source(&key)?;
            jobs.push(Job { file, cursor: 0, prev_events: 0 });
        } else {
            jobs.push(Job { file, cursor: prev.cursor, prev_events: prev.events });
        }
    }
    if jobs.is_empty() {
        return Ok(());
    }

    // Reads fan out to the pool; inserts stay on this thread (`Connection` is
    // `!Sync`) and happen as each file lands so memory stays flat.
    let workers = idx.max_workers.min(jobs.len()).max(1);
    if workers == 1 {
        for job in jobs {
            let read = read_one(adapter, filter, job);
            persist(idx, tool, read, acc)?;
        }
        return Ok(());
    }

    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<FileRead>();
        // Each queue is shared through a mutex rather than moved in, so jobs a
        // failed spawn or a panicked worker left behind still get read here.
        let queues: Vec<_> = split_by_load(jobs, workers)
            .into_iter()
            .map(|q| Arc::new(Mutex::new(VecDeque::from(q))))
            .collect();
        for queue in queues.clone() {
            let tx = tx.clone();
            let _ = std::thread::Builder::new().name("tokenme-index-read".into()).spawn_scoped(
                s,
                move || loop {
                    let Some(job) = pop(&queue) else { return };
                    if tx.send(read_one(adapter, filter, job)).is_err() {
                        return;
                    }
                },
            );
        }
        drop(tx);
        for read in rx {
            if let Err(e) = persist(idx, tool, read, acc) {
                acc.errors.push(format!("{tool}: {e}"));
            }
        }
        for queue in &queues {
            while let Some(job) = pop(queue) {
                if let Err(e) = persist(idx, tool, read_one(adapter, filter, job), acc) {
                    acc.errors.push(format!("{tool}: {e}"));
                }
            }
        }
    });
    Ok(())
}

fn pop(queue: &Arc<Mutex<VecDeque<Job>>>) -> Option<Job> {
    let mut guard = queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.pop_front()
}

/// Size-weighted longest-processing-time-first assignment: the biggest file
/// lands on whichever worker is least loaded, so one 3GB tree does not
/// serialise the tail of the pass.
fn split_by_load(jobs: Vec<Job>, workers: usize) -> Vec<Vec<Job>> {
    let mut sorted = jobs;
    sorted
        .sort_by(|a, b| b.file.size.cmp(&a.file.size).then_with(|| a.file.key().cmp(&b.file.key())));
    let mut queues: Vec<Vec<Job>> = (0..workers).map(|_| Vec::new()).collect();
    let mut loads = vec![0u64; workers];
    for job in sorted {
        let w = loads
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| **l)
            .map(|(i, _)| i)
            .unwrap_or(0);
        loads[w] += job.file.size.max(1);
        queues[w].push(job);
    }
    queues
}

fn stored_state(idx: &Index) -> Result<BTreeMap<String, Stored>> {
    let mut stmt = idx
        .conn
        .prepare("SELECT source_key, size, mtime_ms, cursor, events FROM file_state")
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Stored { size: r.get(1)?, mtime_ms: r.get(2)?, cursor: r.get(3)?, events: r.get(4)? },
            ))
        })
        .map_err(sql_err)?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (k, v) = row.map_err(sql_err)?;
        out.insert(k, v);
    }
    Ok(out)
}

fn read_one(adapter: &dyn SourceAdapter, filter: &DateFilter, job: Job) -> FileRead {
    let Job { file, cursor, prev_events } = job;
    let mut cursor = cursor.max(0) as u64;
    let mut events: Vec<UsageEvent> = Vec::new();
    let mut error = None;
    for _ in 0..MAX_BATCHES_PER_FILE {
        // `read` is allowed to fail for one file without killing the pass, and a
        // half-finished adapter may do worse than fail: never let it take the
        // process (or the other files) down.
        let outcome = catch_unwind(AssertUnwindSafe(|| adapter.read(&file, ReadCursor(cursor))));
        match outcome {
            Err(_) => {
                error = Some(format!("{}: read panicked", file.key()));
                break;
            }
            Ok(Err(e)) => {
                error = Some(format!("{}: {e}", file.key()));
                break;
            }
            Ok(Ok(out)) => {
                let advanced = out.cursor.0 > cursor;
                let produced = !out.events.is_empty();
                cursor = cursor.max(out.cursor.0);
                // Out-of-window records are dropped but still consumed: the
                // caller is expected to pass the retention window, not a
                // narrower user filter, or those rows would be lost for good.
                events.extend(out.events.into_iter().filter(|e| filter.within(e.ts_ms)));
                if !advanced || !produced {
                    break;
                }
            }
        }
    }
    FileRead { file, cursor: cursor as i64, prev_events, events, error }
}

fn persist(idx: &mut Index, tool: &str, read: FileRead, acc: &mut Acc) -> Result<()> {
    let FileRead { file, cursor, prev_events, events, error } = read;
    let key = file.key();
    // Read before the transaction: `Transaction` holds `&mut Connection`.
    let claim = idx.claim_value();
    // IMMEDIATE: the write lock is taken up front, so a peer's read-your-writes
    // query never sees a half-applied file and two writers queue instead of
    // deadlocking on a deferred upgrade.
    let tx = idx
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    let mut inserted = 0i64;
    {
        let mut ins_event = tx.prepare(INSERT_EVENT).map_err(sql_err)?;
        let mut ins_call =
            tx.prepare("INSERT INTO call(event_id, kind, name) VALUES(?1,?2,?3)").map_err(sql_err)?;
        let mut ins_quota = tx
            .prepare(
                "INSERT OR REPLACE INTO quota(event_id, used_percent, window_minutes, resets_at_ms, label) \
                 VALUES(?1,?2,?3,?4,?5)",
            )
            .map_err(sql_err)?;
        // Prepared once per persist, not once per event: on a first ingest of a
        // big file this statement runs as often as INSERT_EVENT.
        let mut up_rollup = tx.prepare(ROLLUP_UPSERT).map_err(sql_err)?;
        for ev in &events {
            let c = &ev.counts;
            let changed = ins_event
                .execute(params![
                    tool,
                    ev.ts_ms,
                    ev.session,
                    ev.project,
                    ev.model,
                    meter_str(ev.meter),
                    c.input,
                    c.cache_creation,
                    c.cache_read,
                    c.output,
                    c.reasoning,
                    c.credits,
                    ev.dedupe_key,
                    key,
                ])
                .map_err(sql_err)?;
            // 0 changes means the partial unique index swallowed it: this record
            // was already indexed, possibly under another file id. The rollup
            // must not see it twice either, so it is skipped together.
            if changed == 0 {
                acc.deduped += 1;
                continue;
            }
            acc.new_events += 1;
            inserted += 1;
            let id = tx.last_insert_rowid();
            for call in &ev.calls {
                ins_call.execute(params![id, call_kind_str(call.kind), call.name]).map_err(sql_err)?;
            }
            if let Some(q) = ev.quota.as_ref() {
                ins_quota
                    .execute(params![id, q.used_percent, q.window_minutes, q.resets_at_ms, q.label])
                    .map_err(sql_err)?;
            }
            // Same transaction as the event row above: the rollup is a cache of
            // exactly these rows, and a crash may lose both or neither, never
            // one. A ts with no representable local day lands in the `""` day
            // bucket, which only all_time reads — the report's own fold skips
            // it, exactly like the event path skips unparseable days.
            let day =
                local_day_of(ev.ts_ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
            up_rollup
                .execute(params![
                    day,
                    tool,
                    ev.session,
                    ev.project.as_deref().unwrap_or(""),
                    ev.model.as_deref().unwrap_or(""),
                    meter_str(ev.meter),
                    c.input,
                    c.cache_creation,
                    c.cache_read,
                    c.output,
                    c.reasoning,
                    c.credits,
                    (c.total() > 0.0) as i64,
                    ev.ts_ms,
                ])
                .map_err(sql_err)?;
        }
    }
    // The cursor only becomes durable once the events it covers are durable, so
    // a crash mid-file replays that file instead of skipping it.
    //
    // A file whose read failed gets `(size, mtime) = (0, 0)` written with it:
    // that is the "incomplete" marker. It can never match a real stat, so the
    // next pass sees the file as changed and resumes from the cursor we did
    // reach — a transient `EMFILE` or half-written log is retried rather than
    // silently and permanently skipped.
    let (size, mtime_ms) =
        if error.is_some() { (0, 0) } else { (file.size as i64, file.mtime_ms) };
    // Refresh the lease in the same transaction: free, and it is what lets a
    // long multi-file pass outlive the TTL legitimately.
    tx.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
        params![CLAIM_KEY, claim],
    )
    .map_err(sql_err)?;
    tx.execute(
        "INSERT INTO file_state(source_key, tool, size, mtime_ms, cursor, events, updated_ms) \
         VALUES(?1,?2,?3,?4,?5,?6,?7) \
         ON CONFLICT(source_key) DO UPDATE SET tool = excluded.tool, size = excluded.size, \
           mtime_ms = excluded.mtime_ms, cursor = excluded.cursor, events = excluded.events, \
           updated_ms = excluded.updated_ms",
        params![key, tool, size, mtime_ms, cursor, prev_events + inserted, now_ms()],
    )
    .map_err(sql_err)?;
    tx.commit().map_err(sql_err)?;
    if let Some(msg) = error {
        acc.errors.push(format!("{msg} — kept {} events, cursor parked for the next pass", prev_events.max(0)));
    }
    Ok(())
}

impl Index {
    /// `ingest_with` with pre-discovered snapshots: for an adapter whose id the
    /// `hints` closure answers `Some(files)`, those files replace `discover` —
    /// the engine's root-scoped reuse of the previous snapshot. One call still
    /// means one claim, one `per_tool_counts` and one `last_ingest_ms` write,
    /// exactly like the plain pass.
    pub fn ingest_with_hints(
        &mut self,
        adapters: &[Box<dyn SourceAdapter>],
        filter: &DateFilter,
        opts: &IngestOptions,
        hints: &DiscoverHint<'_>,
    ) -> Result<IngestReport> {
        run_with_hints(self, adapters.iter().map(|a| &**a), filter, opts, false, hints)
    }
}

impl Index {
    /// Drops everything ingested from one source file, returning the row count.
    pub(crate) fn purge_source(&mut self, key: &str) -> Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        // The doomed rows' timestamps name the rollup days that must be
        // recomputed; they have to be read here, before the delete below
        // removes them.
        let doomed_ts: Vec<i64> = {
            let mut stmt = tx
                .prepare("SELECT ts_ms FROM event WHERE source = ?1")
                .map_err(sql_err)?;
            let rows = stmt.query_map(params![key], |r| r.get(0)).map_err(sql_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?
        };
        tx
            .execute(
                "DELETE FROM call WHERE event_id IN (SELECT id FROM event WHERE source = ?1)",
                params![key],
            )
            .map_err(sql_err)?;
        tx
            .execute(
                "DELETE FROM quota WHERE event_id IN (SELECT id FROM event WHERE source = ?1)",
                params![key],
            )
            .map_err(sql_err)?;
        let n = tx.execute("DELETE FROM event WHERE source = ?1", params![key]).map_err(sql_err)?;
        tx.execute("DELETE FROM file_state WHERE source_key = ?1", params![key]).map_err(sql_err)?;
        // Same transaction as the deletes: the rollup never shows the purged
        // file's spend, and `recompute_days_tx` degrades to a full wipe rather
        // than ever leaving it stale.
        recompute_days_tx(&tx, &doomed_ts)?;
        tx.commit().map_err(sql_err)?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod claim_tests {
    use super::*;
    use usage_core::{DetectedSource, ReadOutcome, Semantics};

    /// An adapter that finds nothing, so the only signal left is whether the
    /// pass ran at all or was skipped for a peer's lease.
    struct Empty;
    impl SourceAdapter for Empty {
        fn id(&self) -> &'static str {
            "empty"
        }
        fn display_name(&self) -> &'static str {
            "Empty"
        }
        fn semantics(&self) -> Semantics {
            Semantics::TOKENS_PER_CALL_INLINE
        }
        fn probe(&self) -> Option<DetectedSource> {
            None
        }
        fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
            Vec::new()
        }
        fn read(&self, _file: &SourceFile, cursor: ReadCursor) -> usage_core::Result<ReadOutcome> {
            Ok(ReadOutcome { events: Vec::new(), cursor })
        }
    }

    fn hold_claim(idx: &mut Index, token: &str, taken_ms: i64) {
        idx.conn
            .execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                params![CLAIM_KEY, format!("{token}:{taken_ms}")],
            )
            .unwrap();
    }

    fn claim_row(idx: &Index) -> Option<String> {
        idx.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![CLAIM_KEY], |r| r.get(0))
            .ok()
    }

    #[test]
    fn a_live_foreign_claim_is_visible_and_makes_the_pass_skip() {
        let mut idx = Index::open_in_memory().unwrap();
        hold_claim(&mut idx, "other", now_ms());
        assert!(idx.is_locked().unwrap(), "a live foreign lease must be reportable");

        let report = idx.ingest_adapter(&Empty, &DateFilter::default()).unwrap();
        assert_eq!(report.files_scanned, 0, "nothing was touched");
        assert_eq!(report.new_events, 0);
        assert!(
            idx.errors().iter().any(|e| e.contains("another tokenme process")),
            "the skip has to be explainable: {:?}",
            idx.errors()
        );
        assert_eq!(claim_row(&idx).unwrap().split(':').next().unwrap(), "other", "not ours to steal");
    }

    #[test]
    fn our_own_live_claim_is_not_a_lock_and_reingest_works() {
        let mut idx = Index::open_in_memory().unwrap();
        assert!(idx.claim(&IngestOptions::default()).unwrap());
        assert!(!idx.is_locked().unwrap(), "we hold it, so it is not locked for us");
        assert!(idx.claim(&IngestOptions::default()).unwrap(), "re-entry must not deadlock");
        idx.release_claim();
        assert_eq!(claim_row(&idx), None, "released");
    }

    #[test]
    fn an_expired_lease_belongs_to_a_dead_process() {
        let mut idx = Index::open_in_memory().unwrap();
        hold_claim(&mut idx, "ghost", now_ms() - CLAIM_TTL_MS - 1);
        assert!(!idx.is_locked().unwrap(), "a stale row must not block the index");
        assert!(idx.claim(&IngestOptions::default()).unwrap(), "and the next pass takes over");
        assert!(claim_row(&idx).unwrap().starts_with(&idx.claim_token));
    }

    #[test]
    fn force_is_the_manual_escape_hatch() {
        let mut idx = Index::open_in_memory().unwrap();
        hold_claim(&mut idx, "other", now_ms());
        let patient = IngestOptions { force: false, claim_wait: Duration::from_millis(20) };
        assert!(!idx.claim(&patient).unwrap(), "waiting does not steal a live lease");
        assert!(idx.claim(&IngestOptions { force: true, ..Default::default() }).unwrap());
        assert!(claim_row(&idx).unwrap().starts_with(&idx.claim_token));
    }

    #[test]
    fn releasing_never_clobbers_a_stolen_lease() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.claim(&IngestOptions::default()).unwrap();
        hold_claim(&mut idx, "thief", now_ms());
        idx.release_claim();
        assert!(claim_row(&idx).unwrap().starts_with("thief:"), "the thief keeps its row");
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use usage_core::{DetectedSource, FileKind, ReadOutcome, Semantics};

    /// An adapter that counts its `discover` calls, so a test can prove a hint
    /// replaced the walk instead of supplementing it.
    struct Counting {
        path: PathBuf,
        discovers: Arc<AtomicUsize>,
    }

    impl Counting {
        /// The list a real discover would build: one file, freshly statted.
        fn discover_list(&self) -> Vec<SourceFile> {
            let meta = std::fs::metadata(&self.path).unwrap();
            let mtime_ms = meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;
            vec![SourceFile {
                path: self.path.clone(),
                kind: FileKind::Jsonl,
                size: meta.len(),
                mtime_ms,
            }]
        }
    }

    impl SourceAdapter for Counting {
        fn id(&self) -> &'static str {
            "counting"
        }
        fn display_name(&self) -> &'static str {
            "Counting"
        }
        fn semantics(&self) -> Semantics {
            Semantics::TOKENS_PER_CALL_INLINE
        }
        fn probe(&self) -> Option<DetectedSource> {
            None
        }
        fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
            self.discovers.fetch_add(1, Ordering::SeqCst);
            self.discover_list()
        }
        fn read(&self, _file: &SourceFile, cursor: ReadCursor) -> usage_core::Result<ReadOutcome> {
            // One event per call, cursor unchanged: the pass reads exactly once.
            Ok(ReadOutcome { events: vec![UsageEvent::new("counting", 1_780_000_000_000, "s")], cursor })
        }
    }

    #[test]
    fn a_hint_supplies_the_files_so_discover_never_runs_and_events_still_land() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, b"{}\n").unwrap();
        let discovers = Arc::new(AtomicUsize::new(0));
        let counting = Counting { path: path.clone(), discovers: Arc::clone(&discovers) };
        let hinted = counting.discover_list();
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(counting)];
        let mut idx = Index::open_in_memory().unwrap();

        let filter = DateFilter::default();
        let opts = IngestOptions::default();
        let hints = |id: &str| (id == "counting").then(|| hinted.clone());

        // Pass 1: the hint replaces discover, and its files still flow through
        // the normal diff → read → persist path into real indexed events.
        let report = idx.ingest_with_hints(&adapters, &filter, &opts, &hints).unwrap();
        assert_eq!(discovers.load(Ordering::SeqCst), 0, "a hint must replace discover");
        assert_eq!(report.files_scanned, 1);
        assert_eq!(report.new_events, 1, "hint files produce events like discovered ones");
        assert_eq!(idx.event_count().unwrap(), 1);

        // Pass 2: the same fresh snapshot is unchanged against file_state.
        let report = idx.ingest_with_hints(&adapters, &filter, &opts, &hints).unwrap();
        assert_eq!(report.new_events, 0, "an unchanged hinted file is skipped, not re-read");
        assert_eq!(discovers.load(Ordering::SeqCst), 0);

        // The no-hint path still walks: same stats, so nothing new, but the
        // adapter's discover is the source of the file list again.
        let report = idx.ingest_with(&adapters, &filter, &opts).unwrap();
        assert_eq!(discovers.load(Ordering::SeqCst), 1, "a None hint falls back to discover");
        assert_eq!(report.new_events, 0, "fresh-stat equivalence holds whoever statted");
    }

    #[test]
    fn a_hinted_pass_still_honours_a_foreign_claim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, b"{}\n").unwrap();
        let discovers = Arc::new(AtomicUsize::new(0));
        let counting = Counting { path, discovers: Arc::clone(&discovers) };
        let hinted = counting.discover_list();
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(counting)];
        let mut idx = Index::open_in_memory().unwrap();
        idx.conn
            .execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                params![CLAIM_KEY, format!("other:{}", now_ms())],
            )
            .unwrap();

        let hints = |id: &str| (id == "counting").then(|| hinted.clone());
        let report =
            idx.ingest_with_hints(&adapters, &DateFilter::default(), &IngestOptions::default(), &hints)
                .unwrap();
        assert_eq!(report.files_scanned, 0, "a peer's lease skips the pass even with hints");
        assert_eq!(discovers.load(Ordering::SeqCst), 0);
    }
}

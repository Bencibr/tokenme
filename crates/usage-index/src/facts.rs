//! The report read path: [`Index::report_facts`].
//!
//! One call answers everything the panel needs from ~2.2k rollup rows plus four
//! small live slices over `event` — never from a 390k-row event scan. The
//! rollup is a cache: if a read finds it empty while `event` is not (first
//! publish after the upgrade, a fail-safe wipe), it is rebuilt here from the
//! events, once, inside one transaction.

use rusqlite::{params, params_from_iter, OptionalExtension};
use serde::{Deserialize, Serialize};

use usage_core::{
    local_day_of, AggregatePlan, CallFact, CycleModelUsage, CycleStatus, CycleUsage, FactGroup, MachineScope, QuotaFact, QuotaSample,
    QuotaView, ReportFacts, Result, RollupRow, SessionFact, SessionGroup, TokenCounts, LIVE_HOUR0, LIVE_PREV,
    LIVE_PREV_HOUR0, LIVE_QUARTER0,
    LIVE_TODAY,
};

use crate::{
    call_kind_of, ingest::{local_day_bounds, recompute_day_tx},
    meter_of, sql_err, Index, ORIGIN_SQL,
};

/// Most local days the eager build will walk forward before falling back to a
/// capped backward window. Retention is 400 days, so a healthy index never
/// trips this; a corrupt timestamp pair must not turn one publish into a
/// thousand day-recomputes.
const ROLLUP_BUILD_CAP_DAYS: i64 = 402;
const CYCLE_META_PREFIX: &str = "cycle:v1:";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CycleState {
    identity: String,
    period_start_ms: i64,
    period_end_ms: i64,
    observed_tokens: f64,
    previous_tokens: Option<f64>,
    complete: bool,
    last_used_percent: f64,
    /// The newest quota sample that contributed to this state. Older cached
    /// samples must never move a cycle backwards or trigger a reset.
    #[serde(default)]
    last_sampled_at_ms: i64,
}

const RESET_JITTER_TOLERANCE_MS: i64 = 60_000;

fn cycle_state_key(q: &QuotaView) -> String {
    format!("{CYCLE_META_PREFIX}{}:{}:{}", q.tool, q.window_minutes, cycle_bucket_key(q))
}

fn legacy_cycle_state_key(q: &QuotaView) -> String {
    format!("{CYCLE_META_PREFIX}{}:{}:{}", q.tool, q.window_minutes, q.label.as_deref().unwrap_or_default())
}

fn cycle_bucket_key(q: &QuotaView) -> String {
    if let Some(family) = model_family_hint(q.label.as_deref()) {
        return format!("family:{family}");
    }
    q.label
        .as_deref()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The bare-column rule: in a `SELECT …, name, MIN(rowid) … GROUP BY …` query
/// SQLite takes `name` from the row that produced the MIN — exactly the
/// "first call per kind names the event" semantics the event path gets from
/// `calls.iter().find(...)`.
const FIRST_CALLS_CTE: &str = "\
SELECT c.event_id, c.kind, c.name, MIN(c.rowid) AS rid
FROM call c
JOIN event e ON e.id = c.event_id
JOIN w ON e.ts_ms >= w.lo AND e.ts_ms < w.hi
GROUP BY c.event_id, c.kind";

/// Per-group sums shared by every slice query. The `nonzero` column mirrors
/// `counts.total() > 0` per event, which is what the event path counts into
/// `unpriced.requests`.
const GROUP_SUMS: &str = "\
IFNULL(sum(e.in_tok), 0), IFNULL(sum(e.cc_tok), 0), IFNULL(sum(e.cr_tok), 0), \
IFNULL(sum(e.out_tok), 0), IFNULL(sum(e.reason_tok), 0), IFNULL(sum(e.credits), 0), \
count(*), \
sum(COALESCE(e.in_tok, 0) + COALESCE(e.cc_tok, 0) + COALESCE(e.cr_tok, 0) \
    + COALESCE(e.out_tok, 0) + COALESCE(e.credits, 0) > 0)";

/// `''` is the rollup's NULL sentinel for project/model (the group key is NOT
/// NULL); this folds it back so the fold sees what the event path sees.
fn opt_of(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

impl Index {
    /// Attach local token accounting to the live quota rows. The event table is
    /// indexed by `(tool, ts_ms)`, so bounded sums stay cheap and do not require
    /// a second raw-log scan. Vendor percentages are only used for the estimate
    /// for providers whose window meter is known to be token-like.
    pub fn attach_cycle_usage(&self, quotas: &mut [QuotaView], now_ms: i64) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for q in quotas {
            if q.origin == usage_core::QuotaOrigin::Budget || !matches!(q.window_minutes, 300 | 10_080) {
                continue;
            }
            if !seen.insert((q.tool.clone(), q.window_minutes, q.resets_at_ms, cycle_bucket_key(q))) {
                continue;
            }
            let window_ms = q.window_minutes.saturating_mul(60_000);
            let observed_until = (q.sampled_at_ms > 0 && q.sampled_at_ms <= now_ms).then_some(q.sampled_at_ms).unwrap_or(now_ms);
            // A missing or stale reset is not a license to report zero. Keep a
            // trailing window for the local tally, while withholding the
            // stronger "partial cycle" claim until a future reset is known.
            let start = if q.resets_at_ms > observed_until {
                q.resets_at_ms.saturating_sub(window_ms)
            } else {
                observed_until.saturating_sub(window_ms)
            };
            let key = cycle_state_key(q);
            let identity = q.id.as_deref().unwrap_or("unknown");
            let raw = self.meta_value(&key)?.or_else(|| {
                let legacy = legacy_cycle_state_key(q);
                (legacy != key).then(|| self.meta_value(&legacy).ok()).flatten().flatten()
            });
            let old = raw.and_then(|raw| serde_json::from_str::<CycleState>(&raw).ok());
            let observed_identity = (identity != "unknown").then_some(identity);
            let old_identity = old.as_ref().map(|s| s.identity.as_str()).filter(|id| *id != "unknown");
            // A log-derived/cached row has no account id. Preserve the last
            // verified identity instead of replacing it with `unknown` and
            // losing account-switch isolation while polling is disabled.
            let effective_identity = observed_identity.or(old_identity).unwrap_or("unknown");
            let account_changed = old.as_ref().is_some_and(|s| observed_identity.is_some_and(|new| s.identity != new));
            // An explicit account change wins over timestamp ordering: a
            // delayed cache entry from the previous account must not repaint
            // the new account's cycle.
            let stale_sample = !account_changed && old.as_ref().is_some_and(|s| s.last_sampled_at_ms > observed_until);
            let reset_seen = old.as_ref().is_some_and(|s| {
                !stale_sample && q.resets_at_ms > observed_until && start > s.period_start_ms + RESET_JITTER_TOLERANCE_MS
            });
            let usage_reset = old.as_ref().is_some_and(|s| {
                !stale_sample && !account_changed && s.last_used_percent >= 20.0 && q.used_percent + 20.0 < s.last_used_percent
            });
            if stale_sample {
                let state = old.as_ref().expect("stale_sample implies prior state");
                let trusted = token_percent_is_trustworthy(&q.tool);
                let unverified = !trusted && percent_is_unverified_token_like(&q.tool);
                let estimated_tokens = if state.observed_tokens > 0.0
                    && state.last_used_percent >= if trusted { 5.0 } else { 0.1 }
                    && (trusted || unverified)
                    && !(trusted && state.identity == "unknown")
                    && state.period_end_ms > state.last_sampled_at_ms
                {
                    Some(state.observed_tokens * 100.0 / state.last_used_percent.clamp(0.1, 100.0))
                } else {
                    None
                };
                let status = if unverified {
                    CycleStatus::Unverified
                } else if !trusted {
                    CycleStatus::Unavailable
                } else if state.complete {
                    CycleStatus::Complete
                } else if state.period_end_ms > state.last_sampled_at_ms {
                    CycleStatus::Partial
                } else {
                    CycleStatus::Collecting
                };
                let models = if trusted || unverified {
                    self.sum_tokens_by_model(&q.tool, state.period_start_ms, observed_until, model_family_hint(q.label.as_deref()))?
                    .into_iter()
                    .map(|(model, tokens)| CycleModelUsage { model, tokens })
                    .collect()
                } else {
                    Vec::new()
                };
                q.cycle = Some(CycleUsage {
                    observed_tokens: state.observed_tokens,
                    estimated_tokens,
                    previous_tokens: state.previous_tokens,
                    status,
                    period_start_ms: state.period_start_ms,
                    period_end_ms: state.period_end_ms,
                    models,
                });
                continue;
            }
            let (start, previous_tokens, mut complete) = if account_changed {
                (observed_until, None, false)
            } else if reset_seen || usage_reset {
                (start, old.as_ref().and_then(|s| s.complete.then_some(s.observed_tokens)), false)
            } else if let Some(state) = &old {
                (state.period_start_ms, state.previous_tokens, state.complete)
            } else {
                (start, None, false)
            };
            let end = if q.resets_at_ms > observed_until {
                q.resets_at_ms
            } else {
                start.saturating_add(window_ms)
            };
            let observed_tokens = self.sum_tokens(&q.tool, start, observed_until, model_family_hint(q.label.as_deref()))?;
            let known_boundary = q.resets_at_ms > observed_until;
            if q.used_percent >= 99.5 {
                complete = true;
            }
            let trusted = token_percent_is_trustworthy(&q.tool);
            let unverified = !trusted && percent_is_unverified_token_like(&q.tool);
            // The unverified tier (Cline) relaxes two guards on purpose: the
            // single-account store keeps the "unknown" identity stable, and the
            // whole point is the early-window magnitude the >=5% gate refuses.
            let estimated_tokens = if known_boundary
                && !account_changed
                && (trusted || unverified)
                && !(trusted && identity == "unknown")
                && observed_tokens > 0.0
                && q.used_percent >= if trusted { 5.0 } else { 0.1 }
            {
                Some(observed_tokens * 100.0 / q.used_percent.clamp(0.1, 100.0))
            } else {
                None
            };
            let status = if unverified {
                CycleStatus::Unverified
            } else if !trusted {
                CycleStatus::Unavailable
            } else if complete {
                CycleStatus::Complete
            } else if q.resets_at_ms > observed_until {
                CycleStatus::Partial
            } else {
                CycleStatus::Collecting
            };
            let state = CycleState {
                identity: effective_identity.to_string(),
                period_start_ms: start,
                period_end_ms: end,
                observed_tokens,
                previous_tokens,
                complete,
                last_used_percent: q.used_percent,
                last_sampled_at_ms: observed_until,
            };
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                    params![key, serde_json::to_string(&state).map_err(|e| usage_core::Error::Sqlite(e.to_string()))?],
                )
                .map_err(sql_err)?;
            let models = if trusted || unverified {
                self.sum_tokens_by_model(&q.tool, start, observed_until, model_family_hint(q.label.as_deref()))?
                .into_iter()
                .map(|(model, tokens)| CycleModelUsage { model, tokens })
                .collect()
            } else {
                Vec::new()
            };
            q.cycle = Some(CycleUsage {
                observed_tokens,
                estimated_tokens,
                previous_tokens,
                status,
                period_start_ms: start,
                period_end_ms: end,
                models,
            });
        }
        Ok(())
    }

    /// The same meter and machine scope as [`Self::sum_tokens`], grouped by the
    /// event's model id, heaviest first. Family-hinted rows (Claude's
    /// Sonnet/Opus buckets) filter like `sum_tokens` so the breakdown sums to
    /// the row's observed total; unbounded names are capped at 8 rows.
    fn sum_tokens_by_model(
        &self,
        tool: &str,
        start_ms: i64,
        end_ms: i64,
        model_family: Option<&str>,
    ) -> Result<Vec<(String, f64)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT COALESCE(NULLIF(model, ''), 'unknown') AS m, \
             COALESCE(SUM(COALESCE(in_tok, 0) + COALESCE(cc_tok, 0) + COALESCE(cr_tok, 0) + COALESCE(out_tok, 0)), 0) \
             FROM event WHERE tool = ?1 AND source NOT LIKE 'linux:%' AND meter = 'tokens' \
             AND ts_ms >= ?2 AND ts_ms < ?3 \
             AND (?4 IS NULL OR lower(COALESCE(model, '')) LIKE '%' || lower(?4) || '%') \
             GROUP BY m ORDER BY 2 DESC LIMIT 8",
        ).map_err(sql_err)?;
        let rows = stmt
            .query_map(rusqlite::params![tool, start_ms, end_ms, model_family], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (model, tokens) = row.map_err(sql_err)?;
            if tokens > 0.0 {
                out.push((model, tokens));
            }
        }
        Ok(out)
    }

    fn sum_tokens(&self, tool: &str, start_ms: i64, end_ms: i64, model_family: Option<&str>) -> Result<f64> {
        // shortcut: events have no quota-account fingerprint, so aggregation is per tool; upgrade when adapters persist one.
        self.conn
            .query_row(
                "SELECT COALESCE(SUM(COALESCE(in_tok, 0) + COALESCE(cc_tok, 0) + COALESCE(cr_tok, 0) + COALESCE(out_tok, 0)), 0) FROM event WHERE tool = ?1 AND source NOT LIKE 'linux:%' AND meter = 'tokens' AND ts_ms >= ?2 AND ts_ms < ?3 AND (?4 IS NULL OR lower(COALESCE(model, '')) LIKE '%' || lower(?4) || '%')",
                rusqlite::params![tool, start_ms, end_ms, model_family],
                |r| r.get(0),
            )
            .map_err(sql_err)
    }

    /// Everything [`usage_core::summarize_facts`] needs, in ~2.2k rows instead
    /// of ~390k events. `plan` carries every period boundary, resolved in
    /// chrono by the caller — SQL never derives a local time here.
    ///
    /// `scope` must be the same [`MachineScope`] the caller passes to
    /// [`usage_core::summarize_facts`]: everything except the sessions is
    /// fetched unscoped and filtered by the fold, but the session list's LIMIT
    /// cannot be re-applied after the fact — its top-N has to be the top-N
    /// *within the scope*, or a heavy local machine would starve a remote's
    /// list empty.
    pub fn report_facts(&mut self, plan: &AggregatePlan, scope: &MachineScope) -> Result<ReportFacts> {
        let mut facts = ReportFacts::default();
        // Lazy build: only when there is something to build from and nothing to
        // build into. Any other state is already consistent.
        let event_nonempty: bool = self
            .conn
            .query_row("SELECT EXISTS(SELECT 1 FROM event)", [], |r| r.get(0))
            .map_err(sql_err)?;
        let rollup_nonempty: bool = self
            .conn
            .query_row("SELECT EXISTS(SELECT 1 FROM event_rollup)", [], |r| r.get(0))
            .map_err(sql_err)?;
        if event_nonempty && !rollup_nonempty {
            facts.rebuilt = self.rebuild_rollup()?;
        }
        facts.rollup = self.fetch_rollup()?;
        facts.live = self.fetch_live(plan)?;
        facts.calls = self.fetch_calls(plan)?;
        facts.quotas = self.fetch_quotas(plan.now_ms)?;
        facts.sessions = self.fetch_sessions(plan.recent_session_limit, scope)?;
        facts.syncs = self.fetch_syncs()?;
        Ok(facts)
    }

    /// The `sync:linux:<origin>` records `tokenme import` leaves in `meta`,
    /// newest first. A malformed record is skipped (one bad row must not kill
    /// the badge) — the write side owns the shape and writes in-transaction,
    /// so this is defense against a hand-edited index, not against ourselves.
    fn fetch_syncs(&self) -> Result<Vec<usage_core::SyncRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM meta WHERE key LIKE 'sync:linux:%'")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut out: Vec<usage_core::SyncRecord> = Vec::new();
        for row in rows {
            let Ok(value) = row else { continue };
            if let Ok(record) = serde_json::from_str::<usage_core::SyncRecord>(&value) {
                if !record.origin.trim().is_empty() {
                    out.push(record);
                }
            }
        }
        out.sort_by_key(|r| -r.imported_at_ms);
        Ok(out)
    }

    /// Rebuilds `event_rollup` from `event` in one transaction. Returns whether
    /// rows were written (an empty `event` table builds nothing).
    fn rebuild_rollup(&mut self) -> Result<bool> {
        let (min_ts, max_ts): (Option<i64>, Option<i64>) = self
            .conn
            .query_row("SELECT min(ts_ms), max(ts_ms) FROM event", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(sql_err)?;
        let (Some(lo_ts), Some(hi_ts)) = (min_ts, max_ts) else {
            return Ok(false);
        };
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        tx.execute("DELETE FROM event_rollup", []).map_err(sql_err)?;
        match (local_day_of(lo_ts), local_day_of(hi_ts)) {
            (Some(first), Some(last)) => {
                // Every event between two representable timestamps has a
                // representable day too, so the forward loop covers the table.
                if (last - first).num_days() <= ROLLUP_BUILD_CAP_DAYS {
                    let mut day = first;
                    loop {
                        rollup_one_day(&tx, day)?;
                        if day == last {
                            break;
                        }
                        day += chrono::Duration::days(1);
                    }
                } else {
                    // Corrupt span: keep the newest `ROLLUP_BUILD_CAP_DAYS`
                    // days and sweep everything older into the `""` bucket,
                    // which only all_time reads.
                    let first_kept = last - chrono::Duration::days(ROLLUP_BUILD_CAP_DAYS - 1);
                    let mut day = first_kept;
                    while day <= last {
                        rollup_one_day(&tx, day)?;
                        day += chrono::Duration::days(1);
                    }
                    let (sweep_below, _) = local_day_bounds(first_kept)
                        .ok_or_else(|| crate::Error::Sqlite("no local midnight".into()))?;
                    sweep_into_rollup(&tx, Some(sweep_below))?;
                }
            }
            _ => {
                // Timestamps with no representable local day at all: every
                // event sweeps into the `""` bucket.
                sweep_into_rollup(&tx, None)?;
            }
        }
        tx.commit().map_err(sql_err)?;
        Ok(true)
    }

    fn fetch_rollup(&self) -> Result<Vec<RollupRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT day, origin, tool, session, project, model, meter, \
                        in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts \
                 FROM event_rollup",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RollupRow {
                    day: r.get(0)?,
                    origin: r.get(1)?,
                    tool: r.get(2)?,
                    session: r.get(3)?,
                    project: opt_of(r.get(4)?),
                    model: opt_of(r.get(5)?),
                    meter: meter_of(&r.get::<_, String>(6)?),
                    counts: TokenCounts {
                        input: r.get(7)?,
                        cache_creation: r.get(8)?,
                        cache_read: r.get(9)?,
                        output: r.get(10)?,
                        reasoning: r.get(11)?,
                        credits: r.get(12)?,
                    },
                    n: r.get::<_, i64>(13)?.max(0) as u64,
                    nonzero: r.get::<_, i64>(14)?.max(0) as u64,
                    min_ts: r.get(15)?,
                    max_ts: r.get(16)?,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)
    }

    /// Today, the four prev-slice heads, the 24 hourly buckets of today and of
    /// yesterday, grouped straight out of `event`. One statement, 53 bucket
    /// rows in a VALUES CTE; the bounds all come from the plan.
    fn fetch_live(&self, plan: &AggregatePlan) -> Result<Vec<FactGroup>> {
        // (bucket, lo, hi) — half-open, like every other range in the pipeline.
        let mut buckets: Vec<(u32, i64, i64)> =
            vec![(LIVE_TODAY, plan.cur_start_ms[0], plan.now_ms + 1)];
        for (i, &start) in plan.prev_start_ms.iter().enumerate() {
            buckets.push((LIVE_PREV + i as u32, start, plan.prev_live_end_ms[i]));
        }
        for h in 0..24usize {
            buckets.push((LIVE_HOUR0 + h as u32, plan.hour_start_ms[h], plan.hour_start_ms[h + 1]));
        }
        for h in 0..24usize {
            buckets.push((
                LIVE_PREV_HOUR0 + h as u32,
                plan.prev_hour_start_ms[h],
                plan.prev_hour_start_ms[h + 1],
            ));
        }
        for q in 0..96usize {
            buckets.push((LIVE_QUARTER0 + q as u32, plan.quarter_start_ms[q], plan.quarter_start_ms[q + 1]));
        }
        // The bucket id is our own constant, so it inlines as a literal; only
        // the (lo, hi) bounds bind, two parameters per bucket.
        let values =
            buckets.iter().map(|(bid, _, _)| format!("({bid}, ?, ?)")).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "WITH b(bid, lo, hi) AS (VALUES {values}) \
             SELECT b.bid, {ORIGIN_SQL}, e.tool, e.session, COALESCE(e.project, ''), \
                    COALESCE(e.model, ''), e.meter, {GROUP_SUMS} \
             FROM b JOIN event e ON e.ts_ms >= b.lo AND e.ts_ms < b.hi \
             GROUP BY b.bid, {ORIGIN_SQL}, e.tool, e.session, \
                      COALESCE(e.project, ''), COALESCE(e.model, ''), e.meter \
             ORDER BY b.bid"
        );
        let mut flat: Vec<i64> = Vec::with_capacity(buckets.len() * 3);
        for (_, lo, hi) in &buckets {
            flat.push(*lo);
            flat.push(*hi);
        }
        let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(params_from_iter(flat.iter()), |r| {
                Ok(FactGroup {
                    bucket: r.get::<_, i64>(0)?.max(0) as u32,
                    origin: r.get(1)?,
                    tool: r.get(2)?,
                    session: r.get(3)?,
                    project: opt_of(r.get(4)?),
                    model: opt_of(r.get(5)?),
                    meter: meter_of(&r.get::<_, String>(6)?),
                    counts: TokenCounts {
                        input: r.get(7)?,
                        cache_creation: r.get(8)?,
                        cache_read: r.get(9)?,
                        output: r.get(10)?,
                        reasoning: r.get(11)?,
                        credits: r.get(12)?,
                    },
                    requests: r.get::<_, i64>(13)?.max(0) as u64,
                    nonzero: r.get::<_, i64>(14)?.max(0) as u64,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)
    }

    /// The mcp/skill breakdowns of the four cur windows. `firsts` keeps, per
    /// (event, kind), the first call by rowid — the event path's
    /// `calls.iter().find(kind)` — and the outer join re-projects each event
    /// into every cur window it falls in, exactly like the per-window scan.
    fn fetch_calls(&self, plan: &AggregatePlan) -> Result<Vec<CallFact>> {
        let sql = format!(
            "WITH w(bid, lo, hi) AS (VALUES (0, ?, ?), (1, ?, ?), (2, ?, ?), (3, ?, ?)), \
             firsts AS ({FIRST_CALLS_CTE}) \
             SELECT w.bid, f.kind, f.name, {ORIGIN_SQL}, e.tool, e.session, \
                    COALESCE(e.project, ''), COALESCE(e.model, ''), e.meter, {GROUP_SUMS} \
             FROM w \
             JOIN event e ON e.ts_ms >= w.lo AND e.ts_ms < w.hi \
             JOIN firsts f ON f.event_id = e.id \
             GROUP BY w.bid, f.kind, f.name, {ORIGIN_SQL}, e.tool, e.session, \
                      COALESCE(e.project, ''), COALESCE(e.model, ''), e.meter \
             ORDER BY w.bid"
        );
        let mut flat: Vec<i64> = Vec::with_capacity(8);
        for i in 0..4 {
            flat.push(plan.cur_start_ms[i]);
            flat.push(plan.now_ms + 1);
        }
        let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(params_from_iter(flat.iter()), |r| {
                Ok(CallFact {
                    bucket: r.get::<_, i64>(0)?.max(0) as u32,
                    kind: call_kind_of(&r.get::<_, String>(1)?),
                    name: r.get(2)?,
                    origin: r.get(3)?,
                    tool: r.get(4)?,
                    session: r.get(5)?,
                    project: opt_of(r.get(6)?),
                    model: opt_of(r.get(7)?),
                    meter: meter_of(&r.get::<_, String>(8)?),
                    counts: TokenCounts {
                        input: r.get(9)?,
                        cache_creation: r.get(10)?,
                        cache_read: r.get(11)?,
                        output: r.get(12)?,
                        reasoning: r.get(13)?,
                        credits: r.get(14)?,
                    },
                    requests: r.get::<_, i64>(15)?.max(0) as u64,
                    nonzero: r.get::<_, i64>(16)?.max(0) as u64,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)
    }

    /// Log-derived quota samples in event-id order, with the obvious dead rows
    /// dropped before they reach Rust. `merge_quotas` re-applies the full
    /// expiry rules; this pre-filter only keeps a month-old index from hauling
    /// every historical sample out of storage on every publish. The contract is
    /// therefore one-directional: this WHERE must stay a *superset* of
    /// `merge_quotas`' survival predicates — anything the fold would keep
    /// (including windowed rows inside [`usage_core::report::LOG_SAMPLE_MAX_AGE_MS`])
    /// must survive this query, or the two report paths silently diverge.
    fn fetch_quotas(&self, now_ms: i64) -> Result<Vec<QuotaFact>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT e.tool, e.ts_ms, q.used_percent, q.window_minutes, q.resets_at_ms, q.label \
                 FROM quota q JOIN event e ON e.id = q.event_id \
                 WHERE q.resets_at_ms >= ?1 OR (q.resets_at_ms = 0 AND e.ts_ms > ?1 - 86400000) \
                 ORDER BY q.event_id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![now_ms], |r| {
                Ok(QuotaFact {
                    tool: r.get(0)?,
                    ts_ms: r.get(1)?,
                    sample: QuotaSample {
                        used_percent: r.get(2)?,
                        window_minutes: r.get(3)?,
                        resets_at_ms: r.get(4)?,
                        label: r.get(5)?,
                        // The index never stored a stable window id; the event
                        // path reads these samples with `id: None` too.
                        id: None,
                    },
                })
            })
            .map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)
    }

    /// The `recent_sessions` list: top-N sessions by last activity, each with
    /// its per-(model, meter) sums from the rollup and its display project
    /// (first non-null by id) and model (last non-null by id) resolved from
    /// `event` via the `(tool, session)` index — 20 sessions means a handful of
    /// indexed point lookups, not a scan.
    ///
    /// Scope is pushed into the `LIMIT` itself: filtering after the fact would
    /// let other machines' sessions fill all 20 slots and starve the scoped
    /// view. `origin` is part of the session key even under `All`; two devices
    /// may legitimately reuse a session id. A named scope restricts the same
    /// queries by `origin` (rollup) or the source prefix (event).
    fn fetch_sessions(&self, limit: usize, scope: &MachineScope) -> Result<Vec<SessionFact>> {
        let (origin, all): (&str, bool) = match scope {
            MachineScope::All => ("", true),
            MachineScope::Local => ("", false),
            MachineScope::Origin { name } => (name.as_str(), false),
        };
        let mut top = self
            .conn
            .prepare(
                "SELECT origin, tool, session, MIN(min_ts), MAX(max_ts) \
                 FROM event_rollup \
                 WHERE (?2 = 1 OR origin = ?3) \
                 GROUP BY origin, tool, session \
                 ORDER BY MAX(max_ts) DESC, origin ASC, tool ASC, session ASC \
                 LIMIT ?1",
            )
            .map_err(sql_err)?;
        let heads: Vec<(String, String, String, i64, i64)> = top
            .query_map(params![limit.max(1) as i64, all, origin], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map_err(sql_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        drop(top);

        let mut groups_stmt = self
            .conn
            .prepare(
                "SELECT model, meter, \
                        IFNULL(sum(in_tok), 0), IFNULL(sum(cc_tok), 0), IFNULL(sum(cr_tok), 0), \
                        IFNULL(sum(out_tok), 0), IFNULL(sum(reason_tok), 0), IFNULL(sum(credits), 0), \
                        sum(n) \
                 FROM event_rollup WHERE origin = ?1 AND tool = ?2 AND session = ?3 \
                 GROUP BY model, meter ORDER BY model, meter",
            )
            .map_err(sql_err)?;
        let mut project_stmt = self
            .conn
            .prepare(&format!(
                "SELECT project FROM event \
                 WHERE tool = ?1 AND session = ?2 AND ({ORIGIN_SQL}) = ?3 \
                   AND project IS NOT NULL \
                 ORDER BY id ASC LIMIT 1",
            ))
            .map_err(sql_err)?;
        let mut model_stmt = self
            .conn
            .prepare(&format!(
                "SELECT model FROM event \
                 WHERE tool = ?1 AND session = ?2 AND ({ORIGIN_SQL}) = ?3 \
                   AND model IS NOT NULL \
                 ORDER BY id DESC LIMIT 1",
            ))
            .map_err(sql_err)?;

        let mut out = Vec::with_capacity(heads.len());
        for (origin, tool, session, first_ms, last_ms) in heads {
            let groups_rows = groups_stmt
                .query_map(params![origin, tool, session], |r| {
                    Ok(SessionGroup {
                        model: opt_of(r.get(0)?),
                        meter: meter_of(&r.get::<_, String>(1)?),
                        counts: TokenCounts {
                            input: r.get(2)?,
                            cache_creation: r.get(3)?,
                            cache_read: r.get(4)?,
                            output: r.get(5)?,
                            reasoning: r.get(6)?,
                            credits: r.get(7)?,
                        },
                        requests: r.get::<_, i64>(8)?.max(0) as u64,
                    })
                })
                .map_err(sql_err)?;
            let groups =
                groups_rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?;
            let project: Option<String> = project_stmt
                .query_row(params![tool, session, origin], |r| r.get(0))
                .optional()
                .map_err(sql_err)?;
            let model: Option<String> = model_stmt
                .query_row(params![tool, session, origin], |r| r.get(0))
                .optional()
                .map_err(sql_err)?;
            out.push(SessionFact {
                origin,
                tool,
                session,
                model,
                project,
                first_ms,
                last_ms,
                groups,
            });
        }
        Ok(out)
    }
}

fn token_percent_is_trustworthy(tool: &str) -> bool {
    matches!(tool, "codex" | "claude" | "antigravity" | "antigravity_ide")
}

/// Percent meters whose token-like reading is plausible but NOT vendor-verified
/// (Cline's usage-limits endpoint is third-party-documented; its own clients
/// never call it). They extrapolate under [`CycleStatus::Unverified`] — shown
/// with a caveat — and only the identity guard is relaxed, because these
/// stores are single-account so the "unknown" identity is stable.
fn percent_is_unverified_token_like(tool: &str) -> bool {
    tool == "cline"
}

/// Claude's weekly Sonnet/Opus rows are separate vendor buckets. Only apply a
/// filter for families the provider names explicitly; generic labels continue
/// to aggregate every local model so we do not invent a model mapping.
fn model_family_hint(label: Option<&str>) -> Option<&'static str> {
    let suffix = label?.split_once(" · ")?.1;
    match suffix.to_ascii_lowercase().as_str() {
        "sonnet" => Some("sonnet"),
        "opus" => Some("opus"),
        "haiku" => Some("haiku"),
        _ => None,
    }
}

/// Recomputes one local day of the rollup inside `tx`, used by the lazy build's
/// day loops (the maintenance paths after deletes have their own in `ingest`).
fn rollup_one_day(tx: &rusqlite::Transaction<'_>, day: chrono::NaiveDate) -> Result<()> {
    let day_key = day.format("%Y-%m-%d").to_string();
    let (lo, hi) = local_day_bounds(day)
        .ok_or_else(|| crate::Error::Sqlite(format!("no local midnight for {day_key}")))?;
    recompute_day_tx(tx, &day_key, lo, hi)
}

/// Groups every event below `up_to` (or all of them, when `None`) into the
/// rollup's `""`-day bucket: timestamps with no representable local day, and,
/// after a capped rebuild, everything older than the retained window. Only
/// `all_time` reads that bucket — which is precisely what the event path does
/// with such rows.
fn sweep_into_rollup(tx: &rusqlite::Transaction<'_>, up_to: Option<i64>) -> Result<()> {
    // `format!` (not a const): the origin expression must be substituted in.
    let sweep = format!(
        "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter, \
            in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts) \
         SELECT '', {ORIGIN_SQL}, tool, session, IFNULL(project, ''), IFNULL(model, ''), meter, \
            IFNULL(sum(in_tok), 0), IFNULL(sum(cc_tok), 0), IFNULL(sum(cr_tok), 0), \
            IFNULL(sum(out_tok), 0), IFNULL(sum(reason_tok), 0), IFNULL(sum(credits), 0), \
            count(*), \
            sum(COALESCE(in_tok, 0) + COALESCE(cc_tok, 0) + COALESCE(cr_tok, 0) \
                + COALESCE(out_tok, 0) + COALESCE(credits, 0) > 0), \
            min(ts_ms), max(ts_ms) \
         FROM event"
    );
    match up_to {
        Some(t) => tx.execute(
            &format!(
                "{sweep} WHERE ts_ms < ?1 \
                 GROUP BY {ORIGIN_SQL}, tool, session, project, model, meter"
            ),
            params![t],
        ),
        None => tx.execute(
            &format!("{sweep} GROUP BY {ORIGIN_SQL}, tool, session, project, model, meter"),
            [],
        ),
    }
    .map_err(sql_err)?;
    Ok(())
}

// ---- golden parity tests -----------------------------------------------------
//
// The law: for the same underlying events, `summarize_facts(report_facts(..))`
// must equal `summarize(all_events())` field by field. Strings, integers and
// orderings compare exactly; f64 compares with a 1e-9 relative tolerance
// because the rollup path sums per group where the event path sums per event —
// mathematically identical, floating-point-apart.

#[cfg(test)]
mod golden {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    use std::sync::Mutex;

    use chrono::{Local, TimeZone};
    use usage_core::pricing::{PricingMap, PricingOptions};
    use usage_core::report::HEATMAP_DAYS;
    use usage_core::{
        summarize, summarize_facts, Budget, Call, CallKind, DateFilter, DetectedSource, FileKind,
        Item, Meter, QuotaOrigin, QuotaView, ReadCursor, ReadOutcome, ReportOptions, Semantics,
        SourceAdapter, SourceFile, Summary, UsageEvent, Window,
    };

    /// Fixture instant: Wednesday 2026-09-30 14:00 local. This machine is
    /// UTC+8 with no DST, so every local midnight resolves uniquely and the
    /// ISO week starts Monday 2026-09-28.
    fn ms_of(y: i32, mo: u32, d: u32, h: u32) -> i64 {
        Local.with_ymd_and_hms(y, mo, d, h, 0, 0).single().expect("valid local time").timestamp_millis()
    }
    fn now_fixture() -> i64 {
        ms_of(2026, 9, 30, 14)
    }

    fn pricing() -> PricingMap {
        PricingMap::load(&PricingOptions { offline: true, cache_dir: None, overrides: Default::default() })
    }

    fn counts(input: f64) -> TokenCounts {
        TokenCounts { input, ..Default::default() }
    }

    fn ev(tool: &str, ts: i64, session: &str, model: Option<&str>, project: Option<&str>, c: TokenCounts) -> UsageEvent {
        UsageEvent {
            model: model.map(str::to_string),
            project: project.map(str::to_string),
            ..UsageEvent::new(tool, ts, session).with(c)
        }
    }

    /// The dense fixture. Boundaries it deliberately stands on:
    /// - exact local midnights (E3, E13) and 23:59:59.999 (E4),
    /// - prev-slice heads in and out (E7/E8, E10/E11, E19 vs E5),
    /// - whole rollup days inside prev windows (E6, E9),
    /// - the cur-year boundary (E13) and a pre-heatmap day (E12: in prev-year
    ///   yet outside the heatmap window, and within the 402-day build cap so
    ///   the lazy build's forward day loop represents it exactly),
    /// - reasoning tokens (E1), zero-token rows (E14), unpriced filed/not
    ///   (E15 vs E14), credits with a plan rate (E10/E11) and without (E16),
    /// - NULL project and model (E3, E7, E8, E16, E20..), never `Some("")`,
    /// - two mcps + a skill on one event: only the first of each kind names it (E17),
    /// - quota ties resolved by event id (E20/E21), a probe-override window
    ///   (codex weekly), a fresh no-reset row (E22) and an expired one (E23).
    fn batch_one() -> Vec<UsageEvent> {
        let day = |h: u32| ms_of(2026, 9, 30, h);
        vec![
            UsageEvent {
                calls: vec![Call { kind: CallKind::Mcp, name: "bugx".into() }],
                ..ev("claude", ms_of(2026, 9, 30, 9) + 15 * 60_000, "s1", Some("glm-4.7"), Some("/work/app"),
                    TokenCounts { input: 1_000_000.0, output: 2000.0, reasoning: 500.0, ..Default::default() })
            },
            ev("claude", ms_of(2026, 9, 30, 13) + 59 * 60_000 + 999, "s1", Some("glm-4.7"), Some("/work/app"), counts(500.0)),
            UsageEvent {
                calls: vec![Call { kind: CallKind::Skill, name: "review".into() }],
                ..ev("codex", day(0), "s2", None, None, counts(300.0))
            },
            ev("claude", ms_of(2026, 9, 29, 23) + 59 * 60_000 + 999, "s1", Some("glm-4.7"), Some("/work/app"), counts(700.0)),
            ev("claude", ms_of(2026, 9, 29, 10), "s3", Some("glm-4.7"), Some("/work/app"), counts(5_000_000.0)),
            ev("claude", ms_of(2026, 9, 25, 12), "s3", Some("glm-4.7"), Some("/work/app"), counts(11_000.0)),
            ev("claude", ms_of(2026, 9, 24, 15), "s4", None, None, counts(900.0)),
            ev("claude", ms_of(2026, 9, 24, 13), "s4", None, None, counts(60.0)),
            UsageEvent {
                // output only: cached_pct exercises the cache_read denominator path.
                ..ev("claude", ms_of(2026, 8, 15, 10), "s5", Some("gpt-5"), Some("/other"),
                    TokenCounts { input: 2_000_000.0, cache_read: 400_000.0, ..Default::default() })
            },
            credits_ev("qoder", ms_of(2026, 8, 1, 15), "s6", Some("qmodel"), 3.5),
            credits_ev("qoder", ms_of(2026, 8, 1, 13), "s6", Some("qmodel"), 0.25),
            ev("claude", ms_of(2025, 9, 1, 8), "s7", Some("claude-sonnet-4-6"), Some("/old"), counts(40_000.0)),
            ev("codex", ms_of(2026, 1, 1, 0), "s8", Some("gpt-5"), None, counts(100.0)),
            ev("codex", day(10), "s2", Some("mystery-model"), None, TokenCounts::default()),
            ev("codex", day(10) + 30 * 60_000, "s2", Some("mystery-model"), None, counts(4321.0)),
            credits_ev("claude", ms_of(2026, 9, 30, 11), "s9", None, 2.0),
            UsageEvent {
                calls: vec![
                    Call { kind: CallKind::Mcp, name: "bugx".into() },
                    Call { kind: CallKind::Mcp, name: "other".into() },
                    Call { kind: CallKind::Skill, name: "review".into() },
                ],
                ..ev("claude", ms_of(2026, 9, 30, 11) + 30 * 60_000, "s1", Some("glm-4.7"), Some("/work/app"), counts(800.0))
            },
            // E20/E21 sit inside the hour: a windowed log row older than
            // LOG_SAMPLE_MAX_AGE_MS is not this run's answer (report.rs), so
            // the tie's subjects must be fresh to reach merge_quotas at all.
            // E22 stays outside it on purpose — its no-reset arm is judged by
            // the day horizon, and it is the canary for arm-order regressions.
            quota_ev("codex", day(13) + 5 * 60_000, "s2", 20.0, 300, now_fixture() + 3_600_000, None),
            quota_ev("codex", day(13) + 35 * 60_000, "s2", 45.0, 300, now_fixture() + 3_000_000, None),
            quota_ev("zcode", day(12) + 40 * 60_000, "s10", 38.0, 300, 0, Some("5 小时")),
            quota_ev("zcode", ms_of(2026, 9, 29, 9), "s10", 99.0, 216_000, 0, Some("ZCode MCP")),
        ]
    }

    /// Same session as batch one's s1 events: the rollup's per-day upsert must
    /// merge this into the groups the first pass created.
    fn batch_two() -> Vec<UsageEvent> {
        vec![
            ev("claude", ms_of(2026, 9, 30, 13) + 30 * 60_000, "s1", Some("glm-4.7"), Some("/work/app"), counts(900.0)),
            ev("claude", ms_of(2026, 9, 29, 16), "s3", Some("glm-4.7"), Some("/work/app"), counts(222.0)),
        ]
    }

    fn credits_ev(tool: &str, ts: i64, session: &str, model: Option<&str>, credits: f64) -> UsageEvent {
        let mut e = ev(tool, ts, session, model, None, TokenCounts { credits, ..Default::default() });
        e.meter = Meter::Credits;
        e
    }

    fn quota_ev(tool: &str, ts: i64, session: &str, used: f64, window: i64, resets: i64, label: Option<&str>) -> UsageEvent {
        let mut e = ev(tool, ts, session, Some("gpt-5"), None, counts(10.0));
        e.quota = Some(QuotaSample { used_percent: used, window_minutes: window, resets_at_ms: resets, label: label.map(str::to_string), id: None });
        e
    }

    /// An adapter whose `read` pops one queued batch per call, with a cursor
    /// that always advances. `discover` bumps its mtime per pass, so pass N+1
    /// re-reads; tests shrink `size` to stage a rewrite for the purge path.
    struct Mock {
        path: PathBuf,
        batches: Mutex<Vec<Vec<UsageEvent>>>,
        size: AtomicU64,
        seq: AtomicI64,
    }

    impl Mock {
        fn new(path: PathBuf, first: Vec<UsageEvent>) -> Self {
            Self { path, batches: Mutex::new(vec![first]), size: AtomicU64::new(100), seq: AtomicI64::new(0) }
        }
        fn push_batch(&self, batch: Vec<UsageEvent>) {
            self.batches.lock().unwrap().push(batch);
        }
        fn set_size(&self, size: u64) {
            self.size.store(size, Ordering::SeqCst);
        }
    }

    impl SourceAdapter for Mock {
        fn id(&self) -> &'static str {
            "mock"
        }
        fn display_name(&self) -> &'static str {
            "Mock"
        }
        fn semantics(&self) -> Semantics {
            Semantics::TOKENS_PER_CALL_INLINE
        }
        fn probe(&self) -> Option<DetectedSource> {
            None
        }
        fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
            let mtime = 1_780_000_000_000 + self.seq.fetch_add(1_000, Ordering::SeqCst);
            vec![SourceFile {
                path: self.path.clone(),
                kind: FileKind::Jsonl,
                size: self.size.load(Ordering::SeqCst),
                mtime_ms: mtime,
            }]
        }
        fn read(&self, _file: &SourceFile, cursor: ReadCursor) -> usage_core::Result<ReadOutcome> {
            let mut q = self.batches.lock().unwrap();
            let batch = if q.is_empty() { Vec::new() } else { q.remove(0) };
            Ok(ReadOutcome { events: batch, cursor: ReadCursor(cursor.0 + 1) })
        }
    }

    fn golden_opts<'a>(p: &'a PricingMap) -> ReportOptions<'a> {
        let mut budgets = BTreeMap::new();
        budgets.insert("zcode".to_string(), Budget { daily_usd: 10.0, monthly_usd: 0.0 });
        let mut opts = ReportOptions::new(p)
            .with_now(now_fixture())
            .with_quota(vec![QuotaView {
                tool: "codex".into(),
                used_percent: 7.5,
                window_minutes: 10_080,
                resets_at_ms: now_fixture() + 5 * 86_400_000,
                sampled_at_ms: now_fixture(),
                label: Some("周".into()),
                id: None,
                origin: QuotaOrigin::Probe,
                cycle: None,
            }])
            .with_budgets(budgets);
        // Same limit both paths see: the plan was built with 3, so the event
        // path must truncate at 3 too or the comparison is meaningless.
        opts.recent_session_limit = 3;
        opts
    }

    fn plan() -> AggregatePlan {
        AggregatePlan::build(now_fixture(), 3)
    }

    // ---- the comparator ----

    fn rel_eq(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * f64::max(1.0, f64::max(a.abs(), b.abs()))
    }

    fn assert_f64(path: &str, a: f64, b: f64) {
        assert!(rel_eq(a, b), "{path}: {a} != {b}");
    }

    fn assert_counts_eq(path: &str, a: &TokenCounts, b: &TokenCounts) {
        assert_f64(&format!("{path}.input"), a.input, b.input);
        assert_f64(&format!("{path}.cache_creation"), a.cache_creation, b.cache_creation);
        assert_f64(&format!("{path}.cache_read"), a.cache_read, b.cache_read);
        assert_f64(&format!("{path}.output"), a.output, b.output);
        assert_f64(&format!("{path}.reasoning"), a.reasoning, b.reasoning);
        assert_f64(&format!("{path}.credits"), a.credits, b.credits);
    }

    fn assert_summary_eq(path: &str, a: &Summary, b: &Summary) {
        assert_counts_eq(path, &a.counts, &b.counts);
        assert_f64(&format!("{path}.total_tokens"), a.total_tokens, b.total_tokens);
        assert_f64(&format!("{path}.cost"), a.cost, b.cost);
        assert_f64(&format!("{path}.credits"), a.credits, b.credits);
        assert_f64(&format!("{path}.credit_cost"), a.credit_cost, b.credit_cost);
        assert_eq!(a.requests, b.requests, "{path}.requests");
        assert_eq!(a.sessions, b.sessions, "{path}.sessions");
        assert_f64(&format!("{path}.cached_pct"), a.cached_pct, b.cached_pct);
        assert_eq!(a.unpriced.len(), b.unpriced.len(), "{path}.unpriced: {:?} vs {:?}", a.unpriced, b.unpriced);
        for (i, (x, y)) in a.unpriced.iter().zip(&b.unpriced).enumerate() {
            let p = &format!("{path}.unpriced[{i}]");
            assert_eq!(x.tool, y.tool, "{p}.tool");
            assert_eq!(x.model, y.model, "{p}.model");
            assert_eq!(x.requests, y.requests, "{p}.requests");
            assert_f64(p, x.total_tokens, y.total_tokens);
        }
    }

    fn assert_items_eq(path: &str, a: &[Item], b: &[Item]) {
        assert_eq!(a.len(), b.len(), "{path} keys {:?} vs {:?}", a.iter().map(|i| &i.key).collect::<Vec<_>>(), b.iter().map(|i| &i.key).collect::<Vec<_>>());
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            let p = &format!("{path}[{i}]");
            assert_eq!(x.key, y.key, "{p}.key");
            assert_eq!(x.label, y.label, "{p}.label");
            assert_eq!(x.priced, y.priced, "{p}.priced");
            assert_eq!(x.requests, y.requests, "{p}.requests");
            assert_eq!(x.sessions, y.sessions, "{p}.sessions");
            assert_counts_eq(p, &x.counts, &y.counts);
            assert_f64(&format!("{p}.total_tokens"), x.total_tokens, y.total_tokens);
            assert_f64(&format!("{p}.cost"), x.cost, y.cost);
        }
    }

    fn assert_breakdown_eq(path: &str, a: &usage_core::Breakdown, b: &usage_core::Breakdown) {
        assert_items_eq(&format!("{path}.tools"), &a.tools, &b.tools);
        assert_items_eq(&format!("{path}.models"), &a.models, &b.models);
        assert_items_eq(&format!("{path}.projects"), &a.projects, &b.projects);
        assert_items_eq(&format!("{path}.mcps"), &a.mcps, &b.mcps);
        assert_items_eq(&format!("{path}.skills"), &a.skills, &b.skills);
    }

    fn assert_window_eq(path: &str, a: &Window, b: &Window) {
        assert_eq!(a.key, b.key, "{path}.key");
        assert_eq!(a.label, b.label, "{path}.label");
        assert_eq!(a.start_ms, b.start_ms, "{path}.start_ms");
        assert_eq!(a.end_ms, b.end_ms, "{path}.end_ms");
        assert_f64(&format!("{path}.delta_cost_pct"), a.delta_cost_pct, b.delta_cost_pct);
        assert_f64(&format!("{path}.delta_tokens_pct"), a.delta_tokens_pct, b.delta_tokens_pct);
        assert_summary_eq(&format!("{path}.summary"), &a.summary, &b.summary);
        assert_summary_eq(&format!("{path}.prev"), &a.prev, &b.prev);
        assert_breakdown_eq(&format!("{path}.breakdown"), &a.breakdown, &b.breakdown);
    }

    fn assert_report_eq(a: &usage_core::Report, b: &usage_core::Report) {
        assert_eq!(a.generated_at_ms, b.generated_at_ms);
        assert_eq!(a.utc_offset, b.utc_offset);
        assert_window_eq("day", &a.day, &b.day);
        assert_window_eq("week", &a.week, &b.week);
        assert_window_eq("month", &a.month, &b.month);
        assert_window_eq("year", &a.year, &b.year);
        assert_eq!(a.heatmap.len(), b.heatmap.len());
        assert_eq!(a.heatmap.len() as i64, HEATMAP_DAYS);
        for (i, (x, y)) in a.heatmap.iter().zip(&b.heatmap).enumerate() {
            assert_eq!(x.date, y.date, "heatmap[{i}]");
            assert_eq!(x.requests, y.requests, "heatmap[{} {}]", i, x.date);
            assert_f64(&format!("heatmap[{} {}].tokens", i, x.date), x.total_tokens, y.total_tokens);
            assert_f64(&format!("heatmap[{} {}].cost", i, x.date), x.cost, y.cost);
        }
        assert_eq!(a.hourly.len(), b.hourly.len());
        for (h, (x, y)) in a.hourly.iter().zip(&b.hourly).enumerate() {
            assert_eq!(x.requests, y.requests, "hourly[{h}].requests");
            assert_f64(&format!("hourly[{h}].tokens"), x.total_tokens, y.total_tokens);
            assert_f64(&format!("hourly[{h}].cost"), x.cost, y.cost);
        }
        assert_eq!(a.hourly_prev.len(), b.hourly_prev.len());
        assert_eq!(a.hourly_prev.len(), 24, "hourly_prev is always the full day");
        for (h, (x, y)) in a.hourly_prev.iter().zip(&b.hourly_prev).enumerate() {
            assert_eq!(x.requests, y.requests, "hourly_prev[{h}].requests");
            assert_f64(&format!("hourly_prev[{h}].tokens"), x.total_tokens, y.total_tokens);
            assert_f64(&format!("hourly_prev[{h}].cost"), x.cost, y.cost);
        }
        assert_eq!(a.quotas.len(), b.quotas.len(), "quotas {:?} vs {:?}", a.quotas.iter().map(|q| (&q.tool, q.window_minutes, q.used_percent)).collect::<Vec<_>>(), b.quotas.iter().map(|q| (&q.tool, q.window_minutes, q.used_percent)).collect::<Vec<_>>());
        for (i, (x, y)) in a.quotas.iter().zip(&b.quotas).enumerate() {
            let p = format!("quotas[{i}]");
            assert_eq!(x.tool, y.tool, "{p}.tool");
            assert_eq!(x.window_minutes, y.window_minutes, "{p}.window");
            assert_eq!(x.resets_at_ms, y.resets_at_ms, "{p}.resets");
            assert_eq!(x.sampled_at_ms, y.sampled_at_ms, "{p}.sampled");
            assert_eq!(x.label, y.label, "{p}.label");
            assert_eq!(x.id, y.id, "{p}.id");
            assert_eq!(x.origin, y.origin, "{p}.origin");
            assert_f64(&format!("{p}.used_percent"), x.used_percent, y.used_percent);
        }
        assert_eq!(a.sources, b.sources);
        assert_eq!(a.pricing, b.pricing);
        assert_eq!(a.recent_sessions.len(), b.recent_sessions.len(), "recent_sessions");
        for (i, (x, y)) in a.recent_sessions.iter().zip(&b.recent_sessions).enumerate() {
            let p = format!("recent_sessions[{i}]");
            assert_eq!(x.origin, y.origin, "{p}.origin");
            assert_eq!(x.tool, y.tool, "{p}.tool");
            assert_eq!(x.session, y.session, "{p}.session");
            assert_eq!(x.project, y.project, "{p}.project");
            assert_eq!(x.model, y.model, "{p}.model");
            assert_eq!(x.first_ms, y.first_ms, "{p}.first_ms");
            assert_eq!(x.last_ms, y.last_ms, "{p}.last_ms");
            assert_eq!(x.requests, y.requests, "{p}.requests");
            assert_f64(&format!("{p}.total_tokens"), x.total_tokens, y.total_tokens);
            assert_f64(&format!("{p}.cost"), x.cost, y.cost);
        }
        assert_eq!(a.scope, b.scope, "scope echo");
        assert_eq!(a.machines.len(), b.machines.len(), "machines {:?} vs {:?}", a.machines, b.machines);
        for (i, (x, y)) in a.machines.iter().zip(&b.machines).enumerate() {
            assert_eq!(x.origin, y.origin, "machines[{i}].origin");
            assert_f64(&format!("machines[{i}].today_tokens"), x.today_tokens, y.today_tokens);
        }
        assert_summary_eq("all_time", &a.all_time, &b.all_time);
    }

    // ---- the tests ----

    /// Path (a): the dense fixture through the public ingest, in two passes so
    /// one session spans both; the rollup is maintained incrementally, so the
    /// reads never rebuild (`rebuilt` stays false) and a second `report_facts`
    /// returns facts identical to the first.
    #[test]
    fn golden_parity_through_ingest_across_two_passes() {
        let p = pricing();
        let mock = Mock::new(PathBuf::from("/fixture/mock.jsonl"), batch_one());
        let mut idx = Index::open_in_memory().unwrap();

        let one = idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        assert_eq!(one.new_events, batch_one().len() as u64);

        mock.push_batch(batch_two());
        let two = idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        assert_eq!(two.new_events, batch_two().len() as u64);
        assert_eq!(two.purged, 0);

        let opts = golden_opts(&p);
        let oracle = summarize(&idx.all_events().unwrap(), &opts);

        let plan = plan();
        let facts_first = idx.report_facts(&plan, &MachineScope::All).unwrap();
        assert!(!facts_first.rebuilt, "the write path maintains the rollup; no rebuild expected");
        let mine = summarize_facts(&facts_first, &opts);
        assert_report_eq(&oracle, &mine);

        let facts_second = idx.report_facts(&plan, &MachineScope::All).unwrap();
        assert_eq!(facts_first, facts_second, "a second read must be byte-stable");
        let mine_again = summarize_facts(&facts_second, &opts);
        assert_report_eq(&oracle, &mine_again);
    }

    #[test]
    fn all_scope_keeps_same_session_name_per_origin() {
        let mut idx = Index::open_in_memory().unwrap();
        let ts = now_fixture();
        idx.conn
            .execute(
                "INSERT INTO event(tool, ts_ms, session, project, model, meter, in_tok, source) VALUES('codex', ?1, 'same-session', '/local', 'gpt-5', 'tokens', 10, '/local/log')",
                params![ts - 1],
            )
            .unwrap();
        idx.conn
            .execute(
                "INSERT INTO event(tool, ts_ms, session, project, model, meter, in_tok, source) VALUES('codex', ?1, 'same-session', '/remote', 'gpt-5', 'tokens', 20, 'linux:remote:row-1')",
                params![ts],
            )
            .unwrap();

        let facts = idx.report_facts(&plan(), &MachineScope::All).unwrap();
        assert!(facts.rebuilt);
        assert_eq!(facts.sessions.len(), 2);
        assert_eq!(facts.sessions.iter().map(|s| s.origin.as_str()).collect::<Vec<_>>(), vec!["remote", ""]);
        let mut live_origins = facts.live.iter().filter(|f| f.bucket == LIVE_TODAY).map(|f| (f.origin.as_str(), f.requests)).collect::<Vec<_>>();
        live_origins.sort_by_key(|(origin, _)| *origin);
        assert_eq!(live_origins, vec![("", 1), ("remote", 1)]);
        let report = summarize_facts(&facts, &golden_opts(&pricing()));
        assert_eq!(report.recent_sessions.len(), 2);
        assert_eq!(report.recent_sessions.iter().map(|s| s.origin.as_str()).collect::<Vec<_>>(), vec!["remote", ""]);
        assert_eq!(report.recent_sessions.iter().map(|s| s.total_tokens).collect::<Vec<_>>(), vec![20.0, 10.0]);
        assert_eq!(report.day.summary.sessions, 2);
        assert_eq!(report.day.breakdown.tools[0].sessions, 2);
    }

    /// Raw-insert helper for path (b): replicates persist's mapping (ids in
    /// insertion order) without touching the rollup, so a lazy rebuild has to
    /// produce it.
    fn raw_insert(idx: &Index, events: &[UsageEvent]) {
        let tx = idx.conn.unchecked_transaction().unwrap();
        for e in events {
            let c = &e.counts;
            let changed = tx
                .execute(
                    crate::INSERT_EVENT,
                    params![
                        e.tool, e.ts_ms, e.session, e.project, e.model,
                        crate::meter_str(e.meter), c.input, c.cache_creation, c.cache_read,
                        c.output, c.reasoning, c.credits, e.dedupe_key, "/fixture/raw.jsonl",
                    ],
                )
                .unwrap();
            assert_eq!(changed, 1, "raw fixture rows never collide");
            let id = tx.last_insert_rowid();
            for call in &e.calls {
                tx.execute(
                    "INSERT INTO call(event_id, kind, name) VALUES(?1,?2,?3)",
                    params![id, crate::call_kind_str(call.kind), call.name],
                )
                .unwrap();
            }
            if let Some(q) = e.quota.as_ref() {
                tx.execute(
                    "INSERT OR REPLACE INTO quota(event_id, used_percent, window_minutes, resets_at_ms, label) \
                     VALUES(?1,?2,?3,?4,?5)",
                    params![id, q.used_percent, q.window_minutes, q.resets_at_ms, q.label],
                )
                .unwrap();
            }
        }
        tx.commit().unwrap();
    }

    /// Path (b): the same fixture rows inserted raw — replicating persist's
    /// mapping, ids in insertion order — but with NO rollup rows, so the first
    /// read must lazily rebuild the rollup from `event` and still land exactly
    /// on the oracle. The second read rides the rebuilt rollup.
    #[test]
    fn lazy_rebuild_from_raw_inserts_matches_the_oracle() {
        let p = pricing();
        let idx = Index::open_in_memory().unwrap();
        let events: Vec<UsageEvent> =
            batch_one().into_iter().chain(batch_two()).collect();
        // `report_facts` takes `&mut self`; a raw-insert setup only needs the
        // connection, so the mutability is introduced afterwards.
        let mut idx = idx;
        raw_insert(&idx, &events);

        let opts = golden_opts(&p);
        let oracle = summarize(&idx.all_events().unwrap(), &opts);
        let plan = plan();

        let facts_first = idx.report_facts(&plan, &MachineScope::All).unwrap();
        assert!(facts_first.rebuilt, "an empty rollup must be lazily rebuilt");
        assert_report_eq(&oracle, &summarize_facts(&facts_first, &opts));

        let facts_second = idx.report_facts(&plan, &MachineScope::All).unwrap();
        assert!(!facts_second.rebuilt);
        // `rebuilt` is diagnostic: the first read did build, the second rode
        // the result. Parity is about content, which must be byte-stable.
        let mut facts_first = facts_first;
        facts_first.rebuilt = false;
        assert_eq!(facts_first, facts_second);
        assert_report_eq(&oracle, &summarize_facts(&facts_second, &opts));
    }

    /// A span wider than the 402-day build cap (only reachable through manual
    /// tampering or a broken prune — retention caps healthy indexes at 400
    /// days) takes the backward branch: the newest 402 days are rebuilt as
    /// whole days, everything older sweeps into the `""` day. The sweep keeps
    /// all_time exact; dated windows knowingly lose the ancient events, which
    /// is the accepted stale-proof degradation (an empty rollup costs one
    /// rebuild, a wrong one lies).
    #[test]
    fn a_span_older_than_the_build_cap_sweeps_into_the_empty_day() {
        let p = pricing();
        let idx = Index::open_in_memory().unwrap();
        let mut idx = idx;
        raw_insert(
            &idx,
            &[
                ev("claude", ms_of(2024, 1, 1, 8), "old", Some("glm-4.7"), Some("/old"), counts(100.0)),
                ev("claude", ms_of(2024, 3, 2, 9), "old", Some("glm-4.7"), Some("/old"), counts(50.0)),
                ev("codex", ms_of(2026, 9, 30, 9), "new", Some("gpt-5"), None, counts(7.0)),
            ],
        );

        let facts = idx.report_facts(&plan(), &MachineScope::All).unwrap();
        assert!(facts.rebuilt);

        // No dated rollup row exists for the ancient days; they live in the
        // `""` bucket, summed together.
        assert!(facts.rollup.iter().all(|r| r.day != "2024-01-01" && r.day != "2024-03-02"));
        let swept = facts
            .rollup
            .iter()
            .find(|r| r.day.is_empty())
            .expect("pre-cap events sweep into the empty-day bucket");
        assert_eq!(swept.tool, "claude");
        assert_eq!(swept.counts.input, 150.0);
        assert_eq!(swept.n, 2);

        // The retained day is exact.
        let kept = facts
            .rollup
            .iter()
            .find(|r| r.tool == "codex")
            .expect("the modern day survives the capped build");
        assert_eq!(kept.counts.input, 7.0);

        // all_time still counts every event, so total parity holds.
        let opts = golden_opts(&p);
        let oracle = summarize(&idx.all_events().unwrap(), &opts);
        let mine = summarize_facts(&facts, &opts);
        assert_eq!(mine.all_time.requests, oracle.all_time.requests, "all_time keeps swept events");
        assert_f64("all_time.input", oracle.all_time.counts.input, mine.all_time.counts.input);
        assert_f64("all_time.cost", oracle.all_time.cost, mine.all_time.cost);
    }

    /// A rewritten log purges its old rows and re-reads from scratch; the
    /// rollup must follow within the same pass, not just after a rebuild.
    #[test]
    fn a_rewritten_file_purges_and_the_rollup_follows() {
        let p = pricing();
        let mock = Mock::new(
            PathBuf::from("/fixture/rewrite.jsonl"),
            vec![
                ev("claude", ms_of(2026, 9, 30, 9), "s1", Some("glm-4.7"), Some("/work/app"), counts(1_000.0)),
                ev("claude", ms_of(2026, 9, 29, 9), "s1", Some("glm-4.7"), Some("/work/app"), counts(2_000.0)),
            ],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        // Shrink = rewrite: purge both rows, then read the replacement bytes.
        mock.set_size(99);
        mock.push_batch(vec![
            ev("claude", ms_of(2026, 9, 30, 9), "s1", Some("glm-4.7"), Some("/work/app"), counts(7.0)),
            ev("codex", ms_of(2026, 9, 30, 10), "s2", Some("gpt-5"), None, counts(9.0)),
        ]);
        let report = idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        assert_eq!(report.purged, 2, "the rewritten file's old rows went away");
        assert_eq!(report.new_events, 2);

        let opts = golden_opts(&p);
        let oracle = summarize(&idx.all_events().unwrap(), &opts);
        assert_report_eq(&oracle, &summarize_facts(&idx.report_facts(&plan(), &MachineScope::All).unwrap(), &opts));
        // The purged spend is really gone from the rollup, not just from event.
        assert!(
            !idx.report_facts(&plan(), &MachineScope::All).unwrap().rollup.iter().any(|r| r.counts.input == 1_000.0 || r.counts.input == 2_000.0),
            "purged tokens must not survive in any rollup group"
        );
    }

    /// Prune drops a contiguous prefix; only the doomed rows' days may be
    /// recomputed, and the boundary event exactly at the cutoff survives.
    #[test]
    fn prune_repairs_exactly_the_affected_days() {
        let p = pricing();
        let mock = Mock::new(
            PathBuf::from("/fixture/prune.jsonl"),
            vec![
                ev("claude", ms_of(2026, 8, 15, 10), "old", Some("glm-4.7"), None, counts(5_000.0)),
                ev("codex", ms_of(2026, 9, 1, 0), "edge", Some("gpt-5"), None, counts(60.0)),
                ev("claude", ms_of(2026, 9, 30, 9), "new", Some("glm-4.7"), None, counts(300.0)),
            ],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();

        let removed = idx.prune(ms_of(2026, 9, 1, 0)).unwrap();
        assert_eq!(removed, 1, "only the August row is older than the cutoff");

        let opts = golden_opts(&p);
        let oracle = summarize(&idx.all_events().unwrap(), &opts);
        assert_eq!(oracle.all_time.requests, 2, "the boundary event survived");
        assert_report_eq(&oracle, &summarize_facts(&idx.report_facts(&plan(), &MachineScope::All).unwrap(), &opts));
    }

    /// `clear` empties the rollup along with everything else; a re-ingest
    /// repopulates both and the parity law holds again.
    #[test]
    fn clear_empties_the_rollup_and_reingest_restores_parity() {
        let p = pricing();
        let mock = Mock::new(PathBuf::from("/fixture/clear.jsonl"), batch_one());
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();

        idx.clear().unwrap();
        assert_eq!(idx.event_count().unwrap(), 0);
        let facts = idx.report_facts(&plan(), &MachineScope::All).unwrap();
        assert!(facts.rollup.is_empty() && facts.live.is_empty() && facts.calls.is_empty());
        assert!(!facts.rebuilt, "an empty index has nothing to rebuild");

        // Re-ingest the same bytes from scratch (a fresh cursor-0 read).
        let empty: Vec<UsageEvent> = Vec::new();
        let _ = empty;
        let replay = batch_one();
        let mock2 = Mock::new(PathBuf::from("/fixture/clear.jsonl"), replay);
        idx.ingest_adapter(&mock2, &DateFilter::default()).unwrap();

        let opts = golden_opts(&p);
        let oracle = summarize(&idx.all_events().unwrap(), &opts);
        let facts = idx.report_facts(&plan(), &MachineScope::All).unwrap();
        assert!(!facts.rebuilt, "the re-ingest maintained the rollup again");
        assert_report_eq(&oracle, &summarize_facts(&facts, &opts));
    }

    /// An empty index reports the empty report, and the two paths agree on it.
    #[test]
    fn an_empty_index_reports_empty_facts_with_parity() {
        let p = pricing();
        let mut idx = Index::open_in_memory().unwrap();
        let opts = golden_opts(&p);
        let oracle = summarize(&idx.all_events().unwrap(), &opts);
        let facts = idx.report_facts(&plan(), &MachineScope::All).unwrap();
        assert!(facts.rollup.is_empty());
        assert!(!facts.rebuilt);
        assert_report_eq(&oracle, &summarize_facts(&facts, &opts));
    }

    /// The scoped fold, against its golden twin: with a second machine's
    /// bundle merged, `Local` and `Origin` each fold exactly that machine's
    /// events on both report paths, `machines` still lists every machine under
    /// every scope (that is what the switcher draws), and the two scoped folds
    /// partition the unscoped one.
    #[test]
    fn scoped_folds_match_the_event_path_oracle() {
        let p = pricing();

        // A "remote" machine's own index, exported as its bundle. The huge
        // `days` keeps this deterministic: the export window is cut against
        // the *wall clock*, and these fixtures are dated 2026-09-30, so the
        // window must stay wider than any clock this test will ever see.
        let mut remote = Index::open_in_memory().unwrap();
        let remote_mock = Mock::new(PathBuf::from("/fixture/remote.jsonl"), batch_one());
        remote.ingest_adapter(&remote_mock, &DateFilter::default()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bundle = remote
            .export_sync(&crate::sync::ExportOptions {
                days: 5_000_000,
                out_dir: dir.path().to_path_buf(),
                origin: "ops-box".into(),
            })
            .unwrap();

        // This machine: its own fresh batch, plus the merged remote bundle.
        let mut idx = Index::open_in_memory().unwrap();
        let mock = Mock::new(PathBuf::from("/fixture/local.jsonl"), batch_two());
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        let rep = idx
            .import_sync(&crate::sync::ImportOptions { file: bundle.gz_path, dry_run: false })
            .unwrap();
        assert!(rep.inserted > 0, "the bundle merged onto the local index");

        let events = idx.all_events().unwrap();
        assert!(
            events.iter().any(|e| e.source.starts_with("linux:ops-box:")),
            "imported rows carry the origin-stamped source"
        );
        let facts_all = idx.report_facts(&plan(), &MachineScope::All).unwrap();
        assert!(!facts_all.rebuilt, "ingest and the merge both maintain the rollup");

        // The sessions' top-N is resolved in SQL, so — exactly like the panel
        // does on every publish — each scope gets its own `report_facts` call;
        // everything else in the fold re-filters the same facts object.
        for scope in [
            MachineScope::All,
            MachineScope::Local,
            MachineScope::Origin { name: "ops-box".into() },
        ] {
            let mut opts = golden_opts(&p);
            opts.scope = scope.clone();
            let facts = idx.report_facts(&plan(), &scope).unwrap();
            let oracle = summarize(&events, &opts);
            assert_report_eq(&oracle, &summarize_facts(&facts, &opts));
        }
        let facts = facts_all;

        // The menu material: both machines under a scoped report, local first
        // ("" sorts first), the remote's today volume real.
        let mut opts = golden_opts(&p);
        opts.scope = MachineScope::Local;
        let local = summarize_facts(&facts, &opts);
        let names: Vec<&str> = local.machines.iter().map(|m| m.origin.as_str()).collect();
        assert_eq!(names, vec!["", "ops-box"], "{:?}", local.machines);
        assert!(local.machines[1].today_tokens > 0.0, "{:?}", local.machines);

        // The scoped folds partition the unscoped one on the day window.
        let all = summarize_facts(&facts, &golden_opts(&p));
        let mut opts = golden_opts(&p);
        opts.scope = MachineScope::Origin { name: "ops-box".into() };
        let remote_r = summarize_facts(&facts, &opts);
        assert_f64(
            "day tokens partition",
            local.day.summary.total_tokens + remote_r.day.summary.total_tokens,
            all.day.summary.total_tokens,
        );
        assert_f64("day cost partition", local.day.summary.cost + remote_r.day.summary.cost, all.day.summary.cost);
    }

    /// The upgrade path for the origin column: an index written by the older
    /// build holds a 6-column-PK `event_rollup` (no origin, and no
    /// `schema_version` bump either — it is a cache). Opening it again must
    /// drop that table, not silently keep it under `CREATE TABLE IF NOT
    /// EXISTS` (which would make every scoped read fail on the missing
    /// column), and the first read rebuilds the modern shape from `event`.
    #[test]
    fn legacy_rollup_is_dropped_on_open_and_rebuilt_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let mut idx = Index::open(&path).unwrap();
            let mock = Mock::new(PathBuf::from("/fixture/legacy.jsonl"), batch_one());
            idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
            // Swap in the pre-origin shape, plus one row of legacy junk that
            // must not survive into the rebuilt table.
            idx.conn()
                .execute_batch(
                    "DROP TABLE event_rollup; \
                     CREATE TABLE event_rollup(day TEXT NOT NULL, tool TEXT NOT NULL, \
                        session TEXT NOT NULL, project TEXT NOT NULL, model TEXT NOT NULL, \
                        meter TEXT NOT NULL, in_tok REAL, cc_tok REAL, cr_tok REAL, \
                        out_tok REAL, reason_tok REAL, credits REAL, n INTEGER NOT NULL, \
                        nonzero INTEGER NOT NULL, min_ts INTEGER NOT NULL, max_ts INTEGER NOT NULL, \
                        PRIMARY KEY(day, tool, session, project, model, meter)); \
                     INSERT INTO event_rollup VALUES('1999-01-01','junk','s','','','tokens', \
                        9,0,0,0,0,0,1,1,0,0)",
                )
                .unwrap();
        }
        let mut idx = Index::open(&path).unwrap();
        let cols: i64 = idx
            .conn()
            .query_row(
                "SELECT count(*) FROM pragma_table_info('event_rollup') WHERE name = 'origin'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cols, 1, "reopening recreated the table with the origin column");
        let facts = idx.report_facts(&plan(), &MachineScope::All).unwrap();
        assert!(facts.rebuilt, "the emptied rollup rebuilds from the events");
        assert!(
            facts.rollup.iter().all(|r| r.day != "1999-01-01"),
            "the legacy junk row is gone, not carried into the new shape"
        );
        let p = pricing();
        let opts = golden_opts(&p);
        let oracle = summarize(&idx.all_events().unwrap(), &opts);
        assert_report_eq(&oracle, &summarize_facts(&facts, &opts));
    }

    /// The two mcps on E17 must not double-count: only the first mcp (and the
    /// first skill) name the event, and the "other" mcp appears nowhere.
    #[test]
    fn first_call_per_kind_names_the_whole_event() {
        let p = pricing();
        let mock = Mock::new(PathBuf::from("/fixture/calls.jsonl"), batch_one());
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        let opts = golden_opts(&p);
        let oracle = summarize(&idx.all_events().unwrap(), &opts);
        let facts = idx.report_facts(&plan(), &MachineScope::All).unwrap();
        let mine = summarize_facts(&facts, &opts);
        let oracle_bugx = oracle.day.breakdown.mcps.iter().find(|i| i.key == "bugx").expect("oracle has bugx");
        let mine_bugx = mine.day.breakdown.mcps.iter().find(|i| i.key == "bugx").expect("facts have bugx");
        assert_eq!(mine_bugx.requests, oracle_bugx.requests, "E1 + E17 name bugx, E17's second mcp does not");
        assert_report_eq(&oracle, &mine);
        assert!(mine.day.breakdown.mcps.iter().all(|i| i.key != "other"), "the second mcp never names an event");
        assert_eq!(mine.day.breakdown.skills.len(), 1, "review is the only skill");
    }

    #[test]
    fn cycle_usage_counts_local_tokens_and_only_projects_trusted_token_meters() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle.jsonl"),
            vec![
                ev("codex", now - 10 * 60_000, "cycle", Some("gpt-5"), None, counts(1_000.0)),
                ev("opencode", now - 10 * 60_000, "cycle", Some("model"), None, counts(2_000.0)),
            ],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = CASE id WHEN 1 THEN 'codex' ELSE 'opencode' END", []).unwrap();
        let mut quotas = vec![
            QuotaView {
                tool: "codex".into(), used_percent: 25.0, window_minutes: 300,
                resets_at_ms: now + 290 * 60_000, sampled_at_ms: now, label: Some("5 小时".into()),
                id: Some("acct:codex:300".into()), origin: usage_core::QuotaOrigin::Probe, cycle: None,
            },
            QuotaView {
                tool: "opencode".into(), used_percent: 25.0, window_minutes: 300,
                resets_at_ms: now + 290 * 60_000, sampled_at_ms: now, label: Some("5 小时".into()),
                id: None, origin: usage_core::QuotaOrigin::Probe, cycle: None,
            },
        ];
        idx.attach_cycle_usage(&mut quotas, now).unwrap();
        assert_eq!(quotas[0].cycle.as_ref().unwrap().observed_tokens, 1_000.0);
        assert_eq!(quotas[0].cycle.as_ref().unwrap().estimated_tokens, Some(4_000.0));
        assert_eq!(quotas[1].cycle.as_ref().unwrap().observed_tokens, 2_000.0);
        assert_eq!(quotas[1].cycle.as_ref().unwrap().estimated_tokens, None, "OpenCode's official percentage is a dollar meter, not a Token meter");
        assert_eq!(quotas[1].cycle.as_ref().unwrap().status, CycleStatus::Unavailable);
    }

    #[test]
    fn cycle_usage_keeps_tokens_when_reset_is_unknown() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-no-reset.jsonl"),
            vec![
                ev("codex", now - 10 * 60_000, "cycle", Some("gpt-5"), None, counts(1_000.0)),
                ev("codex", now - 60_000, "cycle", Some("gpt-5"), None, counts(2_000.0)),
            ],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'codex'", []).unwrap();
        let mut quotas = vec![QuotaView {
            tool: "codex".into(), used_percent: 25.0, window_minutes: 300,
            resets_at_ms: 0, sampled_at_ms: now - 5 * 60_000, label: Some("5 小时".into()),
            id: None, origin: usage_core::QuotaOrigin::Probe, cycle: None,
        }];
        idx.attach_cycle_usage(&mut quotas, now).unwrap();
        let cycle = quotas[0].cycle.as_ref().unwrap();
        assert_eq!(cycle.observed_tokens, 1_000.0);
        assert_eq!(cycle.estimated_tokens, None);
        assert_eq!(cycle.status, CycleStatus::Collecting);
        assert_eq!(cycle.period_end_ms, now - 5 * 60_000);
    }

    #[test]
    fn cycle_usage_starts_fresh_when_quota_account_changes() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-account.jsonl"),
            vec![ev("codex", now - 10 * 60_000, "cycle", Some("gpt-5"), None, counts(1_000.0))],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'codex'", []).unwrap();
        let quota = |id: &str| QuotaView {
            tool: "codex".into(), used_percent: 25.0, window_minutes: 300,
            resets_at_ms: now + 290 * 60_000, sampled_at_ms: now, label: Some("5 小时".into()),
            id: Some(id.into()), origin: usage_core::QuotaOrigin::Probe, cycle: None,
        };
        let mut first = vec![quota("acct:a:300")];
        idx.attach_cycle_usage(&mut first, now).unwrap();
        assert_eq!(first[0].cycle.as_ref().unwrap().observed_tokens, 1_000.0);
        let mut switched = vec![quota("acct:b:300")];
        idx.attach_cycle_usage(&mut switched, now).unwrap();
        let cycle = switched[0].cycle.as_ref().unwrap();
        assert_eq!(cycle.observed_tokens, 0.0);
        assert_eq!(cycle.previous_tokens, None);
        assert_eq!(cycle.period_start_ms, now);
    }

    #[test]
    fn cycle_usage_keeps_verified_identity_when_cached_quota_has_none() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-cached-identity.jsonl"),
            vec![ev("codex", now - 10 * 60_000, "cycle", Some("gpt-5"), None, counts(1_000.0))],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'codex'", []).unwrap();
        let quota = |id: Option<&str>, sampled_at_ms| QuotaView {
            tool: "codex".into(), used_percent: 25.0, window_minutes: 300,
            resets_at_ms: now + 290 * 60_000, sampled_at_ms, label: Some("5 小时".into()),
            id: id.map(str::to_string), origin: usage_core::QuotaOrigin::Probe, cycle: None,
        };
        let mut first = vec![quota(Some("acct:a:300"), now)];
        idx.attach_cycle_usage(&mut first, now).unwrap();
        let mut cached = vec![quota(None, now + 60_000)];
        idx.attach_cycle_usage(&mut cached, now + 60_000).unwrap();
        let mut switched = vec![quota(Some("acct:b:300"), now + 120_000)];
        idx.attach_cycle_usage(&mut switched, now + 120_000).unwrap();
        assert_eq!(switched[0].cycle.as_ref().unwrap().observed_tokens, 0.0);
        assert_eq!(switched[0].cycle.as_ref().unwrap().period_start_ms, now + 120_000);
    }

    #[test]
    fn cycle_usage_does_not_merge_same_window_buckets() {
        let now = now_fixture();
        let mut idx = Index::open_in_memory().unwrap();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-buckets.jsonl"),
            vec![
                ev("claude", now - 10 * 60_000, "cycle-sonnet", Some("claude-sonnet-4"), None, counts(1_000.0)),
                ev("claude", now - 9 * 60_000, "cycle-opus", Some("claude-opus-4"), None, counts(2_000.0)),
            ],
        );
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'claude'", []).unwrap();
        let quota = |label: &str| QuotaView {
            tool: "claude".into(), used_percent: 25.0, window_minutes: 10_080,
            resets_at_ms: now + 6 * 86_400_000, sampled_at_ms: now, label: Some(label.into()),
            id: Some("acct:a:10080".into()), origin: usage_core::QuotaOrigin::Probe, cycle: None,
        };
        let mut quotas = vec![quota("7 天 · Sonnet"), quota("7 天 · Opus")];
        idx.attach_cycle_usage(&mut quotas, now).unwrap();
        assert!(quotas.iter().all(|q| q.cycle.is_some()));
        assert_eq!(quotas[0].cycle.as_ref().unwrap().observed_tokens, 1_000.0);
        assert_eq!(quotas[1].cycle.as_ref().unwrap().observed_tokens, 2_000.0);
    }

    #[test]
    fn cycle_usage_does_not_extrapolate_without_account_identity() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-unknown-account.jsonl"),
            vec![ev("antigravity", now - 10 * 60_000, "cycle", Some("gemini"), None, counts(1_000.0))],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'antigravity'", []).unwrap();
        let mut quotas = vec![QuotaView {
            tool: "antigravity".into(), used_percent: 25.0, window_minutes: 300,
            resets_at_ms: now + 290 * 60_000, sampled_at_ms: now, label: Some("5 小时".into()),
            id: None, origin: usage_core::QuotaOrigin::Probe, cycle: None,
        }];
        idx.attach_cycle_usage(&mut quotas, now).unwrap();
        assert_eq!(quotas[0].cycle.as_ref().unwrap().observed_tokens, 1_000.0);
        assert_eq!(quotas[0].cycle.as_ref().unwrap().estimated_tokens, None);
    }

    /// Cline rides the unverified tier: the estimate fires at 1% with an
    /// unknown account identity (the store is single-account), the status says
    /// so, and the per-model breakdown rides along — the model list's data.
    #[test]
    fn cycle_usage_estimates_cline_as_unverified_with_models() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-cline-unverified.jsonl"),
            vec![
                ev("cline", now - 10 * 60_000, "cycle-a", Some("claude-sonnet-4-5"), None, counts(8_700_000.0)),
                ev("cline", now - 9 * 60_000, "cycle-b", Some("qwen3-coder-480b"), None, counts(3_900_000.0)),
                ev("cline", now - 8 * 60_000, "cycle-c", Some("deepseek-v3.2"), None, counts(1_400_000.0)),
            ],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'cline'", []).unwrap();
        let mut quotas = vec![QuotaView {
            tool: "cline".into(), used_percent: 1.0, window_minutes: 300,
            resets_at_ms: now + 290 * 60_000, sampled_at_ms: now, label: Some("5 小时".into()),
            id: None, origin: usage_core::QuotaOrigin::Probe, cycle: None,
        }];
        idx.attach_cycle_usage(&mut quotas, now).unwrap();
        let cycle = quotas[0].cycle.as_ref().unwrap();
        assert_eq!(cycle.observed_tokens, 14_000_000.0);
        assert_eq!(cycle.status, CycleStatus::Unverified);
        // 1% refuses the estimate for a trusted tool; the unverified tier
        // exists precisely to show this magnitude, caveated.
        assert_eq!(cycle.estimated_tokens, Some(1_400_000_000.0));
        assert_eq!(cycle.models.len(), 3);
        assert_eq!(cycle.models[0].model, "claude-sonnet-4-5");
        assert_eq!(cycle.models[0].tokens, 8_700_000.0);
        assert!(cycle.models[1].tokens > cycle.models[2].tokens, "heaviest first");
    }

    /// A tool in neither tier keeps the old behavior: the row stays with its
    /// observed tokens, but no estimate and no model list ship at all.
    #[test]
    fn cycle_usage_still_refuses_unknown_meters() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-opencode-unavailable.jsonl"),
            vec![ev("opencode", now - 10 * 60_000, "cycle", Some("gpt-5"), None, counts(1_000.0))],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'opencode'", []).unwrap();
        let mut quotas = vec![QuotaView {
            tool: "opencode".into(), used_percent: 25.0, window_minutes: 300,
            resets_at_ms: now + 290 * 60_000, sampled_at_ms: now, label: Some("5 小时".into()),
            id: None, origin: usage_core::QuotaOrigin::Probe, cycle: None,
        }];
        idx.attach_cycle_usage(&mut quotas, now).unwrap();
        let cycle = quotas[0].cycle.as_ref().unwrap();
        assert_eq!(cycle.status, CycleStatus::Unavailable);
        assert_eq!(cycle.estimated_tokens, None);
        assert!(cycle.models.is_empty());
    }

    #[test]
    fn cycle_usage_ignores_stale_lower_percentage() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-stale-quota.jsonl"),
            vec![ev("codex", now - 10 * 60_000, "cycle", Some("gpt-5"), None, counts(1_000.0))],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'codex'", []).unwrap();
        let quota = |used_percent, sampled_at_ms| QuotaView {
            tool: "codex".into(), used_percent, window_minutes: 300,
            resets_at_ms: now + 290 * 60_000, sampled_at_ms, label: Some("5 小时".into()),
            id: Some("acct:a:300".into()), origin: usage_core::QuotaOrigin::Probe, cycle: None,
        };
        let mut fresh = vec![quota(25.0, now)];
        idx.attach_cycle_usage(&mut fresh, now).unwrap();
        let mut stale = vec![quota(0.0, now - 60_000)];
        idx.attach_cycle_usage(&mut stale, now).unwrap();
        assert_eq!(stale[0].cycle.as_ref().unwrap().observed_tokens, 1_000.0);
        assert_eq!(stale[0].cycle.as_ref().unwrap().status, CycleStatus::Partial);
    }

    #[test]
    fn cycle_usage_tolerates_small_reset_timestamp_jitter() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-reset-jitter.jsonl"),
            vec![ev("codex", now - 10 * 60_000, "cycle", Some("gpt-5"), None, counts(1_000.0))],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'codex'", []).unwrap();
        let quota = |reset, sampled_at_ms| QuotaView {
            tool: "codex".into(), used_percent: 25.0, window_minutes: 300,
            resets_at_ms: reset, sampled_at_ms, label: Some("5 小时".into()),
            id: Some("acct:a:300".into()), origin: usage_core::QuotaOrigin::Probe, cycle: None,
        };
        let first_reset = now + 290 * 60_000;
        let mut first = vec![quota(first_reset, now)];
        idx.attach_cycle_usage(&mut first, now).unwrap();
        let mut jittered = vec![quota(first_reset + 30_000, now + 30_000)];
        idx.attach_cycle_usage(&mut jittered, now + 30_000).unwrap();
        assert_eq!(jittered[0].cycle.as_ref().unwrap().period_start_ms, now - 10 * 60_000);
        assert_eq!(jittered[0].cycle.as_ref().unwrap().observed_tokens, 1_000.0);
    }

    #[test]
    fn cycle_usage_ignores_label_whitespace_changes() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-label-spelling.jsonl"),
            vec![ev("codex", now - 10 * 60_000, "cycle", Some("gpt-5"), None, counts(1_000.0))],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'codex'", []).unwrap();
        let quota = |label: &str, sampled_at_ms| QuotaView {
            tool: "codex".into(), used_percent: 25.0, window_minutes: 300,
            resets_at_ms: now + 290 * 60_000, sampled_at_ms, label: Some(label.into()),
            id: Some("acct:a:300".into()), origin: usage_core::QuotaOrigin::Probe, cycle: None,
        };
        let mut first = vec![quota("5 小时", now)];
        idx.attach_cycle_usage(&mut first, now).unwrap();
        let mut spelling = vec![quota("5小时", now + 60_000)];
        idx.attach_cycle_usage(&mut spelling, now + 60_000).unwrap();
        assert_eq!(spelling[0].cycle.as_ref().unwrap().period_start_ms, now - 10 * 60_000);
    }

    #[test]
    fn cycle_usage_account_change_wins_over_a_delayed_cached_sample() {
        let now = now_fixture();
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-delayed-account.jsonl"),
            vec![ev("codex", now - 10 * 60_000, "cycle", Some("gpt-5"), None, counts(1_000.0))],
        );
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'codex'", []).unwrap();
        let quota = |id: &str, sampled_at_ms| QuotaView {
            tool: "codex".into(), used_percent: 25.0, window_minutes: 300,
            resets_at_ms: now + 290 * 60_000, sampled_at_ms, label: Some("5 小时".into()),
            id: Some(id.into()), origin: usage_core::QuotaOrigin::Probe, cycle: None,
        };
        let mut first = vec![quota("acct:a:300", now)];
        idx.attach_cycle_usage(&mut first, now).unwrap();
        let mut switched = vec![quota("acct:b:300", now - 60_000)];
        idx.attach_cycle_usage(&mut switched, now).unwrap();
        assert_eq!(switched[0].cycle.as_ref().unwrap().observed_tokens, 0.0);
        assert_eq!(switched[0].cycle.as_ref().unwrap().previous_tokens, None);
    }

    #[test]
    fn cycle_usage_restores_last_complete_cycle_after_reopen() {
        let now = now_fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mock = Mock::new(
            PathBuf::from("/fixture/cycle-reopen.jsonl"),
            vec![ev("codex", now - 10 * 60_000, "cycle", Some("gpt-5"), None, counts(1_000.0))],
        );
        let mut idx = Index::open(&path).unwrap();
        idx.ingest_adapter(&mock, &DateFilter::default()).unwrap();
        idx.conn().execute("UPDATE event SET tool = 'codex'", []).unwrap();
        let mut first = vec![QuotaView {
            tool: "codex".into(), used_percent: 100.0, window_minutes: 300,
            resets_at_ms: now + 290 * 60_000, sampled_at_ms: now, label: Some("5 小时".into()),
            id: Some("acct:a:300".into()), origin: usage_core::QuotaOrigin::Probe, cycle: None,
        }];
        idx.attach_cycle_usage(&mut first, now).unwrap();
        drop(idx);

        let next = now + 301 * 60_000;
        let mut second = vec![QuotaView {
            tool: "codex".into(), used_percent: 10.0, window_minutes: 300,
            resets_at_ms: next + 290 * 60_000, sampled_at_ms: next, label: Some("5 小时".into()),
            id: Some("acct:a:300".into()), origin: usage_core::QuotaOrigin::Probe, cycle: None,
        }];
        let idx = Index::open(&path).unwrap();
        idx.attach_cycle_usage(&mut second, next).unwrap();
        assert_eq!(second[0].cycle.as_ref().unwrap().previous_tokens, Some(1_000.0));
    }
}


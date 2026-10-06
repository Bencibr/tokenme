//! The single aggregation both frontends read. Keeping the period math, money
//! math and breakdown grouping here is what stops the menu-bar panel and the CLI
//! from ever disagreeing about a number.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::AddAssign;
use std::path::PathBuf;

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

use crate::budget::Budget;
use crate::pricing::{Price, PricingMap, PricingMeta};
use crate::types::{CallKind, Meter, QuotaSample, TokenCounts, UsageEvent};

/// Cells ending today; a whole number of weeks so the grid needs no padding.
pub const HEATMAP_DAYS: i64 = 371;

/// How long a log-derived quota row that advertises no reset date stays on the
/// panel after its last mention.
pub const LOG_ROW_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    pub counts: TokenCounts,
    pub total_tokens: f64,
    pub cost: f64,
    /// Sum of credit-metered sources. Their share of `cost` is a plan-price
    /// estimate, so the count stays visible next to the money.
    pub credits: f64,
    /// Part of `cost` produced by converting credits at a published plan price
    /// instead of a per-token table: non-zero means `cost` is partly "≈".
    pub credit_cost: f64,
    pub requests: u64,
    pub sessions: u64,
    pub cached_pct: f64,
    pub unpriced: Vec<UnpricedModel>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnpricedModel {
    pub tool: String,
    pub model: String,
    pub total_tokens: f64,
    pub requests: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub key: String,
    pub label: String,
    pub counts: TokenCounts,
    pub total_tokens: f64,
    pub cost: f64,
    pub requests: u64,
    pub sessions: u64,
    /// False when no price was found, so the UI can say "no price" instead of "$0".
    pub priced: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Breakdown {
    pub tools: Vec<Item>,
    pub models: Vec<Item>,
    pub projects: Vec<Item>,
    pub mcps: Vec<Item>,
    pub skills: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Window {
    /// `2026-09-23`, `2026-W39`, `2026-09`.
    pub key: String,
    pub label: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub summary: Summary,
    /// Previous period of equal length (today up to now vs yesterday to the
    /// same clock time), so a partial period is never compared to a full one.
    pub prev: Summary,
    pub delta_cost_pct: f64,
    pub delta_tokens_pct: f64,
    pub breakdown: Breakdown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HeatCell {
    pub date: String,
    pub total_tokens: f64,
    pub cost: f64,
    pub requests: u64,
}

/// One local-hour bucket of today, for the panel's 今日 chart. The hour is the
/// vec index (0..24 always present), so the frontend never guesses which slots
/// exist and an hour that has not happened yet is a zero, not a gap.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HourCell {
    pub total_tokens: f64,
    pub cost: f64,
    pub requests: u64,
}

/// Where a quota number came from: read out of the tool's own log records, or
/// polled live from the tool. A probe result is fresher by construction, so it
/// replaces the log-derived one for the same window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaOrigin {
    /// Read out of the tool's own log records.
    Log,
    /// Polled live from the tool (vendor API, its CLI, or its local credential).
    Probe,
    /// A cap the user set in tokenme, measured against our own cost — the only
    /// honest answer for tools whose vendor publishes no limit (Cline, ZCode).
    Budget,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaView {
    pub tool: String,
    pub used_percent: f64,
    pub window_minutes: i64,
    pub resets_at_ms: i64,
    pub sampled_at_ms: i64,
    pub label: Option<String>,
    /// Stable identity across polls, for a panel that lets its rows be
    /// reordered: labels and implied lengths both drift, ids do not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub origin: QuotaOrigin,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceStatus {
    pub id: String,
    pub display: String,
    pub detected: bool,
    pub roots: Vec<PathBuf>,
    pub hint: Option<String>,
    pub events_ingested: u64,
}

/// Which machines' rows a report folds over. `All` is the pre-feature merge
/// (this machine plus every imported origin). `Local` is this machine alone —
/// origin `''` in the index. `Origin` is one remote machine. The panel sends
/// it as `{"kind":"all"}` / `{"kind":"local"}` / `{"kind":"origin",
/// "name":"ops-box"}`; the report echoes it back so a stale in-flight report
/// can never paint a freshly switched panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MachineScope {
    #[default]
    All,
    Local,
    Origin {
        name: String,
    },
}

impl MachineScope {
    /// Whether one index row's `origin` belongs in this scope.
    pub fn keeps(&self, origin: &str) -> bool {
        match self {
            MachineScope::All => true,
            MachineScope::Local => origin.is_empty(),
            MachineScope::Origin { name } => origin == name,
        }
    }
}

/// One machine's today volume for the panel's scope menu — `origin: ""` is
/// this machine. Every origin with any indexed row appears (a remote that has
/// not synced today still gets a row, with 0); the panel joins ages from
/// [`Report::syncs`] and hides the whole control when no remote exists.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MachineView {
    pub origin: String,
    pub today_tokens: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionRow {
    pub tool: String,
    pub session: String,
    pub project: Option<String>,
    pub model: String,
    pub first_ms: i64,
    pub last_ms: i64,
    pub total_tokens: f64,
    pub cost: f64,
    pub requests: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub generated_at_ms: i64,
    /// Local UTC offset, e.g. `+08:00`. All period boundaries are local time.
    pub utc_offset: String,
    pub day: Window,
    pub week: Window,
    pub month: Window,
    pub year: Window,
    pub heatmap: Vec<HeatCell>,
    /// Today bucketed by local hour, 24 slots in index order. Older snapshots
    /// without the field read as empty; the panel then keeps the heatmap view.
    #[serde(default)]
    pub hourly: Vec<HourCell>,
    pub quotas: Vec<QuotaView>,
    /// True while the quota probes have not run yet: the boot publish skips
    /// them on purpose and the post-scan publish carries the numbers. The
    /// frontend shows a loading state instead of a strip that vanishes for no
    /// visible reason. Defaults off — reports without the field (older
    /// snapshots, the browser preview) read as "already settled".
    #[serde(default)]
    pub quotas_pending: bool,
    pub sources: Vec<SourceStatus>,
    /// One record per origin whose Linux bundle was successfully merged into
    /// this index, newest first — the panel's sync-health badge reads this.
    /// Defaults empty for reports that predate the field.
    #[serde(default)]
    pub syncs: Vec<SyncRecord>,
    pub pricing: PricingMeta,
    /// The machine scope this report was folded under, echoed from the
    /// request; the panel compares it against its own selection and ignores
    /// mismatched pushes.
    pub scope: MachineScope,
    /// Per-machine today volumes for the scope menu, this machine first
    /// (`""`), then remotes by name. Independent of `scope` on purpose: the
    /// menu must list every machine even while one of them is selected.
    #[serde(default)]
    pub machines: Vec<MachineView>,
    pub recent_sessions: Vec<SessionRow>,
    /// Whole indexed history, for the "总计" line.
    pub all_time: Summary,
}

/// What one `tokenme import` merged from one origin, as written to the index's
/// `meta` table (`sync:linux:<origin>`) in the same transaction as the merge —
/// so a record can never describe a batch that was rolled back. The panel
/// renders it as the "Linux 同步" badge; `imported_at_ms` is what tells a
/// stalled sync apart from a quiet afternoon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRecord {
    pub origin: String,
    pub imported_at_ms: i64,
    pub window_lo_ms: i64,
    pub window_hi_ms: i64,
    pub rows: u64,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReportOptions<'a> {
    pub pricing: &'a PricingMap,
    pub now_ms: i64,
    pub sources: Vec<SourceStatus>,
    pub recent_session_limit: usize,
    /// Quota read from the tools just now, via `SourceAdapter::quota`.
    pub polled_quota: Vec<QuotaView>,
    /// Spend caps set in tokenme, keyed by tool id. They become quota bars too,
    /// measured against the cost this same report computes.
    pub budgets: BTreeMap<String, Budget>,
    /// Which machines to fold. [`summarize_facts`] honors it, including for
    /// the budget bars' cost inputs (budgets and vendor quotas stay the local
    /// machine's, whatever the scope). The event path ([`summarize`]) only
    /// ever sees this machine's events, so `All` and `Local` fold them and a
    /// remote scope folds none.
    pub scope: MachineScope,
}

impl<'a> ReportOptions<'a> {
    pub fn new(pricing: &'a PricingMap) -> Self {
        Self {
            pricing,
            now_ms: now_ms(),
            sources: Vec::new(),
            recent_session_limit: 20,
            polled_quota: Vec::new(),
            budgets: BTreeMap::new(),
            scope: MachineScope::All,
        }
    }

    pub fn with_now(mut self, now_ms: i64) -> Self {
        self.now_ms = now_ms;
        self
    }

    pub fn with_quota(mut self, polled: Vec<QuotaView>) -> Self {
        self.polled_quota = polled;
        self
    }

    pub fn with_budgets(mut self, budgets: BTreeMap<String, Budget>) -> Self {
        self.budgets = budgets;
        self
    }
}

/// Ask every adapter for its live quota. Cheap enough to call once per refresh,
/// but it may touch the network or a local server, so callers own the cadence.
pub fn poll_quota(adapters: &[Box<dyn crate::SourceAdapter>]) -> Vec<QuotaView> {
    let at = now_ms();
    let mut out = Vec::new();
    for adapter in adapters {
        for q in adapter.quota() {
            out.push(QuotaView {
                tool: adapter.id().to_string(),
                used_percent: q.used_percent,
                window_minutes: q.window_minutes,
                resets_at_ms: q.resets_at_ms,
                sampled_at_ms: at,
                label: q.label,
                id: q.id,
                origin: QuotaOrigin::Probe,
            });
        }
    }
    out
}

pub fn now_ms() -> i64 {
    Local::now().timestamp_millis()
}

/// The instant a report should be stamped with, given what it waited for.
///
/// A live probe answers *after* the caller captured its `now` — up to thirty
/// seconds later for `agy /usage` — and a report stamped with the earlier instant
/// claims to predate samples it carries. A caller that polled passes the samples
/// in and gets an instant no older than any of them.
///
/// `as_of_cutoff` is the `--until` case: the report describes a past instant, so
/// its stamp is that instant, and nothing live belongs inside it at all.
pub fn instant_after_polling(as_of_cutoff: bool, captured_ms: i64, polled: &[QuotaView]) -> i64 {
    if as_of_cutoff {
        return captured_ms;
    }
    polled.iter().map(|q| q.sampled_at_ms).max().unwrap_or(0).max(captured_ms)
}

fn to_local(ms: i64) -> Option<DateTime<Local>> {
    Local.timestamp_millis_opt(ms).single()
}

fn date_of(ms: i64) -> Option<NaiveDate> {
    to_local(ms).map(|d| d.date_naive())
}

/// The local calendar day an instant falls on, shared by the rollup writer and
/// the report fold. Bucketing is identical by construction because both sides
/// call this one function — the index never re-derives a day in SQL.
pub fn local_day_of(ms: i64) -> Option<NaiveDate> {
    date_of(ms)
}

fn start_of_day_ms(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0)
        .and_then(|naive| Local.from_local_datetime(&naive).single())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn week_start(d: NaiveDate) -> NaiveDate {
    d - chrono::Duration::days(i64::from(d.weekday().num_days_from_monday()))
}

fn iso_week_key(d: NaiveDate) -> String {
    let monday = week_start(d);
    format!("{}-W{:02}", monday.iso_week().year(), monday.iso_week().week())
}

struct Span {
    start_ms: i64,
    end_ms: i64,
    prev_start_ms: i64,
    key: String,
    label: String,
}

/// Calendar-aligned span plus an equally long slice immediately before it.
fn spans(now_ms: i64) -> (Span, Span, Span, Span) {
    let now = to_local(now_ms).unwrap_or_else(Local::now);
    let today = now.date_naive();

    let day_start = start_of_day_ms(today);
    let elapsed = (now_ms - day_start).max(1);
    let day = Span {
        start_ms: day_start,
        end_ms: now_ms,
        prev_start_ms: day_start - elapsed,
        key: today.format("%Y-%m-%d").to_string(),
        label: today.format("%m-%d").to_string(),
    };

    let this_monday = week_start(today);
    let week_start_ms = start_of_day_ms(this_monday);
    let week_elapsed = (now_ms - week_start_ms).max(1);
    let week = Span {
        start_ms: week_start_ms,
        end_ms: now_ms,
        prev_start_ms: week_start_ms - week_elapsed,
        key: iso_week_key(today),
        label: format!("{} ~ {}", this_monday.format("%m-%d"), today.format("%m-%d")),
    };

    let month_first = today.with_day(1).unwrap_or(today);
    let month_start_ms = start_of_day_ms(month_first);
    let month_elapsed = (now_ms - month_start_ms).max(1);
    let month = Span {
        start_ms: month_start_ms,
        end_ms: now_ms,
        prev_start_ms: month_start_ms - month_elapsed,
        key: month_first.format("%Y-%m").to_string(),
        label: month_first.format("%Y-%m").to_string(),
    };
    let year_first = today.with_month(1).and_then(|d| d.with_day(1)).unwrap_or(today);
    let year_start_ms = start_of_day_ms(year_first);
    let year_elapsed = (now_ms - year_start_ms).max(1);
    let year = Span {
        start_ms: year_start_ms,
        end_ms: now_ms,
        prev_start_ms: year_start_ms - year_elapsed,
        key: year_first.format("%Y").to_string(),
        label: year_first.format("%Y").to_string(),
    };
    (day, week, month, year)
}

fn pct_delta(cur: f64, prev: f64) -> f64 {
    if prev.abs() < 1e-9 {
        if cur.abs() < 1e-9 {
            0.0
        } else {
            100.0
        }
    } else {
        (cur - prev) / prev * 100.0
    }
}

/// An event with its money value resolved once, so every grouping is cheap.
#[derive(Debug, Clone, Copy)]
struct Row<'a> {
    ev: &'a UsageEvent,
    cost: f64,
    priced: bool,
    /// `cost` came from a credits conversion rather than a token price.
    credit_money: bool,
}

impl Row<'_> {
    fn tokens(&self) -> f64 {
        self.ev.counts.total()
    }
}

fn summarize_events(rows: &[Row<'_>]) -> Summary {
    let mut s = Summary::default();
    let mut sessions: HashSet<(&str, &str)> = HashSet::new();
    let mut unpriced: HashMap<(&str, &str), UnpricedModel> = HashMap::new();
    for r in rows {
        s.counts.add_assign(&r.ev.counts);
        s.cost += r.cost;
        if r.ev.meter == Meter::Credits {
            s.credits += r.ev.counts.credits;
            if r.credit_money {
                s.credit_cost += r.cost;
            }
        }
        s.requests += 1;
        sessions.insert((r.ev.tool.as_str(), r.ev.session.as_str()));
        if !r.priced && r.tokens() > 0.0 {
            let key = (r.ev.tool.as_str(), r.ev.model.as_deref().unwrap_or("unknown"));
            let e = unpriced.entry(key).or_insert_with(|| UnpricedModel {
                tool: key.0.to_string(),
                model: key.1.to_string(),
                total_tokens: 0.0,
                requests: 0,
            });
            e.total_tokens += r.tokens();
            e.requests += 1;
        }
    }
    s.total_tokens = s.counts.total();
    s.sessions = sessions.len() as u64;
    s.cached_pct = s.counts.cached_pct();
    let mut unpriced: Vec<_> = unpriced.into_values().collect();
    unpriced.sort_by(|a, b| b.total_tokens.total_cmp(&a.total_tokens).then_with(|| a.model.cmp(&b.model)));
    s.unpriced = unpriced;
    s
}

/// Qoder's gateway names models with opaque keys (`qfmodel`), but the key
/// itself decodes — `<vendor><version/tier>model`, one first letter per
/// vendor: q = Qwen, g = GLM, k = Kimi, d = DeepSeek, m = MiniMax. The
/// vendor-letter readings below are corroborated by the Qoder CN proxies'
/// official model lists (qwen3.8-max/flash, glm-5.3/flash, kimi-k3,
/// deepseek-v4-pro/flash, minimax-m3) and by the IDE's own
/// `chat_model_preferences` table, where `qmodel_38max` carries the 1M
/// context window the proxies advertise for qwen3.8-max.
///
/// Display only. The grouping key stays the raw key everywhere, so pricing,
/// dedupe and identity never see the rename — a credits-metered call must
/// not grow a dollar price just because its label got readable. Keys that
/// don't decode with confidence (`qmodel_latest`, `smodel`, `cmodel`) stay
/// verbatim rather than risk naming the wrong model.
fn model_display(id: &str) -> &str {
    match id {
        "qmodel_38max" => "qwen3.8-max",
        "qfmodel" => "qwen3.8-flash",
        "gmodel" => "glm-5.3",
        "gfmodel" => "glm-5.3-flash",
        "kmodel_latest" => "kimi-k3",
        "dmodel" => "deepseek-v4-pro",
        "dfmodel" => "deepseek-v4-flash",
        "mmodel" => "minimax-m3",
        other => other,
    }
}

fn group<F>(rows: &[Row<'_>], mut key_of: F) -> Vec<Item>
where
    F: FnMut(&UsageEvent) -> Option<(&str, &str)>,
{
    let mut acc: BTreeMap<String, (String, Vec<Row>)> = BTreeMap::new();
    for r in rows {
        let Some((key, label)) = key_of(r.ev) else { continue };
        if key.is_empty() {
            continue;
        }
        acc.entry(key.to_string()).or_insert_with(|| (label.to_string(), Vec::new())).1.push(*r);
    }
    let mut items: Vec<Item> = acc
        .into_iter()
        .map(|(key, (label, group))| {
            let s = summarize_events(&group);
            Item {
                key,
                label,
                counts: s.counts,
                total_tokens: s.total_tokens,
                cost: s.cost,
                requests: s.requests,
                sessions: s.sessions,
                priced: group.iter().any(|r| r.priced),
            }
        })
        .collect();
    items.sort_by(|a, b| {
        b.cost.total_cmp(&a.cost)
            .then_with(|| b.total_tokens.total_cmp(&a.total_tokens))
            // P1: pin full ties to ascending key. The fold below iterates a
            // BTreeMap (ascending key) with this stable sort, so equal-cost items
            // already emerged in key order — the comparison only makes that
            // explicit, and it keeps the SQL-rollup path (`summarize_facts`),
            // whose per-group costs can differ in the last ULP from the
            // per-event sums, in exactly the same order instead of at the
            // mercy of a floating-point hair.
            .then_with(|| a.key.cmp(&b.key))
    });
    items
}

fn build_window(span: Span, rows: &[Row<'_>]) -> Window {
    let cur: Vec<Row> = rows.iter().copied().filter(|r| r.ev.ts_ms >= span.start_ms && r.ev.ts_ms <= span.end_ms).collect();
    let prev: Vec<Row> = rows
        .iter()
        .copied()
        .filter(|r| r.ev.ts_ms >= span.prev_start_ms && r.ev.ts_ms < span.start_ms)
        .collect();
    let summary = summarize_events(&cur);
    let prev_summary = summarize_events(&prev);
    let breakdown = Breakdown {
        tools: group(&cur, |e| Some((e.tool.as_str(), e.tool.as_str()))),
        models: group(&cur, |e| {
            let id = e.model.as_deref().unwrap_or("unknown");
            Some((id, model_display(id)))
        }),
        projects: group(&cur, |e| e.project.as_deref().map(|p| (p, p))),
        mcps: group(&cur, |e| e.calls.iter().find(|c| c.kind == CallKind::Mcp).map(|c| (c.name.as_str(), c.name.as_str()))),
        skills: group(&cur, |e| e.calls.iter().find(|c| c.kind == CallKind::Skill).map(|c| (c.name.as_str(), c.name.as_str()))),
    };
    Window {
        key: span.key,
        label: span.label,
        start_ms: span.start_ms,
        end_ms: span.end_ms,
        delta_cost_pct: pct_delta(summary.cost, prev_summary.cost),
        delta_tokens_pct: pct_delta(summary.total_tokens, prev_summary.total_tokens),
        summary,
        prev: prev_summary,
        breakdown,
    }
}

/// Per-tool cost inside one window, for the budget bars.
/// The name that tells two same-length windows apart, normalised so a probe
/// writing "5 小时" replaces a log row saying "5小时".
fn label_key(label: Option<&str>) -> String {
    label
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Codex currently publishes a five-hour and a weekly window. Older log
/// records (and some backend responses) can contain a 30-day bucket, but that
/// is not a Codex quota and must not survive into the panel report. This check
/// also removes stale bad rows already present in the local index.
fn keep_vendor_quota(tool: &str, window_minutes: i64) -> bool {
    !(tool == "codex" && window_minutes >= 43_200)
}

/// Nominal window lengths order short-to-long; a length the source only implies —
/// a reset gap that shrinks on every poll — would make its row climb over its
/// neighbours as it decays, so implied lengths sort after every nominal one and
/// no-window rows (session budgets, credit meters) sort last.
fn window_rank(minutes: i64) -> i64 {
    match minutes {
        m if m <= 0 => 5,
        300 => 0,
        1_440 => 1,
        10_080 => 2,
        43_200 => 3,
        _ => 4,
    }
}

fn cost_by_tool(items: &[Item]) -> BTreeMap<String, f64> {
    items.iter().map(|i| (i.key.clone(), i.cost)).collect()
}

/// Local midnight `days` ahead, in ms. The day/month a budget is measured over is
/// the user's calendar, not UTC, so the reset line agrees with the period tabs.
fn next_local_midnight(now_ms: i64, days: i64) -> i64 {
    let Some(now) = to_local(now_ms) else { return 0 };
    let Some(target) = now.date_naive().checked_add_signed(chrono::Duration::days(days)) else {
        return 0;
    };
    target
        .and_hms_opt(0, 0, 0)
        .and_then(|d| Local.from_local_datetime(&d).single())
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

/// How many days ahead the first of next month is, from the report's own "now".
fn first_of_next_month_days(now_ms: i64) -> i64 {
    let Some(now) = to_local(now_ms) else { return 0 };
    let (y, m) = (now.year(), now.month());
    let (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let first = NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(now.date_naive());
    (first - now.date_naive()).num_days()
}

fn utc_offset(now_ms: i64) -> String {
    to_local(now_ms).map(|d| d.offset().to_string()).unwrap_or_else(|| "+00:00".into())
}

/// The whole quota-strip pipeline, shared verbatim by the event path
/// ([`summarize`]) and the rollup path ([`summarize_facts`]) — one function so
/// the two report paths cannot drift on which meters survive and how they sort.
///
/// `log_rows` must arrive in event-id order: two samples with the same
/// `(tool, window, label)` key resolve ties by "later row wins".
fn merge_quotas<'a>(
    log_rows: impl Iterator<Item = (&'a str, i64, &'a QuotaSample)>,
    polled: &[QuotaView],
    spent_today: &BTreeMap<String, f64>,
    spent_month: &BTreeMap<String, f64>,
    now_ms: i64,
    budgets: &BTreeMap<String, Budget>,
) -> Vec<QuotaView> {
    // Only the newest sample per window is meaningful, and a tool can report
    // several windows at once. The window's *name* is part of its identity: ZCode
    // has a monthly tool-call meter and a monthly MCP meter, and Antigravity has a
    // 5-hour window per model group — keyed on length alone, the second silently
    // replaced the first. A row with no name (Codex writes its window into the log
    // without one) is the same window as the named probe of it, so an empty name
    // matches anything.
    let mut quotas: BTreeMap<(String, i64, String), QuotaView> = BTreeMap::new();
    for (tool, ts_ms, q) in log_rows {
        if !keep_vendor_quota(tool, q.window_minutes) {
            continue;
        }
        let key = (tool.to_string(), q.window_minutes, label_key(q.label.as_deref()));
        let better = quotas.get(&key).is_none_or(|cur| ts_ms >= cur.sampled_at_ms);
        if better {
            quotas.insert(
                key,
                QuotaView {
                    tool: tool.to_string(),
                    used_percent: q.used_percent,
                    window_minutes: q.window_minutes,
                    resets_at_ms: q.resets_at_ms,
                    sampled_at_ms: ts_ms,
                    label: q.label.clone(),
                    id: q.id.clone(),
                    origin: QuotaOrigin::Log,
                },
            );
        }
    }
    // A live probe is fresher than whatever the last log record happened to carry.
    for polled in polled {
        if !keep_vendor_quota(&polled.tool, polled.window_minutes) {
            continue;
        }
        let named = label_key(polled.label.as_deref());
        let same = quotas
            .iter()
            .find(|(k, _)| {
                k.0 == polled.tool
                    && k.1 == polled.window_minutes
                    && (named.is_empty() || k.2.is_empty() || k.2 == named)
            })
            .map(|(k, _)| k.clone());
        if let Some(previous) = same {
            quotas.remove(&previous);
        }
        quotas.insert((polled.tool.clone(), polled.window_minutes, named), polled.clone());
    }
    // A window whose reset has already passed is stale by construction: the tool
    // has moved on to a new one and either reported it or stopped reporting.
    // A log row that advertises no reset at all cannot be judged that way, so it
    // is judged by age instead: a meter nobody has mentioned for a day — a probe
    // that changed its labels, a session long closed — is not on the panel.
    let mut quotas: Vec<QuotaView> = quotas
        .into_values()
        .filter(|q| match (q.resets_at_ms, q.origin) {
            (0, QuotaOrigin::Log) => q.sampled_at_ms + LOG_ROW_LIFETIME_MS > now_ms,
            _ => q.resets_at_ms == 0 || q.resets_at_ms >= now_ms,
        })
        .collect();
    // Budgets are measured on the same local boundaries the windows above use, so
    // "80 % of today's cap" and today's cost line can never disagree.
    quotas.extend(crate::budget::views(
        budgets,
        spent_today,
        spent_month,
        now_ms,
        next_local_midnight(now_ms, 1),
        next_local_midnight(now_ms, first_of_next_month_days(now_ms)),
    ));
    // Rows whose label names a subject before the window — Antigravity's
    // "Gemini · 5 小时" — are grouped by that subject, and the subjects keep the
    // order the probes emitted them in, which is the vendor's own grouping. A
    // subject with a single window has nothing to group: those rows keep the
    // window-length order (ZCode's meters must not start following the vendor's
    // limits array). Bare window labels ("5 小时") name no subject either.
    // Nothing here moves with the numbers: a percentage or a shrinking reset
    // gap would make the bars jump around while being read.
    fn family_label(q: &QuotaView) -> Option<&str> {
        q.label.as_deref().and_then(|l| l.split_once(" · ")).map(|(fam, _)| fam).filter(|fam| !fam.is_empty())
    }
    let mut family_rank: Vec<(String, String)> = Vec::new();
    for q in polled {
        let Some(fam) = family_label(q) else { continue };
        if family_rank.iter().any(|(t, f)| t == &q.tool && f == fam) {
            continue;
        }
        family_rank.push((q.tool.clone(), fam.to_string()));
    }
    // Owned, not borrowed: the vec feeds a closure that runs inside
    // `quotas.sort_by`, and a borrow of `quotas` rows would fight the sort's
    // mutable borrow.
    let mut family_windows: Vec<(String, String, usize)> = Vec::new();
    for q in &quotas {
        let Some(fam) = family_label(q) else { continue };
        match family_windows.iter_mut().find(|(t, f, _)| t == &q.tool && f == fam) {
            Some((_, _, n)) => *n += 1,
            None => family_windows.push((q.tool.clone(), fam.to_string(), 1)),
        }
    }
    let family_rank = |q: &QuotaView| -> usize {
        let Some(fam) = family_label(q) else { return 0 };
        let grouped = family_windows.iter().any(|(t, f, n)| t == &q.tool && f == fam && *n > 1);
        if !grouped {
            return 0;
        }
        family_rank.iter().position(|(t, f)| t == &q.tool && f == fam).map_or(0, |i| i + 1)
    };
    quotas.sort_by(|a, b| {
        a.tool.cmp(&b.tool)
            .then_with(|| family_rank(a).cmp(&family_rank(b)))
            .then_with(|| window_rank(a.window_minutes).cmp(&window_rank(b.window_minutes)))
            .then_with(|| a.label.cmp(&b.label))
    });
    quotas
}

/// Build the whole panel/CLI payload from already-deduplicated events.
pub fn summarize(events: &[UsageEvent], opts: &ReportOptions) -> Report {
    let all_rows: Vec<Row> = events
        .iter()
        .map(|ev| {
            let credits = ev.meter == Meter::Credits;
            let price = opts.pricing.price_of(ev.model.as_deref());
            // A credit-metered source has no per-token price by definition, so it is
            // "priced" rather than being filed under unknown models; where the vendor
            // publishes a plan price, its money is an estimate and gets flagged.
            let rate = crate::pricing::credit_rate(&ev.tool);
            let cost = if credits {
                rate.map(|(r, _)| ev.counts.credits * r).unwrap_or(0.0)
            } else {
                price.map(|p| p.cost_of(&ev.counts)).unwrap_or(0.0)
            };
            Row { ev, cost, priced: credits || price.is_some(), credit_money: credits && rate.is_some() }
        })
        .collect();
    // The scope filter, applied the same way `summarize_facts` applies it to
    // the index's per-origin rows. Quota samples and budget cost inputs stay
    // out of it on purpose: vendor windows and spend caps describe *this*
    // machine whichever machine's numbers are on screen.
    let keep = |source: &str| opts.scope.keeps(crate::origin_of(source));
    let rows: Vec<Row> = all_rows.iter().copied().filter(|r| keep(&r.ev.source)).collect();

    let (day_span, week_span, month_span, year_span) = spans(opts.now_ms);
    let day = build_window(day_span, &rows);
    let week = build_window(week_span, &rows);
    let month = build_window(month_span, &rows);
    let year = build_window(year_span, &rows);

    let mut heat: BTreeMap<NaiveDate, (f64, f64, u64)> = BTreeMap::new();
    for r in &rows {
        let Some(d) = date_of(r.ev.ts_ms) else { continue };
        let e = heat.entry(d).or_insert((0.0, 0.0, 0));
        e.0 += r.tokens();
        e.1 += r.cost;
        e.2 += 1;
    }
    let today = date_of(opts.now_ms).unwrap_or_else(|| Local::now().date_naive());
    let first = today - chrono::Duration::days(HEATMAP_DAYS - 1);
    let heatmap = (0..HEATMAP_DAYS)
        .map(|i| {
            let d = first + chrono::Duration::days(i);
            let (total_tokens, cost, requests) = heat.get(&d).copied().unwrap_or((0.0, 0.0, 0));
            HeatCell { date: d.format("%Y-%m-%d").to_string(), total_tokens, cost, requests }
        })
        .collect();

    // The same rows re-bucketed by local hour of today, for the 今日 chart.
    let mut hour_map: BTreeMap<u32, (f64, f64, u64)> = BTreeMap::new();
    for r in &rows {
        let Some(t) = to_local(r.ev.ts_ms) else { continue };
        if t.date_naive() != today {
            continue;
        }
        let e = hour_map.entry(t.hour()).or_insert((0.0, 0.0, 0));
        e.0 += r.tokens();
        e.1 += r.cost;
        e.2 += 1;
    }
    let hourly = (0..24)
        .map(|h| {
            let (total_tokens, cost, requests) = hour_map.get(&h).copied().unwrap_or((0.0, 0.0, 0));
            HourCell { total_tokens, cost, requests }
        })
        .collect();

    // Only the newest sample per window is meaningful, and a tool can report
    // several windows at once. The whole pipeline (newest-log-row-per-window,
    // probe override, expiry filter, budget bars, family sort) lives in
    // [`merge_quotas`], shared verbatim with the rollup fold so the two report
    // paths cannot drift. Its inputs are the *unscoped* rows: quotas are this
    // machine's, whatever scope the rest of the report was folded under.
    let (spent_today, spent_month) = match opts.scope {
        MachineScope::All => {
            (cost_by_tool(&day.breakdown.tools), cost_by_tool(&month.breakdown.tools))
        }
        _ => {
            let (d, _, m, _) = spans(opts.now_ms);
            let day_all = build_window(d, &all_rows);
            let month_all = build_window(m, &all_rows);
            (cost_by_tool(&day_all.breakdown.tools), cost_by_tool(&month_all.breakdown.tools))
        }
    };
    let quotas = merge_quotas(
        all_rows.iter().filter_map(|r| r.ev.quota.as_ref().map(|q| (r.ev.tool.as_str(), r.ev.ts_ms, q))),
        &opts.polled_quota,
        &spent_today,
        &spent_month,
        opts.now_ms,
        &opts.budgets,
    );

    let mut sessions: BTreeMap<(&str, &str), SessionRow> = BTreeMap::new();
    for r in &rows {
        let key = (r.ev.tool.as_str(), r.ev.session.as_str());
        let e = sessions.entry(key).or_insert_with(|| SessionRow {
            tool: r.ev.tool.clone(),
            session: r.ev.session.clone(),
            project: r.ev.project.clone(),
            model: r.ev.model.as_deref().map(model_display).unwrap_or("unknown").to_string(),
            first_ms: r.ev.ts_ms,
            last_ms: r.ev.ts_ms,
            total_tokens: 0.0,
            cost: 0.0,
            requests: 0,
        });
        e.first_ms = e.first_ms.min(r.ev.ts_ms);
        e.last_ms = e.last_ms.max(r.ev.ts_ms);
        e.total_tokens += r.tokens();
        e.cost += r.cost;
        e.requests += 1;
        if r.ev.model.is_some() {
            e.model = r.ev.model.as_deref().map(model_display).unwrap_or("unknown").to_string();
        }
        if e.project.is_none() {
            e.project = r.ev.project.clone();
        }
    }
    let mut recent_sessions: Vec<_> = sessions.into_values().collect();
    recent_sessions.sort_by_key(|s| -s.last_ms);
    recent_sessions.truncate(opts.recent_session_limit.max(1));

    // Unscoped by definition: the scope menu must list every machine present
    // in the input while one of them is selected. Same accumulation as
    // `summarize_facts`.
    let mut machine_acc: BTreeMap<&str, f64> = BTreeMap::new();
    for r in &all_rows {
        let entry = machine_acc.entry(crate::origin_of(&r.ev.source)).or_insert(0.0);
        if date_of(r.ev.ts_ms) == Some(today) {
            *entry += r.tokens();
        }
    }
    let machines: Vec<MachineView> = machine_acc
        .into_iter()
        .map(|(origin, today_tokens)| MachineView { origin: origin.to_string(), today_tokens })
        .collect();

    Report {
        generated_at_ms: opts.now_ms,
        utc_offset: utc_offset(opts.now_ms),
        day,
        week,
        month,
        year,
        heatmap,
        hourly,
        quotas,
        // The caller (the panel engine) flips this when its boot publish
        // skipped the probes; summarize itself has no opinion.
        quotas_pending: false,
        sources: opts.sources.clone(),
        syncs: Vec::new(),
        pricing: opts.pricing.meta().clone(),
        scope: opts.scope.clone(),
        machines,
        recent_sessions,
        all_time: summarize_events(&rows),
    }
}

// ---- P1: SQL rollup facts ----------------------------------------------------
//
// The panel engine no longer pulls the whole event history into Rust on every
// publish. `usage-index` maintains an `event_rollup` table — one row per local
// (day, tool, session, project, model, meter), ~2.2k rows for a ~390k-event
// index — and answers [`Index::report_facts`] with those rows plus a few small
// live slices over today's events. [`summarize_facts`] folds them into the same
// [`Report`] that [`summarize`] produces from raw events; the golden parity
// tests in `usage-index/src/facts.rs` enforce field-by-field equality.

/// Live-slice bucket ids in [`FactGroup::bucket`]. These slices run over the
/// `event` table (not the rollup) because they cover *partial* days: today's
/// running total and the head of each equal-elapsed prev slice.
pub const LIVE_TODAY: u32 = 100;
/// Prev-slice buckets, day/week/month/year: `LIVE_PREV + window ordinal`.
pub const LIVE_PREV: u32 = 101;
/// Today's hourly buckets: `LIVE_HOUR0 + hour`, 24 of them.
pub const LIVE_HOUR0: u32 = 200;

/// All period boundaries, computed in chrono from `now_ms`.
///
/// SQL never does timezone math: every bound is a local-midnight (or local-hour)
/// millisecond instant resolved here, so the rollup keys and the live slices can
/// never disagree with the fold about where a day starts. All ranges are
/// half-open `[lo, hi)`; a cur window's inclusive old-style `ts <= end` is
/// expressed as `hi = now_ms + 1` (integer ms, exact).
#[derive(Debug, Clone, PartialEq)]
pub struct AggregatePlan {
    pub now_ms: i64,
    pub today: NaiveDate,
    /// Local midnight opening each cur window: day, week, month, year (0..=3).
    pub cur_start_ms: [i64; 4],
    /// Start of each equal-elapsed prev slice; the window is `[prev_start, cur_start)`.
    pub prev_start_ms: [i64; 4],
    /// Next local midnight after each `prev_start`. The prev slice's *head* day
    /// is partial (it starts at `prev_start`, not midnight), so it is served
    /// live from `event`; every rollup day strictly between `date(prev_start)`
    /// and `date(cur_start)` is a whole day inside the window. Cur starts are
    /// midnights, so the tail of a prev window is never partial.
    pub prev_live_end_ms: [i64; 4],
    /// Today's local-midnight hour starts, 25 entries: `hour_start_ms[h]` opens
    /// hour h and closes hour h-1; `[24]` is tomorrow's midnight.
    pub hour_start_ms: [i64; 25],
    /// First date of the [`HEATMAP_DAYS`] window ending today.
    pub heatmap_first: NaiveDate,
    pub recent_session_limit: usize,
}

impl AggregatePlan {
    pub fn build(now_ms: i64, recent_session_limit: usize) -> Self {
        let (day, week, month, year) = spans(now_ms);
        let today = date_of(now_ms).unwrap_or_else(|| Local::now().date_naive());
        let prev_start_ms =
            [day.prev_start_ms, week.prev_start_ms, month.prev_start_ms, year.prev_start_ms];
        let prev_live_end_ms = [
            next_local_midnight(day.prev_start_ms, 1),
            next_local_midnight(week.prev_start_ms, 1),
            next_local_midnight(month.prev_start_ms, 1),
            next_local_midnight(year.prev_start_ms, 1),
        ];
        let day_start_ms = day.start_ms;
        let mut hour_start_ms = [0i64; 25];
        for (h, slot) in hour_start_ms.iter_mut().enumerate() {
            *slot = if h == 24 {
                start_of_day_ms(today + chrono::Duration::days(1))
            } else {
                // Wall-clock hour, not `day_start + h*3_600_000`: on a DST day the
                // linear offset drifts off the local hour the panel charts. Where
                // chrono cannot resolve a single instant (spring-forward gap,
                // fall-back repeat) it degrades to the linear offset, which keeps
                // the buckets monotone and non-overlapping; this machine (UTC+8)
                // never takes that branch.
                today
                    .and_hms_opt(h as u32, 0, 0)
                    .and_then(|naive| Local.from_local_datetime(&naive).single())
                    .map(|dt| dt.timestamp_millis())
                    .unwrap_or(day_start_ms + h as i64 * 3_600_000)
            };
        }
        Self {
            now_ms,
            today,
            cur_start_ms: [day.start_ms, week.start_ms, month.start_ms, year.start_ms],
            prev_start_ms,
            prev_live_end_ms,
            hour_start_ms,
            heatmap_first: today - chrono::Duration::days(HEATMAP_DAYS - 1),
            recent_session_limit: recent_session_limit.max(1),
        }
    }
}

/// One `event_rollup` row: every event of one local day that shares
/// (tool, session, project, model, meter), already summed by SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct RollupRow {
    /// `%Y-%m-%d` local date. `""` means the ts had no representable local day
    /// (corrupt source timestamp): it counts toward all_time only, exactly like
    /// the event path, where `date_of` returning `None` skips heatmap/hourly but
    /// keeps the row in every raw-ts aggregate.
    pub day: String,
    /// The machine these rows came from: `""` for this machine's own files,
    /// the bundle origin for rows merged by `tokenme import`. What
    /// [`MachineScope`] filters on.
    pub origin: String,
    pub tool: String,
    pub session: String,
    /// `''` sentinel in the table folds back to `None` here.
    pub project: Option<String>,
    pub model: Option<String>,
    pub meter: Meter,
    pub counts: TokenCounts,
    /// Events in the group.
    pub n: u64,
    /// Events with `counts.total() > 0` — the only ones the old path counted
    /// into `unpriced.requests`.
    pub nonzero: u64,
    pub min_ts: i64,
    pub max_ts: i64,
}

/// One group of a live slice over `event` (today, prev heads, hourly).
#[derive(Debug, Clone, PartialEq)]
pub struct FactGroup {
    pub bucket: u32,
    /// `""` for this machine's own rows — see [`RollupRow::origin`].
    pub origin: String,
    pub tool: String,
    pub session: String,
    pub project: Option<String>,
    pub model: Option<String>,
    pub meter: Meter,
    pub counts: TokenCounts,
    pub requests: u64,
    pub nonzero: u64,
}

/// One (kind, name) group of first-calls inside a cur window. Per the event
/// path's semantics, the *first* call of each kind in an event names the whole
/// event, whose full counts land under that name.
#[derive(Debug, Clone, PartialEq)]
pub struct CallFact {
    /// Cur-window ordinal: 0 = day, 1 = week, 2 = month, 3 = year.
    pub bucket: u32,
    pub kind: CallKind,
    pub name: String,
    /// `""` for this machine's own rows — see [`RollupRow::origin`].
    pub origin: String,
    pub tool: String,
    pub session: String,
    pub project: Option<String>,
    pub model: Option<String>,
    pub meter: Meter,
    pub counts: TokenCounts,
    pub requests: u64,
    pub nonzero: u64,
}

/// A log-derived quota sample that passed the SQL-side expiry pre-filter,
/// ordered by event id so [`merge_quotas`] tie semantics are preserved.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaFact {
    pub tool: String,
    pub ts_ms: i64,
    /// `used_percent`, `window_minutes`, `resets_at_ms`, `label` — the index
    /// never stores a stable window id, so `QuotaSample::id` is always `None`
    /// here, exactly as the event path receives it.
    pub sample: QuotaSample,
}

/// The (model, meter) sums of one recent session. Pricing is per group, the way
/// the event path prices per event — mathematically identical, FP-apart.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionGroup {
    pub model: Option<String>,
    pub meter: Meter,
    pub counts: TokenCounts,
    pub requests: u64,
}

/// One recent session, already resolved: `model` is its last non-null model by
/// event id, `project` its first non-null project, `first_ms`/`last_ms` the
/// session's min/max ts, groups the per-(model, meter) sums. Ordered by
/// `last_ms` desc, then tool, then session — the event path's stable sort over
/// a `(tool, session)` BTreeMap.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionFact {
    pub tool: String,
    pub session: String,
    pub model: Option<String>,
    pub project: Option<String>,
    pub first_ms: i64,
    pub last_ms: i64,
    pub groups: Vec<SessionGroup>,
}

/// What [`Index::report_facts`](usage_index::Index) hands to [`summarize_facts`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReportFacts {
    /// All `event_rollup` rows (whole history; ~2.2k rows at ~390k events).
    pub rollup: Vec<RollupRow>,
    /// Live slices over `event`: today, the four prev heads, the 24 hourly
    /// buckets. All bounds come from [`AggregatePlan`].
    pub live: Vec<FactGroup>,
    /// First-call (mcp/skill) groups for the four cur windows.
    pub calls: Vec<CallFact>,
    /// Expiry-prefiltered log quota samples in event-id order.
    pub quotas: Vec<QuotaFact>,
    /// The top-N recent sessions, resolved and ordered.
    pub sessions: Vec<SessionFact>,
    /// `meta`-derived sync records (one per imported origin), newest first.
    pub syncs: Vec<SyncRecord>,
    /// True when this call had to rebuild the rollup from `event` first (first
    /// publish after the upgrade, or after a fail-safe invalidation). The
    /// engine logs it; it does not change the report.
    pub rebuilt: bool,
}

/// Money resolved once per (tool, model, meter), not once per event. ~40
/// distinct pairs on the real index; the memo also makes the priced flag cheap.
/// The keys are owned on purpose: the fold feeds this borrows from loop-local
/// window vectors *and* from `facts`, and a borrowed key would pin both to one
/// region. The map holds ~40 entries, so the copies are nothing.
struct Money<'a> {
    pricing: &'a PricingMap,
    cache: HashMap<(String, Option<String>), Option<Price>>,
}

impl<'a> Money<'a> {
    fn new(pricing: &'a PricingMap) -> Self {
        Self { pricing, cache: HashMap::new() }
    }

    /// `(cost, priced)` for one group, mirroring the event path's per-event
    /// pricing rule: credits convert at the published plan rate (priced by
    /// definition), tokens cost via the per-token table or nothing.
    fn cost(&mut self, tool: &str, model: Option<&str>, counts: &TokenCounts, meter: Meter) -> (f64, bool) {
        if meter == Meter::Credits {
            let rate = crate::pricing::credit_rate(tool);
            return (rate.map(|(r, _)| counts.credits * r).unwrap_or(0.0), true);
        }
        let key = (tool.to_string(), model.map(str::to_string));
        let price = match self.cache.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => *e.get(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let p = self.pricing.price_of(model);
                e.insert(p);
                p
            }
        };
        (price.map(|p| p.cost_of(counts)).unwrap_or(0.0), price.is_some())
    }
}

/// How the fold sees one aggregate — a rollup row, a live group, or a call
/// fact — so the window math is written once. `name` is set only for call
/// facts (the mcp/skill breakdown key).
#[derive(Debug, Clone, Copy)]
struct G<'a> {
    tool: &'a str,
    session: &'a str,
    project: Option<&'a str>,
    model: Option<&'a str>,
    meter: Meter,
    counts: &'a TokenCounts,
    requests: u64,
    nonzero: u64,
    name: Option<&'a str>,
}

/// Views of one fact as a [`G`]. Plain `fn`s, not closures: a closure fixes one
/// output region per instantiation, while a `fn` item is generic over it, which
/// is what lets the same helper serve rollup rows and loop-local window vecs.
fn rollup_g(r: &RollupRow) -> G<'_> {
    G {
        tool: &r.tool,
        session: &r.session,
        project: r.project.as_deref(),
        model: r.model.as_deref(),
        meter: r.meter,
        counts: &r.counts,
        requests: r.n,
        nonzero: r.nonzero,
        name: None,
    }
}

fn live_g(f: &FactGroup) -> G<'_> {
    G {
        tool: &f.tool,
        session: &f.session,
        project: f.project.as_deref(),
        model: f.model.as_deref(),
        meter: f.meter,
        counts: &f.counts,
        requests: f.requests,
        nonzero: f.nonzero,
        name: None,
    }
}

fn call_g(c: &CallFact) -> G<'_> {
    G {
        tool: &c.tool,
        session: &c.session,
        project: c.project.as_deref(),
        model: c.model.as_deref(),
        meter: c.meter,
        counts: &c.counts,
        requests: c.requests,
        nonzero: c.nonzero,
        name: Some(&c.name),
    }
}

/// The group-wise twin of `summarize_events`. Requests are group event counts,
/// `nonzero` feeds `unpriced.requests`, sessions set-union across groups, and
/// unpriced filings happen per group (`!priced && total > 0`), which equals the
/// event-path rule because a group's priced flag is uniform: price depends only
/// on (tool, model, meter). Returns whether any constituent group was priced —
/// the breakdown items' `priced` flag.
fn fold_groups<'a>(groups: impl IntoIterator<Item = G<'a>>, money: &mut Money<'a>) -> (Summary, bool) {
    let mut s = Summary::default();
    let mut priced_any = false;
    let mut sessions: HashSet<(&str, &str)> = HashSet::new();
    let mut unpriced: HashMap<(&str, &str), UnpricedModel> = HashMap::new();
    for g in groups {
        let (cost, priced) = money.cost(g.tool, g.model, g.counts, g.meter);
        priced_any |= priced;
        s.counts += g.counts;
        s.cost += cost;
        if g.meter == Meter::Credits {
            s.credits += g.counts.credits;
            if cost > 0.0 || crate::pricing::credit_rate(g.tool).is_some() {
                s.credit_cost += cost;
            }
        }
        s.requests += g.requests;
        sessions.insert((g.tool, g.session));
        if !priced && g.counts.total() > 0.0 {
            let key = (g.tool, g.model.unwrap_or("unknown"));
            let e = unpriced.entry(key).or_insert_with(|| UnpricedModel {
                tool: key.0.to_string(),
                model: key.1.to_string(),
                total_tokens: 0.0,
                requests: 0,
            });
            e.total_tokens += g.counts.total();
            e.requests += g.nonzero;
        }
    }
    s.total_tokens = s.counts.total();
    s.sessions = sessions.len() as u64;
    s.cached_pct = s.counts.cached_pct();
    let mut unpriced: Vec<_> = unpriced.into_values().collect();
    unpriced.sort_by(|a, b| b.total_tokens.total_cmp(&a.total_tokens).then_with(|| a.model.cmp(&b.model)));
    s.unpriced = unpriced;
    (s, priced_any)
}

/// The group-wise twin of `group()`: fold aggregates under `key_of`, then sort
/// by cost desc, tokens desc, key asc — the same order, tie-break included.
fn items_from<'a>(
    groups: &[G<'a>],
    money: &mut Money<'a>,
    key_of: impl Fn(&G<'a>) -> Option<(String, String)>,
) -> Vec<Item> {
    let mut acc: BTreeMap<String, (String, Vec<&G<'a>>)> = BTreeMap::new();
    for g in groups {
        let Some((key, label)) = key_of(g) else { continue };
        if key.is_empty() {
            continue;
        }
        acc.entry(key).or_insert_with(|| (label, Vec::new())).1.push(g);
    }
    let mut items: Vec<Item> = acc
        .into_iter()
        .map(|(key, (label, group))| {
            let (s, priced) = fold_groups(group.into_iter().copied(), money);
            Item {
                key,
                label,
                counts: s.counts,
                total_tokens: s.total_tokens,
                cost: s.cost,
                requests: s.requests,
                sessions: s.sessions,
                priced,
            }
        })
        .collect();
    items.sort_by(|a, b| {
        b.cost.total_cmp(&a.cost)
            .then_with(|| b.total_tokens.total_cmp(&a.total_tokens))
            .then_with(|| a.key.cmp(&b.key))
    });
    items
}

/// Build the whole panel/CLI payload from [`ReportFacts`] — the SQL-rollup twin
/// of [`summarize`]. Produces the same `Report` for the same underlying events;
/// the golden parity tests in `usage-index/src/facts.rs` enforce it, so any
/// semantic change here or there must land on both sides of those tests.
pub fn summarize_facts(facts: &ReportFacts, opts: &ReportOptions) -> Report {
    let plan = AggregatePlan::build(opts.now_ms, opts.recent_session_limit);
    // Typed reborrow, not `opts.pricing`: copying the `&'a PricingMap` field out
    // would pin `Money`'s region to the *caller's* `'a`, which facts borrows can
    // never match. As a plain `&PricingMap` its region is inferred to cover this
    // body, and it unifies with them.
    let pricing: &PricingMap = opts.pricing;
    let mut money = Money::new(pricing);

    // Rollup days are `%Y-%m-%d` strings: lexicographic order is date order.
    let today_key = plan.today.format("%Y-%m-%d").to_string();
    let day_key = |ms: i64| -> String {
        local_day_of(ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
    };
    let cur_keys: Vec<String> = plan.cur_start_ms.iter().copied().map(day_key).collect();
    let prev_keys: Vec<String> = plan.prev_start_ms.iter().copied().map(day_key).collect();

    // The scope filter over every row's origin. Quotas and budget cost inputs
    // stay out of it on purpose: vendor windows and spend caps describe *this*
    // machine whichever machine's numbers are on screen.
    let keep = |origin: &str| opts.scope.keeps(origin);

    // Cur = whole rollup days from the window's local midnight through yesterday,
    // plus today's live slice; prev = whole days strictly between the prev head
    // day and the cur start, plus that head day's live tail. Together they
    // reassemble `[cur_start, now]` / `[prev_start, cur_start)` exactly.
    let mut windows = Vec::with_capacity(4);
    let (sp0, sp1, sp2, sp3) = spans(opts.now_ms);
    for (i, span) in [sp0, sp1, sp2, sp3].into_iter().enumerate() {
        let i = i as u32;
        let cur: Vec<G<'_>> = facts
            .rollup
            .iter()
            .filter(|r| {
                keep(&r.origin)
                    && r.day.as_str() >= cur_keys[i as usize].as_str()
                    && r.day.as_str() < today_key.as_str()
            })
            .map(rollup_g)
            .chain(
                facts
                    .live
                    .iter()
                    .filter(|f| f.bucket == LIVE_TODAY && keep(&f.origin))
                    .map(live_g),
            )
            .collect();
        let prev: Vec<G<'_>> = facts
            .rollup
            .iter()
            .filter(|r| {
                keep(&r.origin)
                    && r.day.as_str() > prev_keys[i as usize].as_str()
                    && r.day.as_str() < cur_keys[i as usize].as_str()
            })
            .map(rollup_g)
            .chain(
                facts
                    .live
                    .iter()
                    .filter(|f| f.bucket == LIVE_PREV + i && keep(&f.origin))
                    .map(live_g),
            )
            .collect();
        let (summary, _) = fold_groups(cur.iter().copied(), &mut money);
        let (prev_summary, _) = fold_groups(prev.iter().copied(), &mut money);
        // The first-call facts split by kind before grouping, so each breakdown
        // list folds only its own calls.
        let mcps: Vec<G<'_>> = facts
            .calls
            .iter()
            .filter(|c| c.bucket == i && c.kind == CallKind::Mcp && keep(&c.origin))
            .map(call_g)
            .collect();
        let skills: Vec<G<'_>> = facts
            .calls
            .iter()
            .filter(|c| c.bucket == i && c.kind == CallKind::Skill && keep(&c.origin))
            .map(call_g)
            .collect();
        let breakdown = Breakdown {
            tools: items_from(&cur, &mut money, |g| Some((g.tool.to_string(), g.tool.to_string()))),
            models: items_from(&cur, &mut money, |g| {
                let id = g.model.unwrap_or("unknown");
                Some((id.to_string(), model_display(id).to_string()))
            }),
            projects: items_from(&cur, &mut money, |g| {
                g.project.map(|p| (p.to_string(), p.to_string()))
            }),
            mcps: items_from(&mcps, &mut money, |g| {
                g.name.map(|n| (n.to_string(), n.to_string()))
            }),
            skills: items_from(&skills, &mut money, |g| {
                g.name.map(|n| (n.to_string(), n.to_string()))
            }),
        };
        windows.push(Window {
            key: span.key,
            label: span.label,
            start_ms: span.start_ms,
            end_ms: span.end_ms,
            delta_cost_pct: pct_delta(summary.cost, prev_summary.cost),
            delta_tokens_pct: pct_delta(summary.total_tokens, prev_summary.total_tokens),
            summary,
            prev: prev_summary,
            breakdown,
        });
    }
    let [day, week, month, year] = windows.try_into().expect("four windows");

    // Heatmap: one accumulator per local day over the rollup; only the 371-day
    // window is emitted, which is what the event path did too (it accumulated
    // everything, emitted the window). `""`-day rows never match a real date.
    let mut heat_acc: HashMap<&str, (f64, f64, u64)> = HashMap::new();
    for r in facts.rollup.iter().filter(|r| keep(&r.origin)) {
        let (cost, _) = money.cost(&r.tool, r.model.as_deref(), &r.counts, r.meter);
        let e = heat_acc.entry(r.day.as_str()).or_insert((0.0, 0.0, 0));
        e.0 += r.counts.total();
        e.1 += cost;
        e.2 += r.n;
    }
    let heatmap = (0..HEATMAP_DAYS)
        .map(|i| {
            let d = plan.heatmap_first + chrono::Duration::days(i);
            let (total_tokens, cost, requests) = heat_acc
                .get(d.format("%Y-%m-%d").to_string().as_str())
                .copied()
                .unwrap_or((0.0, 0.0, 0));
            HeatCell { date: d.format("%Y-%m-%d").to_string(), total_tokens, cost, requests }
        })
        .collect();

    // Hourly: today's live hour buckets, always 24 slots in index order.
    let mut hourly = Vec::with_capacity(24);
    for h in 0..24u32 {
        let (s, _) = fold_groups(
            facts.live.iter().filter(|f| f.bucket == LIVE_HOUR0 + h && keep(&f.origin)).map(live_g),
            &mut money,
        );
        hourly.push(HourCell { total_tokens: s.total_tokens, cost: s.cost, requests: s.requests });
    }

    // Budget bars keep this machine's own cost inputs even when the panel is
    // scoped to a remote machine — a spend cap is not the remote's. At `All`
    // the day/month breakdowns below already are those unscoped inputs.
    let (spent_today, spent_month) = match opts.scope {
        MachineScope::All => {
            (cost_by_tool(&day.breakdown.tools), cost_by_tool(&month.breakdown.tools))
        }
        _ => (
            cost_by_tool(&window_tool_items(facts, &cur_keys[0], &today_key, &mut money)),
            cost_by_tool(&window_tool_items(facts, &cur_keys[2], &today_key, &mut money)),
        ),
    };
    let quotas = merge_quotas(
        facts.quotas.iter().map(|q| (q.tool.as_str(), q.ts_ms, &q.sample)),
        &opts.polled_quota,
        &spent_today,
        &spent_month,
        opts.now_ms,
        &opts.budgets,
    );

    // Recent sessions arrive resolved and ordered from the index; the fold only
    // prices their groups and sums.
    let recent_sessions: Vec<SessionRow> = facts
        .sessions
        .iter()
        .map(|s| {
            let mut total_tokens = 0.0;
            let mut cost = 0.0;
            let mut requests = 0;
            for g in &s.groups {
                let (c, _) = money.cost(&s.tool, g.model.as_deref(), &g.counts, g.meter);
                cost += c;
                total_tokens += g.counts.total();
                requests += g.requests;
            }
            SessionRow {
                tool: s.tool.clone(),
                session: s.session.clone(),
                project: s.project.clone(),
                model: model_display(s.model.as_deref().unwrap_or("unknown")).to_string(),
                first_ms: s.first_ms,
                last_ms: s.last_ms,
                total_tokens,
                cost,
                requests,
            }
        })
        .take(opts.recent_session_limit.max(1))
        .collect();

    // Whole indexed history: every rollup row, including `""`-day ones — the
    // event path's all_time never filtered by date either.
    let (all_time, _) =
        fold_groups(facts.rollup.iter().filter(|r| keep(&r.origin)).map(rollup_g), &mut money);

    // Unscoped by definition: the menu lists every machine while one of them
    // is selected. Origins are enumerated from the rollup (a remote whose
    // today slice is empty still gets a row); the numbers are today's live
    // sums, `""` being this machine.
    let mut machine_acc: BTreeMap<&str, f64> = BTreeMap::new();
    for r in &facts.rollup {
        machine_acc.entry(r.origin.as_str()).or_insert(0.0);
    }
    for f in &facts.live {
        if f.bucket == LIVE_TODAY {
            *machine_acc.entry(f.origin.as_str()).or_default() += f.counts.total();
        }
    }
    let machines: Vec<MachineView> = machine_acc
        .into_iter()
        .map(|(origin, today_tokens)| MachineView { origin: origin.to_string(), today_tokens })
        .collect();

    Report {
        generated_at_ms: opts.now_ms,
        utc_offset: utc_offset(opts.now_ms),
        day,
        week,
        month,
        year,
        heatmap,
        hourly,
        quotas,
        quotas_pending: false,
        sources: opts.sources.clone(),
        syncs: facts.syncs.clone(),
        pricing: opts.pricing.meta().clone(),
        scope: opts.scope.clone(),
        machines,
        recent_sessions,
        all_time,
    }
}

/// The unscoped per-tool items of one cur window (whole rollup days from
/// `lo_key` through yesterday, plus today's live slice) — the budget bars'
/// input under a scoped report, where the window breakdown itself is filtered.
fn window_tool_items<'a>(
    facts: &'a ReportFacts,
    lo_key: &str,
    today_key: &str,
    money: &mut Money<'a>,
) -> Vec<Item> {
    let cur: Vec<G<'a>> = facts
        .rollup
        .iter()
        .filter(|r| r.day.as_str() >= lo_key && r.day.as_str() < today_key)
        .map(rollup_g)
        .chain(facts.live.iter().filter(|f| f.bucket == LIVE_TODAY).map(live_g))
        .collect();
    items_from(&cur, money, |g| Some((g.tool.to_string(), g.tool.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::PricingOptions;
    use crate::types::Call;

    fn pricing() -> PricingMap {
        PricingMap::load(&PricingOptions { offline: true, cache_dir: None, overrides: Default::default() })
    }

    /// A quota sample as it would arrive from a live probe or a log record.
    fn quota(tool: &str, window: i64, resets_at_ms: i64, used: f64, origin: QuotaOrigin) -> QuotaView {
        QuotaView {
            tool: tool.to_string(),
            used_percent: used,
            window_minutes: window,
            resets_at_ms,
            sampled_at_ms: resets_at_ms - 1,
            label: None,
            id: None,
            origin,
        }
    }

    fn ev(tool: &str, ts_ms: i64, session: &str, model: &str, counts: TokenCounts) -> UsageEvent {
        UsageEvent {
            dedupe_key: Some(format!("{tool}-{session}-{ts_ms}")),
            source: format!("/tmp/{tool}.jsonl"),
            meter: Meter::Tokens,
            project: Some("/work/app".into()),
            model: Some(model.into()),
            calls: vec![Call { kind: CallKind::Mcp, name: "bugx".into() }],
            quota: None,
            ..UsageEvent::new(tool, ts_ms, session).with(counts)
        }
    }

    fn ms_of(y: i32, mo: u32, d: u32, hh: u32) -> i64 {
        Local.with_ymd_and_hms(y, mo, d, hh, 0, 0).single().expect("local time exists").timestamp_millis()
    }

    #[test]
    fn today_counts_only_todays_events() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 14);
        let events = vec![
            ev("claude", now, "s1", "claude-sonnet-4-6", TokenCounts { input: 1_000_000.0, output: 100.0, ..Default::default() }),
            ev("claude", now - 86_400_000 * 3, "s0", "claude-sonnet-4-6", TokenCounts { input: 5_000_000.0, ..Default::default() }),
        ];
        let opts = ReportOptions::new(&p).with_now(now);
        let r = summarize(&events, &opts);
        assert_eq!(r.day.summary.requests, 1);
        assert_eq!(r.day.summary.total_tokens, 1_000_000.0 + 100.0);
        assert_eq!(r.all_time.requests, 2);
        assert_eq!(r.day.start_ms, ms_of(2026, 9, 23, 0));
        assert_eq!(r.day.key, "2026-09-23");
    }

    /// The 今日 chart's buckets: today's rows land in their local hour, other
    /// days never leak in, and the vec is always the full 24 slots.
    #[test]
    fn hourly_buckets_hold_todays_local_hours_only() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 14);
        let events = vec![
            ev("claude", ms_of(2026, 9, 23, 9), "s1", "claude-sonnet-4-6", TokenCounts { input: 100.0, ..Default::default() }),
            ev("claude", ms_of(2026, 9, 23, 9) + 1, "s2", "claude-sonnet-4-6", TokenCounts { input: 300.0, ..Default::default() }),
            ev("claude", ms_of(2026, 9, 23, 13), "s3", "claude-sonnet-4-6", TokenCounts { input: 500.0, ..Default::default() }),
            // Yesterday's 9 o'clock must not merge into today's 9 o'clock.
            ev("claude", ms_of(2026, 9, 22, 9), "s0", "claude-sonnet-4-6", TokenCounts { input: 9_900.0, ..Default::default() }),
        ];
        let opts = ReportOptions::new(&p).with_now(now);
        let r = summarize(&events, &opts);
        assert_eq!(r.hourly.len(), 24);
        assert_eq!(r.hourly[9].requests, 2);
        assert_eq!(r.hourly[9].total_tokens, 400.0);
        assert_eq!(r.hourly[13].total_tokens, 500.0);
        assert_eq!(r.hourly[8].total_tokens, 0.0);
        assert_eq!(r.hourly[10].total_tokens, 0.0);
        assert!(r.hourly.iter().enumerate().all(|(h, c)| h == 9 || h == 13 || c.total_tokens == 0.0));
    }

    #[test]
    fn delta_compares_against_equal_elapsed_previous_slice() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 14);
        // 09:00 yesterday is inside the equal-elapsed window; 20:00 is not.
        let events = vec![
            ev("claude", now, "s1", "claude-sonnet-4-6", TokenCounts { input: 100.0, ..Default::default() }),
            ev("claude", ms_of(2026, 9, 22, 9), "s0", "claude-sonnet-4-6", TokenCounts { input: 400.0, ..Default::default() }),
            ev("claude", ms_of(2026, 9, 22, 20), "s0", "claude-sonnet-4-6", TokenCounts { input: 900.0, ..Default::default() }),
        ];
        let opts = ReportOptions::new(&p).with_now(now);
        let r = summarize(&events, &opts);
        assert_eq!(r.day.prev.requests, 1);
        assert!(r.day.delta_cost_pct < 0.0, "cost fell vs yesterday slice: {}", r.day.delta_cost_pct);
    }

    /// A window whose reset has passed is stale: the tool has moved on, and one
    /// old log record must not keep a 0 % bar on the panel forever.
    #[test]
    fn an_expired_window_is_dropped_and_an_open_ended_one_is_not() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let opts = ReportOptions::new(&p)
            .with_now(now)
            .with_quota(vec![
                quota("codex", 43_200, now - 60_000, 0.0, QuotaOrigin::Log),
                quota("codex", 43_200, now + 3_600_000, 88.0, QuotaOrigin::Probe),
                quota("codex", 300, now + 3_600_000, 42.0, QuotaOrigin::Probe),
                quota("qoder", 0, 0, 100.0, QuotaOrigin::Probe),
            ]);
        let r = summarize(&[ev("codex", now, "s1", "gpt-5.6", TokenCounts { input: 10.0, ..Default::default() })], &opts);
        let windows: Vec<i64> = r.quotas.iter().map(|q| q.window_minutes).collect();
        assert!(!windows.contains(&43_200), "the closed monthly window is gone: {windows:?}");
        assert!(windows.contains(&300), "the live 5-hour window stays: {windows:?}");
        assert!(windows.contains(&0), "a plan that never reports a reset is open-ended, not expired: {windows:?}");
    }

    /// Budgets are computed from the same cost the panel shows above them.
    #[test]
    fn a_user_budget_becomes_a_quota_bar_measured_on_our_own_cost() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let events = vec![ev("zcode", now, "s1", "glm-5.3-flash", TokenCounts { input: 1_000_000.0, ..Default::default() })];
        let mut budgets = BTreeMap::new();
        budgets.insert("zcode".to_string(), crate::budget::Budget { daily_usd: 1.0, monthly_usd: 0.0 });
        let plain = summarize(&events, &ReportOptions::new(&p).with_now(now));
        let spent = plain.day.summary.cost;
        assert!(spent > 0.0, "the fixture model must be priced for this to mean anything");
        let r = summarize(&events, &ReportOptions::new(&p).with_now(now).with_budgets(budgets));
        let bar = r.quotas.iter().find(|q| q.origin == QuotaOrigin::Budget).expect("a budget bar");
        assert_eq!(bar.tool, "zcode");
        assert_eq!(bar.window_minutes, 1_440);
        assert!((bar.used_percent - spent * 100.0).abs() < 1e-6, "$1 cap on ${spent:.4} spent: {bar:?}");
        assert!(bar.resets_at_ms > now, "the cap lifts at the next local midnight");
    }

    #[test]
    fn credits_become_money_only_at_a_published_plan_price() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let mut e = ev("qoder", now, "s9", "qmodel", TokenCounts { credits: 0.5, ..Default::default() });
        e.meter = Meter::Credits;
        let opts = ReportOptions::new(&p).with_now(now);
        let r = summarize(&[e], &opts);
        assert_eq!(r.day.summary.credits, 0.5);
        assert_eq!(r.day.summary.credit_cost, 0.005, "$0.01/credit on 0.5 credits");
        assert_eq!(r.day.summary.requests, 1);
        assert!(r.day.summary.unpriced.is_empty(), "credits are not an unknown price");
    }

    #[test]
    fn unpriced_model_is_flagged_not_free() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let events = vec![ev("codex", now, "s3", "totally-unknown-model", TokenCounts { input: 1000.0, ..Default::default() })];
        let opts = ReportOptions::new(&p).with_now(now);
        let r = summarize(&events, &opts);
        assert_eq!(r.day.summary.cost, 0.0);
        assert_eq!(r.day.summary.unpriced.len(), 1);
        assert_eq!(r.day.summary.unpriced[0].total_tokens, 1000.0);
        assert!(!r.day.breakdown.models[0].priced);
    }

    #[test]
    fn heatmap_is_a_whole_number_of_weeks_ending_today() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let opts = ReportOptions::new(&p).with_now(now);
        let r = summarize(&[], &opts);
        assert_eq!(r.heatmap.len() as i64, HEATMAP_DAYS);
        assert_eq!(r.heatmap.last().unwrap().date, "2026-09-23");
        assert_eq!(r.heatmap.first().unwrap().date, "2025-09-18");
        assert!(r.heatmap.iter().all(|c| c.total_tokens == 0.0));
    }

    #[test]
    fn quota_keeps_only_the_newest_sample_per_tool() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let mut old = ev("codex", now - 60_000, "s", "gpt-5", TokenCounts { input: 1.0, ..Default::default() });
        old.quota = Some(crate::types::QuotaSample { used_percent: 16.0, window_minutes: 10_080, resets_at_ms: now, label: None, id: None });
        let mut new = ev("codex", now, "s", "gpt-5", TokenCounts { input: 1.0, ..Default::default() });
        new.quota = Some(crate::types::QuotaSample { used_percent: 42.0, window_minutes: 10_080, resets_at_ms: now, label: None, id: None });
        let opts = ReportOptions::new(&p).with_now(now);
        let r = summarize(&[old.clone(), new.clone()], &opts);
        assert_eq!(r.quotas.len(), 1);
        assert_eq!(r.quotas[0].used_percent, 42.0);
        let older_first = summarize(&[new, old], &opts);
        assert_eq!(older_first.quotas[0].used_percent, 42.0, "order of input must not matter");
    }

    /// Two meters of the same length are two meters. This is what silently
    /// deleted ZCode's monthly tool-call window (the monthly MCP window overwrote
    /// it) and would delete half of Antigravity's per-group windows.
    #[test]
    fn same_length_windows_with_different_names_both_survive() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let view = |label: &str, used: f64| QuotaView {
            tool: "zcode".into(),
            used_percent: used,
            window_minutes: 43_200,
            resets_at_ms: now + 86_400_000,
            sampled_at_ms: now,
            label: Some(label.into()),
            id: None,
            origin: QuotaOrigin::Probe,
        };
        let opts = ReportOptions::new(&p)
            .with_now(now)
            .with_quota(vec![view("1 月 · GLM Coding Lite", 12.0), view("MCP 调用 · GLM Coding Lite", 40.0)]);
        let r = summarize(&[], &opts);
        assert_eq!(r.quotas.len(), 2, "{:?}", r.quotas.iter().map(|q| &q.label).collect::<Vec<_>>());
        assert_eq!(r.quotas.iter().map(|q| q.used_percent).collect::<Vec<_>>(), vec![12.0, 40.0]);

        // A repeated name is still the same window: the later sample wins, no twin.
        let opts = ReportOptions::new(&p)
            .with_now(now)
            .with_quota(vec![view("1 月 · GLM Coding Lite", 12.0), view("1月 · GLM Coding Lite", 18.0)]);
        let r = summarize(&[], &opts);
        assert_eq!(r.quotas.len(), 1, "whitespace is not a second window");
        assert_eq!(r.quotas[0].used_percent, 18.0);
    }

    /// …but an unnamed row and a named row of the same length are the same window,
    /// which is how Codex's log record yields to the live probe.
    #[test]
    fn an_unnamed_log_row_yields_to_a_named_probe_of_the_same_length() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let mut logged = ev("codex", now - 1000, "s", "gpt-5", TokenCounts { input: 1.0, ..Default::default() });
        logged.quota = Some(crate::types::QuotaSample {
            used_percent: 9.0,
            window_minutes: 300,
            resets_at_ms: now + 3_600_000,
            label: None,
            id: None,
        });
        let polled = QuotaView {
            tool: "codex".into(),
            used_percent: 61.0,
            window_minutes: 300,
            resets_at_ms: now + 3_000_000,
            sampled_at_ms: now,
            label: Some("5 小时".into()),
            id: None,
            origin: QuotaOrigin::Probe,
        };
        let r = summarize(&[logged], &ReportOptions::new(&p).with_now(now).with_quota(vec![polled]));
        assert_eq!(r.quotas.len(), 1, "one window, not a named twin: {:?}", r.quotas);
        assert_eq!(r.quotas[0].used_percent, 61.0);
        assert_eq!(r.quotas[0].origin, QuotaOrigin::Probe);
    }

    /// Antigravity answers with a 5-hour and a weekly window per model group.
    /// The old length-then-label sort interleaved the two models; the groups
    /// now keep the probe's own order (the vendor's — Gemini first) with each
    /// model's two windows adjacent, whatever the percentages say.
    #[test]
    fn a_model_group_keeps_its_windows_together() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let view = |label: &str, minutes: i64, used: f64| QuotaView {
            tool: "antigravity".into(),
            used_percent: used,
            window_minutes: minutes,
            resets_at_ms: now + 86_400_000,
            sampled_at_ms: now,
            label: Some(label.into()),
            id: None,
            origin: QuotaOrigin::Probe,
        };
        let opts = ReportOptions::new(&p).with_now(now).with_quota(vec![
            view("Gemini · 5 小时", 300, 41.0),
            view("Gemini · 周", 10_080, 34.0),
            view("Claude/GPT · 5 小时", 300, 0.0),
            view("Claude/GPT · 周", 10_080, 0.0),
        ]);
        let r = summarize(&[], &opts);
        let labels: Vec<String> = r.quotas.iter().map(|q| q.label.clone().unwrap_or_default()).collect();
        assert_eq!(
            labels,
            vec!["Gemini · 5 小时", "Gemini · 周", "Claude/GPT · 5 小时", "Claude/GPT · 周"],
            "{labels:?}"
        );
    }

    /// A bare window label names no subject, so it keeps the old order: window
    /// length first, the probe's rows untouched by any family grouping.
    #[test]
    fn unlabeled_windows_still_sort_by_length() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let view = |label: &str, minutes: i64| QuotaView {
            tool: "codex".into(),
            used_percent: 50.0,
            window_minutes: minutes,
            resets_at_ms: now + 86_400_000,
            sampled_at_ms: now,
            label: Some(label.into()),
            id: None,
            origin: QuotaOrigin::Probe,
        };
        let opts = ReportOptions::new(&p)
            .with_now(now)
            .with_quota(vec![view("周", 10_080), view("5 小时", 300)]);
        let r = summarize(&[], &opts);
        let labels: Vec<String> = r.quotas.iter().map(|q| q.label.clone().unwrap_or_default()).collect();
        assert_eq!(labels, vec!["5 小时", "周"], "{labels:?}");
    }

    /// ZCode's meters are one window per subject: no grouping applies, and the
    /// rows keep the window-length order instead of following the vendor's
    /// limits array (which puts the idle tool-call meter first).
    #[test]
    fn single_window_subjects_keep_the_length_order() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let view = |label: &str, minutes: i64, used: f64| QuotaView {
            tool: "zcode".into(),
            used_percent: used,
            window_minutes: minutes,
            resets_at_ms: now + 86_400_000,
            sampled_at_ms: now,
            label: Some(label.into()),
            id: None,
            origin: QuotaOrigin::Probe,
        };
        let opts = ReportOptions::new(&p).with_now(now).with_quota(vec![
            view("工具调用 · GLM Coding Lite", 43_200, 0.0),
            view("5 小时 · GLM Coding Lite", 300, 11.0),
            view("ZCode MCP · GLM Coding Lite", 216_000, 0.0),
        ]);
        let r = summarize(&[], &opts);
        let labels: Vec<String> = r.quotas.iter().map(|q| q.label.clone().unwrap_or_default()).collect();
        assert_eq!(labels, vec!["5 小时 · GLM Coding Lite", "工具调用 · GLM Coding Lite", "ZCode MCP · GLM Coding Lite"], "{labels:?}");
    }

    #[test]
    fn a_log_quota_row_without_a_reset_expires_by_age() {
        // A probe that stops writing (or changes its labels, as the Qoder one did
        // when "credits · 已用完" was retired) leaves rows no reset can age out.
        let p = pricing();
        let now = ms_of(2026, 9, 24, 15);
        let mut stale = ev("qoder", now - 2 * 86_400_000, "s", "qoder", TokenCounts::default());
        stale.quota = Some(crate::types::QuotaSample {
            used_percent: 100.0,
            window_minutes: 0,
            resets_at_ms: 0,
            label: Some("credits · 已用完".into()),
            id: None,
        });
        let mut fresh = ev("zcode", now - 3_600_000, "s", "glm-4.7", TokenCounts::default());
        fresh.quota = Some(crate::types::QuotaSample {
            used_percent: 38.0,
            window_minutes: 300,
            resets_at_ms: 0,
            label: Some("5 小时".into()),
            id: None,
        });
        let mut reset_governed = ev("codex", now - 2 * 86_400_000, "s", "gpt-5", TokenCounts::default());
        reset_governed.quota = Some(crate::types::QuotaSample {
            used_percent: 50.0,
            window_minutes: 300,
            resets_at_ms: now + 3_600_000,
            label: Some("5 小时".into()),
            id: None,
        });
        let r = summarize(&[stale, fresh, reset_governed], &ReportOptions::new(&p).with_now(now));
        let labels: Vec<&str> = r.quotas.iter().map(|q| q.label.as_deref().unwrap_or_default()).collect();
        assert!(!labels.iter().any(|l| l.contains("已用完")), "{labels:?} — a day-old no-reset row is not a meter");
        let fresh_row = r.quotas.iter().find(|q| q.tool == "zcode").expect("the fresh no-reset row stays");
        assert_eq!(fresh_row.origin, QuotaOrigin::Log);
        assert_eq!(r.quotas.len(), 2, "{labels:?} — the reset-governed row is judged by its reset, not by its age");
    }

    #[test]
    fn sessions_aggregate_across_turns() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let events = vec![
            ev("claude", now - 1000, "s1", "claude-sonnet-4-6", TokenCounts { input: 10.0, ..Default::default() }),
            ev("claude", now, "s1", "glm-5.3-flash", TokenCounts { input: 20.0, ..Default::default() }),
        ];
        let opts = ReportOptions::new(&p).with_now(now);
        let r = summarize(&events, &opts);
        assert_eq!(r.recent_sessions.len(), 1);
        assert_eq!(r.recent_sessions[0].requests, 2);
        assert_eq!(r.recent_sessions[0].model, "glm-5.3-flash", "latest model wins");
        assert_eq!(r.day.summary.sessions, 1);
    }

    #[test]
    fn breakdowns_split_tools_models_and_mcp_calls() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let mut codex = ev("codex", now, "s2", "gpt-5", TokenCounts { input: 1_000.0, ..Default::default() });
        codex.calls = vec![Call { kind: CallKind::Skill, name: "review".into() }];
        let events = vec![
            ev("claude", now, "s1", "claude-sonnet-4-6", TokenCounts { input: 2_000.0, ..Default::default() }),
            codex,
        ];
        let opts = ReportOptions::new(&p).with_now(now);
        let r = summarize(&events, &opts);
        assert_eq!(r.day.breakdown.tools.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(), vec!["claude", "codex"]);
        assert_eq!(r.day.breakdown.models.len(), 2);
        assert_eq!(r.day.breakdown.mcps.len(), 1);
        assert_eq!(r.day.breakdown.skills[0].key, "review");
        assert_eq!(r.day.summary.requests, 2);
    }

    #[test]
    fn week_and_month_keys_use_local_boundaries() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 14); // Wednesday of ISO week 39
        let opts = ReportOptions::new(&p).with_now(now);
        let r = summarize(&[], &opts);
        assert_eq!(r.week.key, "2026-W39");
        assert_eq!(r.month.key, "2026-09");
        assert_eq!(r.month.start_ms, ms_of(2026, 9, 1, 0));
    }

    #[test]
    fn a_polled_sample_overrides_the_log_for_its_own_window_only() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let mut ev5h = ev("codex", now, "s", "gpt-5", TokenCounts { input: 1.0, ..Default::default() });
        ev5h.quota = Some(crate::types::QuotaSample {
            used_percent: 30.0,
            window_minutes: 300,
            resets_at_ms: now + 60_000,
            label: None,
            id: None,
        });
        let mut week = ev("codex", now, "s", "gpt-5", TokenCounts { input: 1.0, ..Default::default() });
        week.quota = Some(crate::types::QuotaSample {
            used_percent: 5.0,
            window_minutes: 10_080,
            resets_at_ms: now + 86_400_000,
            label: None,
            id: None,
        });
        let polled = QuotaView {
            tool: "codex".into(),
            used_percent: 91.0,
            window_minutes: 300,
            resets_at_ms: now + 30 * 60_000,
            sampled_at_ms: now,
            label: Some("5 小时".into()),
            id: None,
            origin: QuotaOrigin::Probe,
        };
        let opts = ReportOptions::new(&p).with_now(now).with_quota(vec![polled.clone()]);
        let r = summarize(&[ev5h, week], &opts);
        assert_eq!(r.quotas.len(), 2, "the weekly window survives a probe of the 5-hour one");
        let five = r.quotas.iter().find(|q| q.window_minutes == 300).unwrap();
        assert_eq!(five.used_percent, 91.0, "the probe wins for the same window");
        assert_eq!(five.origin, QuotaOrigin::Probe);
        assert_eq!(five.label.as_deref(), Some("5 小时"));
        let weekly = r.quotas.iter().find(|q| q.window_minutes == 10_080).unwrap();
        assert_eq!(weekly.origin, QuotaOrigin::Log);
        // Same input without a probe must be unchanged, so the CLI and the panel
        // agree whenever nothing is polled.
        let plain = summarize(&[ev("codex", now, "s", "gpt-5", TokenCounts { input: 1.0, ..Default::default() })], &ReportOptions::new(&p).with_now(now));
        assert_eq!(plain.quotas.len(), 0);
    }

    /// A report may never look older than the numbers inside it: a probe that
    /// answers after the caller's instant is still part of the report.
    #[test]
    fn a_polled_sample_never_lands_in_the_reports_own_future() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let late = QuotaView {
            tool: "antigravity".into(),
            used_percent: 12.0,
            window_minutes: 10_080,
            resets_at_ms: now + 86_400_000,
            sampled_at_ms: now + 17_000,
            label: Some("周".into()),
            id: None,
            origin: QuotaOrigin::Probe,
        };
        let opts = ReportOptions::new(&p)
            .with_now(instant_after_polling(false, now, std::slice::from_ref(&late)))
            .with_quota(vec![late.clone()]);
        let r = summarize(
            &[ev("claude", now, "s", "claude-sonnet-4-6", TokenCounts { input: 1.0, ..Default::default() })],
            &opts,
        );
        assert_eq!(r.generated_at_ms, now + 17_000, "stamped when the probe answered");
        assert!(r.quotas.iter().all(|q| q.sampled_at_ms <= r.generated_at_ms), "nothing is from the future");

        // Nothing polled, or a back-dated report: the caller's instant stands, so
        // `--until` never drifts to today because a probe happened to answer.
        assert_eq!(instant_after_polling(false, now, &[]), now);
        assert_eq!(instant_after_polling(true, now, &[late]), now);
    }

    /// The MCP meter's length is the vendor's reset gap and shrinks on every
    /// poll. Sorted by raw length its row would climb past the 5-hour and daily
    /// bars as it decays — the rank pins implied lengths after nominal ones.
    #[test]
    fn an_implied_window_length_does_not_climb_over_nominal_ones() {
        let p = pricing();
        let now = ms_of(2026, 9, 23, 10);
        let five = quota("zcode", 300, now + 3_600_000, 10.0, QuotaOrigin::Probe);
        let mut drifted = quota("zcode", 499, now + 499 * 60_000, 5.0, QuotaOrigin::Probe);
        drifted.label = Some("ZCode MCP".into());
        let mut decayed = quota("zcode", 61, now + 61 * 60_000, 5.0, QuotaOrigin::Probe);
        decayed.label = Some("ZCode MCP".into());

        let fresh = summarize(&[], &ReportOptions::new(&p).with_now(now).with_quota(vec![drifted, five.clone()]));
        let later = summarize(&[], &ReportOptions::new(&p).with_now(now).with_quota(vec![decayed, five]));
        assert_eq!(fresh.quotas[0].window_minutes, 300, "{:?}", fresh.quotas);
        assert_eq!(later.quotas[0].window_minutes, 300, "61 min must not outrank the 5-hour window either");
        assert_eq!(fresh.quotas[0].tool, later.quotas[0].tool);
    }
}

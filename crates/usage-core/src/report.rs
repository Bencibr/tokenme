//! The single aggregation both frontends read. Keeping the period math, money
//! math and breakdown grouping here is what stops the menu-bar panel and the CLI
//! from ever disagreeing about a number.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::AddAssign;
use std::path::PathBuf;

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};

use crate::budget::Budget;
use crate::pricing::{PricingMap, PricingMeta};
use crate::types::{CallKind, Meter, TokenCounts, UsageEvent};

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
    pub heatmap: Vec<HeatCell>,
    pub quotas: Vec<QuotaView>,
    pub sources: Vec<SourceStatus>,
    pub pricing: PricingMeta,
    pub recent_sessions: Vec<SessionRow>,
    /// Whole indexed history, for the "总计" line.
    pub all_time: Summary,
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
fn spans(now_ms: i64) -> (Span, Span, Span) {
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
    (day, week, month)
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
        b.cost.total_cmp(&a.cost).then_with(|| b.total_tokens.total_cmp(&a.total_tokens))
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
        models: group(&cur, |e| Some((e.model.as_deref().unwrap_or("unknown"), e.model.as_deref().unwrap_or("unknown")))),
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

/// Build the whole panel/CLI payload from already-deduplicated events.
pub fn summarize(events: &[UsageEvent], opts: &ReportOptions) -> Report {
    let rows: Vec<Row> = events
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

    let (day_span, week_span, month_span) = spans(opts.now_ms);
    let day = build_window(day_span, &rows);
    let week = build_window(week_span, &rows);
    let month = build_window(month_span, &rows);

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

    // Only the newest sample per window is meaningful, and a tool can report
    // several windows at once. The window's *name* is part of its identity: ZCode
    // has a monthly tool-call meter and a monthly MCP meter, and Antigravity has a
    // 5-hour window per model group — keyed on length alone, the second silently
    // replaced the first. A row with no name (Codex writes its window into the log
    // without one) is the same window as the named probe of it, so an empty name
    // matches anything.
    let mut quotas: BTreeMap<(String, i64, String), QuotaView> = BTreeMap::new();
    for r in &rows {
        let Some(q) = r.ev.quota.as_ref() else { continue };
        let key = (r.ev.tool.clone(), q.window_minutes, label_key(q.label.as_deref()));
        let better = quotas.get(&key).is_none_or(|cur| r.ev.ts_ms >= cur.sampled_at_ms);
        if better {
            quotas.insert(
                key,
                QuotaView {
                    tool: r.ev.tool.clone(),
                    used_percent: q.used_percent,
                    window_minutes: q.window_minutes,
                    resets_at_ms: q.resets_at_ms,
                    sampled_at_ms: r.ev.ts_ms,
                    label: q.label.clone(),
                    id: q.id.clone(),
                    origin: QuotaOrigin::Log,
                },
            );
        }
    }
    // A live probe is fresher than whatever the last log record happened to carry.
    for polled in &opts.polled_quota {
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
            (0, QuotaOrigin::Log) => q.sampled_at_ms + LOG_ROW_LIFETIME_MS > opts.now_ms,
            _ => q.resets_at_ms == 0 || q.resets_at_ms >= opts.now_ms,
        })
        .collect();
    // Budgets are measured on the same local boundaries the windows above use, so
    // "80 % of today's cap" and today's cost line can never disagree.
    quotas.extend(crate::budget::views(
        &opts.budgets,
        &cost_by_tool(&day.breakdown.tools),
        &cost_by_tool(&month.breakdown.tools),
        opts.now_ms,
        next_local_midnight(opts.now_ms, 1),
        next_local_midnight(opts.now_ms, first_of_next_month_days(opts.now_ms)),
    ));
    quotas.sort_by(|a, b| {
        a.tool.cmp(&b.tool)
            .then_with(|| window_rank(a.window_minutes).cmp(&window_rank(b.window_minutes)))
            .then_with(|| a.label.cmp(&b.label))
    });


    let mut sessions: BTreeMap<(&str, &str), SessionRow> = BTreeMap::new();
    for r in &rows {
        let key = (r.ev.tool.as_str(), r.ev.session.as_str());
        let e = sessions.entry(key).or_insert_with(|| SessionRow {
            tool: r.ev.tool.clone(),
            session: r.ev.session.clone(),
            project: r.ev.project.clone(),
            model: r.ev.model.clone().unwrap_or_else(|| "unknown".into()),
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
            e.model = r.ev.model.clone().unwrap_or_else(|| "unknown".into());
        }
        if e.project.is_none() {
            e.project = r.ev.project.clone();
        }
    }
    let mut recent_sessions: Vec<_> = sessions.into_values().collect();
    recent_sessions.sort_by_key(|s| -s.last_ms);
    recent_sessions.truncate(opts.recent_session_limit.max(1));

    Report {
        generated_at_ms: opts.now_ms,
        utc_offset: utc_offset(opts.now_ms),
        day,
        week,
        month,
        heatmap,
        quotas,
        sources: opts.sources.clone(),
        pricing: opts.pricing.meta().clone(),
        recent_sessions,
        all_time: summarize_events(&rows),
    }
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

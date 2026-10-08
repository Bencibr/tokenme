//! Subcommand implementations. Every number rendered here comes from
//! `usage_core::report::summarize`; this file only slices events and formats.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{Datelike, Local, TimeZone};
use usage_core::{summarize, DateFilter, Item, Summary, UsageEvent, Window};
use usage_index::{retention_cutoff, IngestReport};

use crate::args::{Group, Win};
use crate::context::{print_json, Ctx};
use crate::render::{self, cost_cell, Table};

const MS_DAY: i64 = 86_400_000;
pub const MAX_PERIODS: i64 = 90;

// ---------------------------------------------------------------- budget

/// Read, set and clear the caps in `tokenme/settings.json`. No index needed: this
/// only touches the settings file, and the report picks the caps up from there.
pub fn budget(action: &crate::args::BudgetCmd, json: bool) -> Result<(), String> {
    use usage_core::budget::{load_budgets, store_budget, Budget};

    match action {
        crate::args::BudgetCmd::List => {
            let budgets = load_budgets();
            if json {
                crate::context::print_json(&budgets);
                return Ok(());
            }
            if budgets.is_empty() {
                println!("no budgets set — tokenme can only show vendor-reported windows until you add one");
                println!("  tokenme budget set <tool> --daily 5 --monthly 50");
                return Ok(());
            }
            let mut t = Table::new(&["Tool", "Daily", "Monthly"]).right(&[1, 2]);
            for (tool, b) in &budgets {
                t.push(vec![
                    tool.clone(),
                    if b.daily_usd > 0.0 { format!("${:.2}", b.daily_usd) } else { "-".into() },
                    if b.monthly_usd > 0.0 { format!("${:.2}", b.monthly_usd) } else { "-".into() },
                ]);
            }
            print!("{}", t.render(false));
            Ok(())
        }
        crate::args::BudgetCmd::Set { tool, daily, monthly } => {
            if daily.is_none() && monthly.is_none() {
                return Err("give at least one of --daily / --monthly".into());
            }
            let previous = load_budgets().remove(tool.as_str()).unwrap_or_default();
            let budget = Budget {
                daily_usd: daily.unwrap_or(if tool_is_new(tool) { 0.0 } else { previous.daily_usd }),
                monthly_usd: monthly.unwrap_or(previous.monthly_usd),
            };
            let path = store_budget(tool, budget).map_err(|e| e.to_string())?;
            println!(
                "{}: {} → {}",
                tool,
                if budget.is_empty() { "no cap".into() } else {
                    [
                        budget.daily_usd.gt(&0.0).then(|| format!("${:.0}/天", budget.daily_usd)),
                        budget.monthly_usd.gt(&0.0).then(|| format!("${:.0}/月", budget.monthly_usd)),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ")
                },
                path.display()
            );
            Ok(())
        }
        crate::args::BudgetCmd::Rm { tool } => {
            store_budget(tool, Budget::default()).map_err(|e| e.to_string())?;
            println!("{tool}: cap removed");
            Ok(())
        }
    }
}

/// `set x --monthly 50` on a tool with no stored cap must not inherit a stale
/// daily figure from a previous run of a different tool.
fn tool_is_new(tool: &str) -> bool {
    !usage_core::budget::load_budgets().contains_key(tool)
}

// ---------------------------------------------------------------- detect

pub fn detect(ctx: &Ctx) -> Result<(), String> {
    let (detected, problems) = ctx.detect();
    if !ctx.quiet() {
        for p in &problems {
            eprintln!("tokenme: {p}");
        }
    }
    let statuses = ctx.index.source_statuses(&detected).map_err(|e| e.to_string())?;

    if ctx.g.json {
        print_json(&statuses);
    } else {
        let mut t = Table::new(&["Tool", "Found", "Events", "Roots", "Hint"]).right(&[2]);
        let mut seen: Vec<&str> = Vec::new();
        for s in &statuses {
            seen.push(s.id.as_str());
            let found = if s.detected {
                render::paint("yes", "32", ctx.color)
            } else {
                render::paint("no", "2", ctx.color)
            };
            let problem = problems.iter().find(|p| p.starts_with(&format!("{}:", s.id)));
            let hint = match (problem, s.hint.as_deref()) {
                (Some(p), _) => render::truncate(p, 52),
                (None, Some(h)) => render::truncate(h, 52),
                (None, None) if s.detected => "ready".into(),
                (None, None) => "no readable logs here yet".into(),
            };
            t.push(vec![
                s.display.clone(),
                found,
                render::count(s.events_ingested),
                render::truncate(
                    &s.roots
                        .iter()
                        .map(|p| render::shorten_path(&p.to_string_lossy()))
                        .collect::<Vec<_>>()
                        .join("  "),
                    46,
                ),
                hint,
            ]);
        }
        // A built-in source that is neither detected nor indexed still belongs in
        // the table: "why is my Codex history missing" is the whole point.
        for adapter in &ctx.adapters {
            if seen.contains(&adapter.id()) {
                continue;
            }
            let problem = problems.iter().find(|p| p.starts_with(&format!("{}:", adapter.id())));
            t.push(vec![
                adapter.display_name().to_string(),
                render::paint("no", "2", ctx.color),
                "0".into(),
                String::new(),
                render::truncate(
                    problem.map(String::as_str).unwrap_or("not installed or no logs yet"),
                    52,
                ),
            ]);
        }
        print!("{}", t.render(ctx.color));
        println!(
            "{}",
            render::dim(&index_line(ctx, None), ctx.color)
        );
    }

    if detected.is_empty() {
        return Err(format!(
            "no supported AI tool logs found (looked for {})",
            usage_adapter_all::TOOL_IDS.join(", ")
        ));
    }
    Ok(())
}

fn index_line(ctx: &Ctx, report: Option<&IngestReport>) -> String {
    let total = ctx.index.event_count().unwrap_or(0);
    let files = ctx.index.files_tracked().unwrap_or(0);
    let age = ctx
        .index
        .meta_value("last_ingest_ms")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
        .map(|ms| render::ago(ctx.now_ms - ms))
        .unwrap_or_else(|| "never".into());
    match report {
        Some(r) => {
            // The pass's own voice: read failures and the adapters' notes about
            // silent ones. Empty on a healthy pass, so nothing changes there.
            let mut extra = String::new();
            for e in &r.errors {
                extra.push_str(&format!("\n  error: {e}"));
            }
            for n in &r.notes {
                extra.push_str(&format!("\n  note: {n}"));
            }
            format!(
                "index {} · {} events from {files} files · {} new, {} deduped, {} purged in {}ms{extra}",
                render::shorten_path(&ctx.db_path.display().to_string()),
                render::count(total),
                r.new_events,
                r.deduped,
                r.purged,
                r.took_ms
            )
        }
        None => format!(
            "index {} · {} events from {files} files · last ingest {age}",
            render::shorten_path(&ctx.db_path.display().to_string()),
            render::count(total)
        ),
    }
}

/// ------------------------------------------------------------ workbuddy-login

pub fn workbuddy_login() -> Result<(), String> {
    let summary = usage_quota::workbuddy_login()?;
    println!("{summary}");
    println!("run `tokenme quota` to see the WorkBuddy credit bars");
    Ok(())
}

// -------------------------------------------------- daily / weekly / month

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grain {
    Day,
    Week,
    Month,
}

impl Grain {
    pub fn label(self) -> &'static str {
        match self {
            Grain::Day => "day",
            Grain::Week => "week",
            Grain::Month => "month",
        }
    }
}

#[derive(Debug)]
struct Period {
    key: String,
    label: String,
    start_ms: i64,
    end_ms: i64,
}

fn local_date(ms: i64) -> Option<chrono::NaiveDate> {
    Local.timestamp_millis_opt(ms).single().map(|d| d.date_naive())
}

fn start_ms_of(d: chrono::NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0)
        .and_then(|t| Local.from_local_datetime(&t).single())
        .map(|t| t.timestamp_millis())
        .unwrap_or(0)
}

fn monday_of(d: chrono::NaiveDate) -> chrono::NaiveDate {
    d - chrono::Duration::days(i64::from(d.weekday().num_days_from_monday()))
}

fn period_of(ms: i64, grain: Grain) -> Period {
    let today = local_date(ms).unwrap_or_else(|| Local::now().date_naive());
    match grain {
        Grain::Day => {
            let start = start_ms_of(today);
            Period { key: today.format("%Y-%m-%d").to_string(), label: today.format("%Y-%m-%d").to_string(), start_ms: start, end_ms: start + MS_DAY - 1 }
        }
        Grain::Week => {
            let monday = monday_of(today);
            let start = start_ms_of(monday);
            Period {
                key: format!("{}-W{:02}", monday.iso_week().year(), monday.iso_week().week()),
                label: format!("{}..{}", monday.format("%m-%d"), (monday + chrono::Duration::days(6)).format("%m-%d")),
                start_ms: start,
                end_ms: start + 7 * MS_DAY - 1,
            }
        }
        Grain::Month => {
            let first = today.with_day(1).unwrap_or(today);
            let start = start_ms_of(first);
            // End of the month = start of the next one minus a millisecond.
            let next = if first.month() == 12 {
                chrono::NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
            } else {
                chrono::NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
            };
            Period {
                key: first.format("%Y-%m").to_string(),
                label: first.format("%Y-%m").to_string(),
                start_ms: start,
                end_ms: next.map(start_ms_of).unwrap_or(start + 31 * MS_DAY) - 1,
            }
        }
    }
}

/// The last `n` periods, oldest first, ending with the one containing `now_ms`.
fn trailing_periods(now_ms: i64, grain: Grain, n: i64) -> Vec<Period> {
    let mut out = Vec::with_capacity(n as usize);
    let mut cursor = now_ms;
    for _ in 0..n.max(1) {
        let p = period_of(cursor, grain);
        cursor = p.start_ms - 1;
        out.push(p);
    }
    out.reverse();
    out
}

#[derive(serde::Serialize)]
struct PeriodRow {
    key: String,
    label: String,
    start_ms: i64,
    end_ms: i64,
    sessions: u64,
    requests: u64,
    input: f64,
    cache_creation: f64,
    cache_read: f64,
    output: f64,
    total_tokens: f64,
    /// `None` means "no price known", never zero spend.
    cost: Option<f64>,
    cached_pct: f64,
    unpriced_models: Vec<String>,
}

pub fn periods(ctx: &Ctx, grain: Grain, requested: i64) -> Result<(), String> {
    let n = requested.clamp(1, MAX_PERIODS);
    let capped = requested != n;
    let mut spans = trailing_periods(ctx.now_ms, grain, n);
    if let Some(since) = ctx.since_ms {
        spans.retain(|p| p.end_ms >= since);
    }
    if spans.is_empty() {
        return Err(format!(
            "--since {} is after the last {} period",
            ctx.g.since.as_deref().unwrap_or("?"),
            grain.label()
        ));
    }

    let range_start = spans[0].start_ms;
    let events = ctx.events_since(range_start)?;
    let mut buckets: BTreeMap<String, Vec<UsageEvent>> = BTreeMap::new();
    for ev in &events {
        buckets.entry(period_of(ev.ts_ms, grain).key).or_default().push(ev.clone());
    }

    let total_span =
        Period { key: "total".into(), label: "total".into(), start_ms: range_start, end_ms: ctx.now_ms };
    let totals = ctx.summary_of(&events);
    let mut rows: Vec<PeriodRow> = Vec::new();
    for span in &spans {
        let empty = Vec::new();
        let slice = buckets.get(&span.key).unwrap_or(&empty);
        rows.push(period_row(ctx, span, &ctx.summary_of(slice)));
    }
    let total_row = period_row(ctx, &total_span, &totals);

    if ctx.g.json {
        #[derive(serde::Serialize)]
        struct Out {
            grain: &'static str,
            rows: Vec<PeriodRow>,
            totals: PeriodRow,
            pricing: usage_core::PricingMeta,
        }
        print_json(&Out {
            grain: grain.label(),
            rows,
            totals: total_row,
            pricing: ctx.pricing_meta().clone(),
        });
        return Ok(());
    }

    let head = render::capitalised(grain.label());
    let mut t = Table::new(&[head.as_str(),
        "sessions",
        "requests",
        "input",
        "cache read",
        "output",
        "total",
        "cost",
        "cached",
    ])
    .right(&[1, 2, 3, 4, 5, 6, 7, 8]);
    for r in &rows {
        t.push(vec![
            r.label.clone(),
            render::count(r.sessions),
            render::count(r.requests),
            render::tokens(r.input),
            render::tokens(r.cache_read),
            render::tokens(r.output),
            render::tokens(r.total_tokens),
            cost_cell(r.cost),
            render::pct(r.cached_pct),
        ]);
    }
    t.footer(vec![
        "total".into(),
        render::count(totals.sessions),
        render::count(totals.requests),
        render::tokens(totals.counts.input),
        render::tokens(totals.counts.cache_read),
        render::tokens(totals.counts.output),
        render::tokens(totals.total_tokens),
        cost_cell(row_cost(ctx, &totals)),
        render::pct(totals.cached_pct),
    ]);
    print!("{}", t.render(ctx.color));
    footnotes(ctx, &totals, &head_note(grain, n, requested, capped, &spans));
    Ok(())
}

fn head_note(
    grain: Grain,
    n: i64,
    requested: i64,
    capped: bool,
    spans: &[Period],
) -> String {
    let cap =
        if capped { format!(", requested {requested}, capped at {MAX_PERIODS}") } else { String::new() };
    let clipped = if spans.len() as i64 != n {
        format!(" ({} in --since/--until)", spans.len())
    } else {
        String::new()
    };
    format!(
        "last {} {}{} · {} → {}{}{}",
        n,
        grain.label(),
        if n == 1 { "" } else { "s" },
        spans.first().map(|p| p.label.clone()).unwrap_or_default(),
        spans.last().map(|p| p.label.clone()).unwrap_or_default(),
        cap,
        clipped,
    )
}

/// `None` whenever the price table cannot answer, so a JSON consumer sees the
/// difference between free and unknown.
fn row_cost(ctx: &Ctx, s: &Summary) -> Option<f64> {
    if !ctx.pricing_available() {
        return None;
    }
    Some(s.cost)
}

fn period_row(ctx: &Ctx, span: &Period, s: &Summary) -> PeriodRow {
    PeriodRow {
        key: span.key.clone(),
        label: span.label.clone(),
        start_ms: span.start_ms,
        end_ms: span.end_ms,
        sessions: s.sessions,
        requests: s.requests,
        input: s.counts.input,
        cache_creation: s.counts.cache_creation,
        cache_read: s.counts.cache_read,
        output: s.counts.output,
        total_tokens: s.total_tokens,
        cost: row_cost(ctx, s),
        cached_pct: s.cached_pct,
        unpriced_models: s.unpriced.iter().map(|u| u.model.clone()).collect(),
    }
}

fn footnotes(ctx: &Ctx, totals: &Summary, head: &str) {
    let mut notes = vec![head.to_string(), ctx.pricing_footer()];
    if !totals.unpriced.is_empty() {
        let top: Vec<String> = totals
            .unpriced
            .iter()
            .take(3)
            .map(|u| format!("{} {} tok", u.model, render::tokens(u.total_tokens)))
            .collect();
        notes.push(format!("no price for {} — tokens counted, cost excluded", top.join(", ")));
    }
    if totals.credits > 0.0 {
        notes.push(if totals.credit_cost > 0.0 {
            format!(
                "{} credits metered -> ~{} of the cost at the published plan price",
                render::count(totals.credits as u64),
                render::money(totals.credit_cost)
            )
        } else {
            format!("{} credits metered (no published price, not in cost)", render::count(totals.credits as u64))
        });
    }
    for note in notes {
        println!("{}", render::dim(&note, ctx.color));
    }
}

// ---------------------------------------------------------------- report

fn window_slice(r: &usage_core::Report, w: Win) -> &Window {
    match w {
        Win::Day => &r.day,
        Win::Week => &r.week,
        Win::Month => &r.month,
    }
}

fn group_slice(w: &Window, g: Group) -> (&str, &Vec<Item>) {
    match g {
        Group::Tool => ("Tool", &w.breakdown.tools),
        Group::Model => ("Model", &w.breakdown.models),
        Group::Project => ("Project", &w.breakdown.projects),
        Group::Mcp => ("MCP", &w.breakdown.mcps),
        Group::Skill => ("Skill", &w.breakdown.skills),
    }
}

pub fn report(ctx: &Ctx, win: Win, group: Group) -> Result<(), String> {
    // The full `Report` carries whole-history totals, so this command reads the
    // whole index rather than a window slice.
    let events = ctx.all_events()?;
    let opts = ctx.report_options(ctx.source_statuses(), 20);
    let report = summarize(&events, &opts);

    if ctx.g.json {
        print_json(&report);
        return Ok(());
    }

    let w = window_slice(&report, win);
    let (col, items) = group_slice(w, group);
    println!(
        "{} {}",
        render::bold(&format!("{} · {}", win_label(win), w.label), ctx.color),
        render::dim(&format!("({} elapsed)", human_span(w.end_ms - w.start_ms)), ctx.color)
    );

    let mut t = Table::new(&[col, "requests", "sessions", "tokens", "cost", "cached", "share"])
        .right(&[1, 2, 3, 4, 5, 6]);
    for item in items {
        let share = if w.summary.total_tokens > 0.0 {
            render::pct(item.total_tokens / w.summary.total_tokens * 100.0)
        } else {
            "—".into()
        };
        t.push(vec![
            render::truncate(&item.label, 34),
            render::count(item.requests),
            render::count(item.sessions),
            render::tokens(item.total_tokens),
            if item.priced { render::money(item.cost) } else { "no price".into() },
            render::pct(item.counts.cached_pct()),
            share,
        ]);
    }
    if t.is_empty() {
        println!("{}", render::dim("nothing indexed in this window", ctx.color));
    } else {
        t.footer(vec![
            "window total".into(),
            render::count(w.summary.requests),
            render::count(w.summary.sessions),
            render::tokens(w.summary.total_tokens),
            cost_cell(if ctx.pricing_available() { Some(w.summary.cost) } else { None }),
            render::pct(w.summary.cached_pct),
            String::new(),
        ]);
        print!("{}", t.render(ctx.color));
    }

    // `Window::prev` is the equally long slice right before the window, so the
    // comparison length is the window's own elapsed span.
    let prev_len = w.end_ms - w.start_ms;
    println!(
        "{}",
        render::dim(
            &format!(
                "vs the equal-elapsed {} before it: cost {} → {} ({}) · tokens {} → {} ({})",
                human_span(prev_len),
                if ctx.pricing_available() { render::money(w.prev.cost) } else { "no price".into() },
                if ctx.pricing_available() { render::money(w.summary.cost) } else { "no price".into() },
                render::signed_pct(w.delta_cost_pct, ctx.color),
                render::tokens(w.prev.total_tokens),
                render::tokens(w.summary.total_tokens),
                render::signed_pct(w.delta_tokens_pct, ctx.color),
            ),
            ctx.color
        )
    );
    footnotes(ctx, &w.summary, &format!("{} events in window", render::count(w.summary.requests)));
    if let Some(all) = report.heatmap.iter().rev().find(|c| c.requests > 0) {
        println!(
            "{}",
            render::dim(
                &format!("last active day: {} · {}", all.date, render::tokens(all.total_tokens)),
                ctx.color
            )
        );
    }
    Ok(())
}

fn win_label(w: Win) -> &'static str {
    match w {
        Win::Day => "Today",
        Win::Week => "This week",
        Win::Month => "This month",
    }
}

fn human_span(ms: i64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

// -------------------------------------------------------------- sessions

pub fn sessions(ctx: &Ctx, limit: usize) -> Result<(), String> {
    let lookback = ctx.now_ms - 90 * MS_DAY;
    let events = ctx.events_since(lookback)?;
    let opts = ctx.report_options(Vec::new(), limit.clamp(1, 200));
    let report = summarize(&events, &opts);
    if ctx.g.json {
        print_json(&report.recent_sessions);
        return Ok(());
    }
    let mut t = Table::new(&["last active", "tool", "session", "project", "model", "reqs", "tokens", "cost"])
        .right(&[5, 6, 7]);
    for s in &report.recent_sessions {
        t.push(vec![
            render::ago(ctx.now_ms - s.last_ms),
            s.tool.clone(),
            s.session.chars().take(12).collect(),
            render::truncate(&s.project.as_deref().map(render::shorten_path).unwrap_or_default(), 28),
            render::truncate(&s.model, 24),
            render::count(s.requests),
            render::tokens(s.total_tokens),
            cost_cell(if ctx.pricing_available() { Some(s.cost) } else { None }),
        ]);
    }
    if t.is_empty() {
        println!("{}", render::dim("no sessions indexed in the last 90 days", ctx.color));
        return Ok(());
    }
    print!("{}", t.render(ctx.color));
    footnotes(ctx, &report.all_time, &format!("{} sessions shown", report.recent_sessions.len()));
    Ok(())
}

// ----------------------------------------------------------------- quota

pub fn quota(ctx: &Ctx) -> Result<(), String> {
    let lookback = ctx.now_ms - 30 * MS_DAY;
    let events = ctx.events_since(lookback)?;
    let report = summarize(&events, &ctx.options());
    if ctx.g.json {
        print_json(&report.quotas);
        return Ok(());
    }
    if report.quotas.is_empty() {
        println!(
            "{}",
            render::dim(
                "no quota: Codex reports it in its own logs, Claude/OpenCode/Copilot only answer when probed (a signed-out or proxy-managed tool has no window)",
                ctx.color
            )
        );
        return Ok(());
    }
    let mut t = Table::new(&["tool", "used", "", "window", "resets", "sample", "via"]).right(&[2, 3, 4, 5]);
    // A window that already elapsed is not remaining quota; the panel hides those
    // too, so the two surfaces have to agree.
    let live: Vec<_> = report
        .quotas
        .iter()
        .filter(|q| q.resets_at_ms == 0 || q.resets_at_ms > ctx.now_ms)
        .collect();
    if live.is_empty() {
        println!(
            "{}",
            render::dim("no unexpired quota window (the last one has already reset)", ctx.color)
        );
        return Ok(());
    }
    for q in &live {
        let window = match &q.label {
            Some(label) if !label.is_empty() => label.clone(),
            _ => render::minutes_window(q.window_minutes),
        };
        let resets = if q.resets_at_ms > 0 { render::until(q.resets_at_ms - ctx.now_ms) } else { "-".into() };
        t.push(vec![
            q.tool.clone(),
            render::pct(q.used_percent),
            render::bar(q.used_percent, 12),
            window,
            resets,
            render::ago(ctx.now_ms - q.sampled_at_ms),
            match q.origin {
                usage_core::QuotaOrigin::Probe => "probe",
                usage_core::QuotaOrigin::Log => "log",
                usage_core::QuotaOrigin::Budget => "budget",
            }
            .to_string(),
        ]);
    }
    print!("{}", t.render(ctx.color));
    Ok(())
}

// --------------------------------------------------------------- pricing

/// One price listing, flattened so `--json` carries the four stages by name.
#[derive(Debug, Clone, serde::Serialize)]
struct Listing {
    provider: String,
    input: f64,
    output: f64,
    cache_creation: f64,
    cache_read: f64,
}

fn listing(provider: &str, p: usage_core::Price) -> Listing {
    Listing { provider: provider.to_string(), input: p.input, output: p.output, cache_creation: p.cache_creation, cache_read: p.cache_read }
}

/// `$/M` with only the digits that mean something: most contested listings sit
/// two decimals apart, and a few are fractions of a cent.
fn usd(v: f64) -> String {
    let digits = if v.abs() >= 1.0 { 2 } else if v.abs() >= 0.01 { 3 } else { 5 };
    let s = format!("{v:.digits$}");
    let (whole, frac) = s.split_once('.').unwrap_or((s.as_str(), ""));
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        format!("${whole}")
    } else {
        format!("${whole}.{frac}")
    }
}

/// A stage price, where a zero means "this stage is not billed" rather than
/// "free model", so it reads as a dash.
fn per_million(v: f64) -> String {
    if v == 0.0 { "—".into() } else { usd(v) }
}

fn price_line(p: usage_core::Price) -> String {
    format!(
        "in {} · out {} · cache write {} · cache read {}",
        per_million(p.input),
        per_million(p.output),
        per_million(p.cache_creation),
        per_million(p.cache_read),
    )
}

/// Which listing priced a model — the answer `explain` gives, shaped for JSON.
#[derive(Debug, Clone, serde::Serialize)]
struct PriceTrace {
    model: String,
    /// The listing the precedence rule chose; `None` means no vendor sells it.
    chosen: Option<Listing>,
    /// Everything it beat that charges differently, still in precedence order.
    /// Listings at an identical price are not in here — there is nothing to
    /// trace about a tie.
    beaten: Vec<Listing>,
    /// Where the pick sits among the others.
    range: Option<Range>,
    pricing: usage_core::PricingMeta,
}

/// The ends of the range among the listings the pick did **not** take, plus how
/// the pick itself sits in it. Computed over the alternatives only: including the
/// winner would let "cheapest" name the price already in use.
#[derive(Debug, Clone, serde::Serialize)]
struct Range {
    cheaper: usize,
    dearer: usize,
    cheapest: Option<Listing>,
    dearest: Option<Listing>,
}

fn range_of(winner: &Listing, beaten: &[Listing]) -> Range {
    let key = |l: &Listing| l.input + l.output + l.cache_creation + l.cache_read;
    // All four stages count: most of `grok-4.6`'s 23 listings agree on $2/$6 and
    // differ *only* in whether they record a cache-read price, so comparing input
    // and output alone would call those ties and hide the real spread.
    let w = key(winner);
    Range {
        cheaper: beaten.iter().filter(|l| key(l) < w).count(),
        dearer: beaten.iter().filter(|l| key(l) > w).count(),
        cheapest: beaten.iter().min_by(|a, b| key(a).total_cmp(&key(b))).cloned(),
        dearest: beaten.iter().max_by(|a, b| key(a).total_cmp(&key(b))).cloned(),
    }
}

fn listing_of(l: &Listing) -> String {
    format!(
        "{} · {}",
        l.provider,
        price_line(usage_core::Price { input: l.input, output: l.output, cache_creation: l.cache_creation, cache_read: l.cache_read })
    )
}

pub fn pricing_explain(
    map: &usage_core::PricingMap,
    models: &[String],
    json: bool,
    color: bool,
) -> Result<(), String> {
    let traces: Vec<PriceTrace> = models
        .iter()
        .map(|m| {
            let e = map.explain(m);
            let chosen = e.map(|x| listing(x.provider, x.price));
            let beaten: Vec<Listing> = e
                .map(|x| x.alternatives.iter().map(|(p, price)| listing(p, *price)).collect())
                .unwrap_or_default();
            let range = chosen.as_ref().filter(|_| !beaten.is_empty()).map(|c| range_of(c, &beaten));
            PriceTrace {
                model: m.clone(),
                chosen,
                beaten,
                range,
                pricing: map.meta().clone(),
            }
        })
        .collect();

    if json {
        print_json(&traces);
        return Ok(());
    }

    for t in &traces {
        println!("{}", render::bold(&t.model, color));
        let Some(chosen) = &t.chosen else {
            println!(
                "  {}",
                render::dim("no provider in the table sells this id — tokens counted, cost excluded", color)
            );
            continue;
        };
        let price = usage_core::Price {
            input: chosen.input,
            output: chosen.output,
            cache_creation: chosen.cache_creation,
            cache_read: chosen.cache_read,
        };
        println!("  priced by {:<18}{}", chosen.provider, price_line(price));
        let Some(range) = &t.range else {
            println!(
                "  {}",
                render::dim("listed by one provider only — this price was not a choice", color)
            );
            continue;
        };
        println!(
            "  {}",
            render::dim(
                &format!(
                    "{} other listing(s) charging differently: {} cheaper, {} dearer",
                    t.beaten.len(),
                    range.cheaper,
                    range.dearer
                ),
                color
            )
        );
        // Both ends need all four stages: most of a contested model's listings
        // agree on input/output and differ only in what they bill a cache read.
        if let Some(lo) = &range.cheapest {
            println!("  {}", render::dim(&format!("cheapest     {}", listing_of(lo)), color));
        }
        if let Some(hi) = &range.dearest {
            println!("  {}", render::dim(&format!("dearest      {}", listing_of(hi)), color));
        }
        println!(
            "  {}",
            render::dim(&format!("next in the precedence order: {}", listing_of(&t.beaten[0])), color)
        );
    }
    if let Some(first) = traces.first() {
        println!("{}", render::dim(&table_note(&first.pricing), color));
    }
    Ok(())
}

fn table_note(meta: &usage_core::PricingMeta) -> String {
    let src = match meta.source {
        usage_core::PricingSource::ModelsDev => "live from models.dev",
        usage_core::PricingSource::Cache => "cached models.dev snapshot",
        usage_core::PricingSource::Bundled => "bundled snapshot",
        usage_core::PricingSource::Unavailable => "UNAVAILABLE",
    };
    let age = if meta.fetched_at_ms == 0 {
        "never".to_string()
    } else {
        render::ago(usage_core::report::now_ms() - meta.fetched_at_ms)
    };
    format!(
        "price table: {src} · {} keys · fetched {age}{}",
        render::count(meta.key_count as u64),
        if meta.stale { " · >24h old" } else { "" }
    )
}

#[derive(Debug, Default)]
struct ModelAgg {
    counts: usage_core::TokenCounts,
    requests: u64,
    tools: BTreeMap<String, ()>,
}

/// A model whose money depends on which listing won.
#[derive(Debug, Clone, serde::Serialize)]
struct ContestedRow {
    model: String,
    tools: Vec<String>,
    requests: u64,
    total_tokens: f64,
    /// What the report charges for it.
    cost: f64,
    priced_by: String,
    /// Distinct prices on offer for this id **including** the pick. Not the
    /// number of listings: `grok-4.6` is sold by 23 providers on the wire but only
    /// four of them quote a different price, and the other nineteen cannot change
    /// this number.
    distinct_prices: usize,
    /// Difference if the next listing that charges differently had won. Listings
    /// at an identical price are not in this list at all, so their delta is 0.
    runner_up_delta: f64,
    /// Widest gap against any listing on offer.
    worst_delta: f64,
}

#[derive(serde::Serialize)]
struct Contested {
    rows: Vec<ContestedRow>,
    /// Rows hidden behind `--limit`; the totals below always cover all of them.
    omitted: usize,
    cost_in_rows: f64,
    max_runner_up_delta: f64,
    max_worst_delta: f64,
    contested_tokens_pct: f64,
    pricing: usage_core::PricingMeta,
}

/// Every model in the index that more than one provider sells, ranked by how
/// much the precedence rule could move the money. This is the error bar on
/// "cost": tokens are exact, a price is a pick between listings.
pub fn pricing_contested(ctx: &Ctx, limit: usize) -> Result<(), String> {
    let events = ctx.all_events()?;
    let map = ctx.pricing();

    let mut acc: BTreeMap<String, ModelAgg> = BTreeMap::new();
    let mut all_tokens = 0.0f64;
    for ev in &events {
        all_tokens += ev.counts.total();
        // Credit-metered sources are priced by a plan rate, not by this table,
        // so a listing spread says nothing about their money.
        if ev.meter == usage_core::Meter::Credits {
            continue;
        }
        let Some(model) = ev.model.as_deref() else { continue };
        let a = acc.entry(model.to_string()).or_default();
        a.counts += &ev.counts;
        a.requests += 1;
        a.tools.insert(ev.tool.clone(), ());
    }

    let mut rows: Vec<ContestedRow> = Vec::new();
    for (model, a) in &acc {
        let Some(e) = map.explain(model) else { continue };
        if e.alternatives.is_empty() {
            continue;
        }
        let winner = e.price.cost_of(&a.counts);
        let deltas: Vec<f64> = e.alternatives.iter().map(|(_, p)| p.cost_of(&a.counts) - winner).collect();
        let worst = deltas.iter().copied().fold(0.0f64, |m, d| if d.abs() > m.abs() { d } else { m });
        rows.push(ContestedRow {
            model: model.clone(),
            tools: a.tools.keys().cloned().collect(),
            requests: a.requests,
            total_tokens: a.counts.total(),
            cost: winner,
            priced_by: e.provider.to_string(),
            distinct_prices: deltas.len() + 1,
            runner_up_delta: deltas[0],
            worst_delta: worst,
        });
    }
    rows.sort_by(|x, y| {
        y.worst_delta
            .abs()
            .total_cmp(&x.worst_delta.abs())
            .then_with(|| y.cost.total_cmp(&x.cost))
    });

    let cost_in_rows: f64 = rows.iter().map(|r| r.cost).sum();
    let max_runner_up_delta: f64 = rows.iter().map(|r| r.runner_up_delta.abs()).sum();
    let max_worst_delta: f64 = rows.iter().map(|r| r.worst_delta.abs()).sum();
    let contested_tokens = rows.iter().map(|r| r.total_tokens).sum::<f64>();
    let contested_tokens_pct =
        if all_tokens > 0.0 { contested_tokens / all_tokens * 100.0 } else { 0.0 };

    if ctx.g.json {
        let omitted = rows.len().saturating_sub(limit);
        print_json(&Contested {
            rows,
            omitted,
            cost_in_rows,
            max_runner_up_delta,
            max_worst_delta,
            contested_tokens_pct,
            pricing: map.meta().clone(),
        });
        return Ok(());
    }

    let mut t = Table::new(&[
        "model",
        "tools",
        "requests",
        "tokens",
        "cost",
        "priced by",
        "prices",
        "Δ next",
        "Δ worst",
    ])
    .right(&[2, 3, 4, 6, 7, 8]);
    for r in rows.iter().take(limit.max(1)) {
        t.push(vec![
            render::truncate(&r.model, 30),
            r.tools.join(","),
            render::count(r.requests),
            render::tokens(r.total_tokens),
            render::money(r.cost),
            r.priced_by.clone(),
            render::count(r.distinct_prices as u64),
            signed_money(r.runner_up_delta),
            signed_money(r.worst_delta),
        ]);
    }
    if t.is_empty() {
        println!(
            "{}",
            render::dim(
                "nothing contested: every priced model in the index has exactly one listing",
                ctx.color
            )
        );
        return Ok(());
    }
    let omitted = rows.len().saturating_sub(limit.max(1));
    t.footer(vec![
        format!("{} contested model(s)", rows.len()),
        String::new(),
        String::new(),
        render::pct(contested_tokens_pct),
        render::money(cost_in_rows),
        String::new(),
        String::new(),
        format!("±{}", render::money(max_runner_up_delta)),
        format!("±{}", render::money(max_worst_delta)),
    ]);
    print!("{}", t.render(ctx.color));
    if omitted > 0 {
        println!("{}", render::dim(&format!("{omitted} more row(s) behind --limit; the totals cover all of them"), ctx.color));
    }
    // No probes here: this command is about prices, and `ctx.options()` would
    // spend up to 30 seconds polling quota nobody asked for.
    let total = summarize(&events, &usage_core::ReportOptions::new(map).with_now(ctx.now_ms)).all_time;
    println!(
        "{}",
        render::dim(
            &format!(
                "{} of the {} tokens in the index has more than one price on offer. If every pick went to the next-ranked listing the report would move by ±{}; across all listings the widest possible spread is ±{} (total cost {}, of which {} sits in the rows above), so a cost is exact only where Δ is 0.",
                render::pct(contested_tokens_pct),
                render::tokens(all_tokens),
                render::money(max_runner_up_delta),
                render::money(max_worst_delta),
                render::money(total.cost),
                render::money(cost_in_rows),
            ),
            ctx.color
        )
    );
    println!("{}", render::dim(&table_note(map.meta()), ctx.color));
    Ok(())
}

fn signed_money(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    // `usd`, not `money`: a row whose whole spread is four tenths of a cent is
    // still information, and `$0.00` would read as an exact tie.
    format!("{}{}", if v > 0.0 { "+" } else { "-" }, usd(v.abs()))
}

/// Which tool rows can show a real application icon. Presentation only — no
/// number depends on it — but the panel and a browser review both read this, so
/// the answer is stated rather than guessed at either end.
pub fn icons(json: bool) -> Result<(), String> {
    let map = usage_core::icons::icon_data_urls();
    if json {
        print_json(&map);
        return Ok(());
    }
    let mut t = Table::new(&["Tool", "Bundle tried", "Icon"]).right(&[2]);
    for (tool, names) in usage_core::icons::APP_BUNDLES {
        let (bundle, icon) = match (map.get(*tool), names.is_empty()) {
            (Some(url), true) => ("内置图标".into(), format!("{:.0} KB", decoded_kb(url))),
            (Some(url), false) => (names.join(", "), format!("{:.0} KB", decoded_kb(url))),
            (None, true) => ("—".into(), "命令行工具，没有 app 图标".into()),
            (None, false) => (names.join(", "), "本机未安装，或图标读不出 PNG".into()),
        };
        t.push(vec![tool.to_string(), bundle, icon]);
    }
    print!("{}", t.render(false));
    println!(
        "{}",
        render::dim("图标只影响外观：读不到就用彩色首字母，任何数字都不来自这里", false)
    );
    Ok(())
}

/// The icon's real byte count out of its `data:` URL: base64 is 4/3 of the
/// payload less the `=` padding, and rounding that down prints a 900 B badge as
/// "0 KB" next to a 20 KB one.
fn decoded_kb(data_url: &str) -> f64 {
    let b64 = data_url.split_once(',').map(|(_, rest)| rest).unwrap_or_default();
    let pad = b64.chars().rev().take_while(|c| *c == '=').count();
    let bytes = b64.len() / 4 * 3;
    (bytes - pad) as f64 / 1024.0
}

// ----------------------------------------------------------------- index

pub fn index(
    ctx: &mut Ctx,
    rebuild: bool,
    prune: bool,
    status: bool,
    force: bool,
) -> Result<(), String> {
    if force {
        ctx.force = true;
    }
    let mut last: Option<IngestReport> = None;
    if rebuild {
        let report = ctx
            .index
            .rebuild_with(&ctx.adapters, force)
            .map_err(|e| format!("rebuild failed: {e}"))?;
        for err in ctx.index.errors() {
            if !ctx.quiet() {
                eprintln!("tokenme: {err}");
            }
        }
        last = Some(report);
    } else if !status {
        last = ctx.ingest();
    }
    let mut pruned = 0u64;
    if prune {
        let cutoff = retention_cutoff(usage_core::report::now_ms());
        pruned = ctx.index.prune(cutoff).map_err(|e| format!("prune failed: {e}"))?;
        let _ = ctx.index.vacuum();
    }
    let per_tool = ctx.index.per_tool_counts().map_err(|e| e.to_string())?;
    let total = ctx.index.event_count().unwrap_or(0);
    let db_size = db_size(&ctx.db_path);

    if ctx.g.json {
        #[derive(serde::Serialize)]
        struct Status {
            db: PathBuf,
            bytes: u64,
            total_events: u64,
            files_tracked: usize,
            per_tool: BTreeMap<String, u64>,
            schema_version: Option<String>,
            last_ingest_ms: Option<i64>,
            pruned: u64,
            ingest: Option<IngestReport>,
            errors: Vec<String>,
        }
        print_json(&Status {
            db: ctx.db_path.clone(),
            bytes: db_size,
            total_events: total,
            files_tracked: ctx.index.files_tracked().unwrap_or(0),
            per_tool,
            schema_version: ctx.index.meta_value("schema_version").ok().flatten(),
            last_ingest_ms: ctx.index.meta_value("last_ingest_ms").ok().flatten().and_then(|s| s.parse().ok()),
            pruned,
            ingest: last.clone(),
            errors: ctx.index.errors().to_vec(),
        });
        return Ok(());
    }

    if let Some(r) = &last {
        print!(
            "{}",
            Table::new(&["files scanned", "files changed", "new events", "deduped", "purged", "total", "took"])
                .right(&[0, 1, 2, 3, 4, 5, 6])
                .row(vec![
                    render::count(r.files_scanned as u64),
                    render::count(r.files_changed as u64),
                    render::count(r.new_events),
                    render::count(r.deduped),
                    render::count(r.purged),
                    render::count(r.total_events),
                    format!("{}ms", r.took_ms),
                ])
                .render(ctx.color)
        );
        for (tool, n) in &r.per_tool {
            println!("  {tool}: {}", render::count(*n));
        }
    } else {
        print!(
            "{}",
            Table::new(&["total events", "files tracked", "index size"])
                .right(&[0, 1, 2])
                .row(vec![
                    render::count(total),
                    render::count(ctx.index.files_tracked().unwrap_or(0) as u64),
                    render::bytes(db_size),
                ])
                .render(ctx.color)
        );
        for (tool, n) in &per_tool {
            println!("  {tool}: {}", render::count(*n));
        }
    }
    if pruned > 0 {
        println!("pruned {pruned} event(s) older than {} days", usage_index::RETENTION_DAYS);
    }
    println!(
        "{}",
        render::dim(
            &format!(
                "db {} · schema {} · last ingest {}",
                render::shorten_path(&ctx.db_path.display().to_string()),
                ctx.index.meta_value("schema_version").ok().flatten().unwrap_or_default(),
                ctx.index
                    .meta_value("last_ingest_ms")
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(|ms| render::ago(ctx.now_ms - ms))
                    .unwrap_or_else(|| "never".into())
            ),
            ctx.color
        )
    );
    for err in ctx.index.errors() {
        eprintln!("tokenme: {err}");
    }
    Ok(())
}

fn db_size(path: &std::path::Path) -> u64 {
    [path.to_path_buf(), path.with_extension("db-wal"), path.with_extension("db-shm")]
        .into_iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

// ---------------------------------------------------------------- sync

fn fmt_day(ms: i64) -> String {
    local_date(ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "?".into())
}

/// The manifest window is `[lo, hi)`; showing the inclusive day reads better.
fn window_label(lo_ms: i64, hi_ms: i64) -> String {
    format!("{} → {}", fmt_day(lo_ms), fmt_day(hi_ms.saturating_sub(1)))
}

/// The write half of machine-to-machine sync: dump a window of this machine's
/// events as a JSONL.gz bundle next to its manifest. The default directory is
/// the one the menu-bar engine watches, so on a Linux collector the whole
/// writer is `tokenme export --days 30` from a systemd timer.
pub fn export_sync(
    ctx: &mut Ctx,
    days: i64,
    out: Option<PathBuf>,
    origin: Option<String>,
) -> Result<(), String> {
    // Unlike a report, an export must not refuse an empty window: a collector
    // between bursts (or fresh out of the box) still ships a valid bundle.
    // Ingest failures degrade like everywhere else — export the index as-is
    // and let the next timer tick catch up.
    let _ = ctx.ingest();
    let out_dir = match out {
        Some(dir) => dir,
        None => usage_index::default_sync_dir()
            .ok_or_else(|| "no home directory; pass --out <dir>".to_string())?,
    };
    let origin = origin
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
        .unwrap_or_else(usage_index::hostname);
    let rep = ctx
        .index
        .export_sync(&usage_index::ExportOptions { days, out_dir, origin })
        .map_err(|e| e.to_string())?;

    if ctx.g.json {
        print_json(&rep);
        return Ok(());
    }
    println!(
        "{}",
        Table::new(&["origin", "rows", "window", "took"])
            .right(&[1, 3])
            .row(vec![
                rep.origin.clone(),
                render::count(rep.rows),
                window_label(rep.window_lo_ms, rep.window_hi_ms),
                format!("{}ms", rep.took_ms),
            ])
            .render(ctx.color)
    );
    println!(
        "{}",
        render::dim(
            &format!(
                "bundle   {}\nmanifest {}\nsha256   {}",
                render::shorten_path(&rep.gz_path.display().to_string()),
                render::shorten_path(&rep.manifest_path.display().to_string()),
                rep.sha256
            ),
            ctx.color
        )
    );
    Ok(())
}

/// The read half: merge one bundle exported by another machine. Idempotent —
/// re-importing the same file changes nothing — and atomic: any failure (bad
/// hash, format mismatch, a batch that does not reconcile) rolls the whole
/// merge back, so the index is never half-merged.
pub fn import_sync(ctx: &mut Ctx, file: &std::path::Path, dry_run: bool) -> Result<(), String> {
    let rep = ctx
        .index
        .import_sync(&usage_index::ImportOptions { file: file.to_path_buf(), dry_run })
        .map_err(|e| e.to_string())?;
    if rep.self_import && !ctx.quiet() {
        eprintln!(
            "tokenme: bundle origin {:?} equals this machine's hostname — merging a copy of this \
             machine's own export; rows stay keyed as foreign",
            rep.origin
        );
    }
    if ctx.g.json {
        print_json(&rep);
        return Ok(());
    }
    println!(
        "{}",
        Table::new(&["origin", "rows", "new", "updated", "deduped", "calls+", "stale", "took"])
            .right(&[1, 2, 3, 4, 5, 6, 7])
            .row(vec![
                rep.origin.clone(),
                render::count(rep.rows),
                render::count(rep.inserted),
                render::count(rep.updated),
                render::count(rep.deduped),
                render::count(rep.calls_added),
                render::count(rep.stale_rows),
                format!("{}ms", rep.took_ms),
            ])
            .render(ctx.color)
    );
    println!(
        "{}",
        render::dim(
            &format!(
                "bundle   {}\nwindow   {}\nsha256   {}",
                render::shorten_path(&rep.file),
                window_label(rep.window_lo_ms, rep.window_hi_ms),
                rep.sha256
            ),
            ctx.color
        )
    );
    if rep.dry_run {
        println!("{}", render::dim("dry run — everything parsed, merged and reconciled; nothing was committed", ctx.color));
    }
    Ok(())
}

/// DSH accuracy audit: the projection ledger the desktop app keeps, printed
/// next to what the index actually holds, per session — plus the structural
/// facts that explain honest drift (inherited pre-v4 events, subagent spend
/// outside `tokenUsage`, a cumulative event that day-buckets onto its last
/// prompt day). A row that agrees reads `ok`; anything else names the gap.
pub fn dsh_doctor(ctx: &Ctx) -> Result<(), String> {
    let ledger = usage_adapter_dsh::ledger();
    let mut indexed: BTreeMap<String, (f64, f64, f64, f64, Option<String>, i64)> = BTreeMap::new();
    for e in ctx.index.all_events().map_err(|e| e.to_string())?.into_iter().filter(|e| e.tool == "dsh") {
        let slot = indexed.entry(e.session.clone()).or_insert((0.0, 0.0, 0.0, 0.0, None, 0));
        slot.0 += e.counts.input;
        slot.1 += e.counts.cache_creation;
        slot.2 += e.counts.cache_read;
        slot.3 += e.counts.output;
        if slot.4.is_none() {
            slot.4 = e.model.clone();
        }
        slot.5 += 1;
    }
    let mut table = Table::new(&[
        "session",
        "fmt",
        "ledger in/out/cr/cc",
        "index in/out/cr/cc",
        "events",
        "status",
    ]);
    let mut mismatches = 0usize;
    for row in &ledger {
        let slot = indexed.get(&row.session);
        let ledger_str = format!("{:.0}/{:.0}/{:.0}/{:.0}", row.input, row.output, row.cache_read, row.cache_write);
        let (index_str, events, status) = match slot {
            Some((i, cc, cr, o, _, n)) if row.format >= 4 => {
                let agree = (*i - row.input).abs() < 0.5
                    && (*o - row.output).abs() < 0.5
                    && (*cr - row.cache_read).abs() < 0.5
                    && (*cc - row.cache_write).abs() < 0.5;
                if agree {
                    (format!("{:.0}/{:.0}/{:.0}/{:.0}", i, o, cr, cc), *n, "ok".to_string())
                } else {
                    mismatches += 1;
                    (format!("{:.0}/{:.0}/{:.0}/{:.0}", i, o, cr, cc), *n, "MISMATCH".to_string())
                }
            }
            Some((i, cc, cr, o, _, n)) => {
                // A v3 stream carries per-call events; the ledger row is
                // identity-only, so agreement is "events exist".
                (format!("{:.0}/{:.0}/{:.0}/{:.0}", i, o, cr, cc), *n, "v3 stream".to_string())
            }
            None if row.input == 0.0 && row.output == 0.0 && row.cache_read == 0.0 && row.cache_write == 0.0 => {
                ("—".to_string(), 0, "ok · empty".to_string())
            }
            None => {
                mismatches += 1;
                ("—".to_string(), 0, "not indexed".to_string())
            }
        };
        let mut notes = Vec::new();
        if row.inherited_events > 0 {
            notes.push(format!("inherited {} pre-v4 events", row.inherited_events));
        }
        if row.subagents > 0 {
            notes.push(format!("{} subagents (spend outside tokenUsage)", row.subagents));
        }
        table.push(vec![
            row.session.chars().take(12).collect(),
            row.format.to_string(),
            ledger_str,
            index_str,
            events.to_string(),
            if notes.is_empty() { status } else { format!("{status} · {}", notes.join("; ")) },
        ]);
    }
    for (session, (i, cc, cr, o, _, n)) in &indexed {
        if !ledger.iter().any(|r| r.session == *session) {
            mismatches += 1;
            table.push(vec![
                session.chars().take(12).collect(),
                "?".into(),
                "—".into(),
                format!("{i:.0}/{o:.0}/{cr:.0}/{cc:.0}"),
                n.to_string(),
                "indexed but no projection".into(),
            ]);
        }
    }
    println!("DSH ledger: {} sessions · index: {} sessions · {} mismatched", ledger.len(), indexed.len(), mismatches);
    print!("{}", table.render(false));
    println!("notes: tokenUsage is the main thread's ledger — subagent and pre-v4 spend sit outside it; a");
    println!("cumulative session day-buckets onto its last prompt day, so day views move as sessions age.");
    Ok(())
}

/// Codex accuracy audit: every rollout replayed from scratch (cursor 0, no
/// manifest, no dedupe) and aggregated per session, printed next to what the
/// index holds. Codex has no vendor-side ledger — the rollouts are the only
/// record — so replay IS the independent truth: a divergence is a cursor or
/// dedupe failure in the indexer, and the row names which side moved.
pub fn codex_doctor(ctx: &Ctx) -> Result<(), String> {
    let replay = usage_adapter_codex::replay(&DateFilter::default());
    let mut indexed: BTreeMap<String, (f64, f64, f64, f64, usize)> = BTreeMap::new();
    for e in ctx.index.all_events().map_err(|e| e.to_string())?.into_iter().filter(|e| e.tool == "codex") {
        let slot = indexed.entry(e.session.clone()).or_insert((0.0, 0.0, 0.0, 0.0, 0));
        slot.0 += e.counts.input;
        slot.1 += e.counts.cache_creation;
        slot.2 += e.counts.cache_read;
        slot.3 += e.counts.output;
        slot.4 += 1;
    }
    let mut table = Table::new(&[
        "session",
        "replay in/out/cached/calls",
        "index in/out/cached/calls",
        "files",
        "status",
    ]);
    let mut mismatches = 0usize;
    let mut checked = 0usize;
    for row in &replay {
        let replay_str = format!(
            "{:.0}/{:.0}/{:.0}/{}",
            row.input, row.output, row.cached, row.calls
        );
        let (index_str, status) = match indexed.get(&row.session) {
            Some((i, cc, cr, o, n)) => {
                checked += 1;
                let agree = (*i - row.input).abs() < 0.5
                    && (*o - row.output).abs() < 0.5
                    && ((*cr + *cc) - row.cached).abs() < 0.5
                    && *n == row.calls;
                let index_str = format!("{i:.0}/{o:.0}/{:.0}/{n}", cr + cc);
                if agree { (index_str, "ok".to_string()) } else { mismatches += 1; (index_str, "MISMATCH".to_string()) }
            }
            None => {
                ("—".to_string(), "not indexed (retention or ingest gap)".to_string())
            }
        };
        table.push(vec![
            row.session.chars().take(14).collect(),
            replay_str,
            index_str,
            row.files.to_string(),
            status,
        ]);
    }
    for (session, (i, cc, cr, o, n)) in &indexed {
        if !replay.iter().any(|r| r.session == *session) {
            mismatches += 1;
            table.push(vec![
                session.chars().take(14).collect(),
                "—".into(),
                format!("{i:.0}/{o:.0}/{:.0}/{n}", cr + cc),
                "?".into(),
                "indexed but replay finds nothing".into(),
            ]);
        }
    }
    println!(
        "Codex replay: {} sessions · index: {} sessions · {} checked · {} mismatched",
        replay.len(),
        indexed.len(),
        checked,
        mismatches
    );
    print!("{}", table.render(false));
    println!("notes: the replay re-parses every rollout from scratch — the index side is what the panel");
    println!("shows; a MISMATCH means the indexer counted some call twice or dropped it.");
    Ok(())
}

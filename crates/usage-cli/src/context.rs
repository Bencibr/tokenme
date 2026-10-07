//! Shared setup for every subcommand: pricing, index, adapters, time window.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use usage_core::pricing::{
    parse_override, Price, PricingMap, PricingMeta, PricingOptions, PricingSource,
};
use usage_core::report::summarize;
use usage_core::{
    DateFilter, DetectedSource, ReportOptions, SourceAdapter, SourceStatus, Summary, UsageEvent,
};
use usage_index::{retention_cutoff, Index, IngestReport};

use crate::args::Global;
use crate::render;

/// Anything fatal the user must see; the message goes to stderr so `--json`
/// stdout stays machine-readable, and the process exits 1.
#[derive(Debug)]
pub struct Fail(pub String);

/// Commands return `Result<_, String>`, so the setup error converts straight in.
impl From<Fail> for String {
    fn from(f: Fail) -> String {
        f.0
    }
}

/// Adapter code is third-party-ish and may panic while it is being written. One
/// broken source must not take the report (or the process) down, so panics are
/// captured and re-surfaced as per-tool errors instead of a backtrace.
static PANICS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

pub fn install_panic_capture() {
    let _ = PANICS.set(Mutex::new(Vec::new()));
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| info.payload().downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| "panicked".into());
        let where_ = info
            .location()
            .map(|l| format!(" ({}:{})", l.file(), l.line()))
            .unwrap_or_default();
        if let Some(mut guard) = PANICS.get().and_then(|m| m.lock().ok()) {
            guard.push(format!("{msg}{where_}"));
        }
    }));
}

pub fn take_panics() -> Vec<String> {
    PANICS
        .get()
        .and_then(|m| m.lock().ok())
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

pub struct Ctx {
    pub g: Global,
    pub index: Index,
    pub db_path: PathBuf,
    pub adapters: Vec<Box<dyn SourceAdapter>>,
    pricing_opts: PricingOptions,
    /// Filled on first use: `detect` and `index` never need a price table, and
    /// loading one may hit the network.
    pricing: OnceLock<PricingMap>,
    /// Effective "now": `--until` moves it back so periods line up with the cut.
    pub now_ms: i64,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub color: bool,
    /// Steal the ingest lease instead of skipping the pass (`--force`).
    pub force: bool,
}

/// `--pricing-override` plus the cache/offline policy, resolved outside the
/// index: `pricing explain` needs a price table but no events at all.
pub fn pricing_options(g: &Global) -> Result<PricingOptions, Fail> {
    let mut overrides: HashMap<String, Price> = HashMap::new();
    for spec in &g.pricing_override {
        match parse_override(spec) {
            Some((key, price)) => {
                overrides.insert(key, price);
            }
            None => {
                return Err(Fail(format!(
                    "bad --pricing-override {spec:?} (want model=in/out/cache_creation/cache_read)"
                )))
            }
        }
    }
    Ok(PricingOptions { offline: g.offline, cache_dir: PricingOptions::default_cache_dir(), overrides })
}

pub fn build(g: &Global) -> Result<Ctx, Fail> {
    let pricing_opts = pricing_options(g)?;
    let ids: Vec<&str> = g.tool.iter().map(String::as_str).collect();
    for id in &ids {
        if !usage_adapter_all::TOOL_IDS.contains(id) {
            return Err(Fail(format!(
                "unknown --tool {id:?}; built-in sources are {}",
                usage_adapter_all::TOOL_IDS.join(", ")
            )));
        }
    }
    let adapters = usage_adapter_all::adapters_for(&ids);

    let db_path = match (g.db.clone(), Index::default_path()) {
        (Some(p), _) => p,
        (None, Some(p)) => p,
        (None, None) => {
            return Err(Fail("no platform data dir available; pass --db <path>".into()));
        }
    };
    let index = Index::open(&db_path)
        .map_err(|e| Fail(format!("cannot open index at {}: {e}", db_path.display())))?;
    // The whole-file guard stays on the CLI's path: this is what an operator
    // runs when the numbers look wrong, and nothing here is rendering a panel.
    // A damaged image is set aside and rebuilt empty, ready for the next ingest.
    let (index, healed) = index.verify_integrity().map_err(|e| Fail(format!(
        "index at {} is damaged and could not be replaced: {e}",
        db_path.display()
    )))?;
    if healed {
        eprintln!(
            "warning: the index image failed its integrity check; it was moved aside as \
             *.corrupt-<timestamp> and recreated empty — run `tokenme ingest` to refill it"
        );
    }

    let real_now = usage_core::report::now_ms();
    let since_ms = g.since.as_deref().map(|s| day_ms(s, false)).transpose()?;
    let until_ms = g.until.as_deref().map(|s| day_ms(s, true)).transpose()?;
    let now_ms = until_ms.unwrap_or(real_now).min(real_now);

    Ok(Ctx {
        g: g.clone(),
        index,
        db_path,
        adapters,
        pricing_opts,
        pricing: OnceLock::new(),
        now_ms,
        since_ms,
        until_ms,
        color: render::colour_wanted(),
        force: false,
    })
}

/// Local midnight (`end_of_day`: local 23:59:59.999), so `--until 2026-09-25`
/// includes that whole day.
fn day_ms(s: &str, end_of_day: bool) -> Result<i64, Fail> {
    use chrono::{NaiveDate, TimeZone};
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| Fail(format!("bad date {s:?} (want YYYY-MM-DD)")))?;
    let naive = if end_of_day { d.and_hms_milli_opt(23, 59, 59, 999) } else { d.and_hms_opt(0, 0, 0) };
    let local = naive
        .and_then(|t| chrono::Local.from_local_datetime(&t).single())
        .or_else(|| naive.map(|t| t.and_utc().with_timezone(&chrono::Local)))
        .ok_or_else(|| Fail(format!("date {s:?} is not representable in local time")))?;
    Ok(local.timestamp_millis())
}

impl Ctx {
    pub fn pricing(&self) -> &PricingMap {
        self.pricing.get_or_init(|| PricingMap::load(&self.pricing_opts))
    }

    pub fn quiet(&self) -> bool {
        self.g.quiet
    }

    /// Probes each adapter, tolerating a panicking one. Tries the registry
    /// first (that is what the menu-bar app uses) and only falls back to
    /// per-adapter probing when the registry panics or finds nothing, so the
    /// table can say *why* a tool is missing.
    pub fn detect(&self) -> (Vec<DetectedSource>, Vec<String>) {
        let mut problems = Vec::new();
        let registry = catch_unwind(AssertUnwindSafe(usage_adapter_all::detect_all));
        if let Ok(detected) = registry {
            if !detected.is_empty() {
                return (detected, problems);
            }
        }
        problems.extend(take_panics());

        let mut out = Vec::new();
        for adapter in &self.adapters {
            match catch_unwind(AssertUnwindSafe(|| adapter.probe())) {
                Ok(Some(src)) => out.push(src),
                Ok(None) => {}
                Err(_) => {
                    let detail = take_panics().into_iter().next().unwrap_or_default();
                    problems.push(format!("{}: probe failed {detail}", adapter.id()));
                }
            }
        }
        (out, problems)
    }

    pub fn source_statuses(&self) -> Vec<SourceStatus> {
        let (detected, _) = self.detect();
        self.index
            .source_statuses(&detected)
            .unwrap_or_else(|_| detected.iter().map(|d| SourceStatus {
                id: d.id.clone(),
                display: d.display.clone(),
                detected: true,
                roots: d.roots.clone(),
                hint: d.hint.clone(),
                events_ingested: 0,
            }).collect())
    }

    /// What every reporting command does first: ingest, then refuse to print an
    /// empty report as if the machine were quiet.
    pub fn prepare(&mut self) -> Result<(), String> {
        if self.g.no_ingest {
            return Ok(());
        }
        self.ingest();
        let (detected, _) = self.detect();
        if detected.is_empty() && self.index.event_count().unwrap_or(0) == 0 {
            return Err(format!(
                "no supported AI tool logs found (looked for {})",
                usage_adapter_all::TOOL_IDS.join(", ")
            ));
        }
        Ok(())
    }

    /// Runs the incremental pass. Failures degrade to "report from what is
    /// already indexed" rather than killing the command.
    pub fn ingest(&mut self) -> Option<IngestReport> {
        if self.g.no_ingest {
            return None;
        }
        // Always the retention window: a narrower filter would consume bytes for
        // records it never indexes, which is why `--since/--until` filter queries
        // instead of the ingest.
        let filter = DateFilter::new(Some(retention_cutoff(usage_core::report::now_ms())), None);
        let opts = usage_index::IngestOptions {
            force: self.force,
            claim_wait: std::time::Duration::from_millis(500),
        };
        let report = match self.index.ingest_with(&self.adapters, &filter, &opts) {
            Ok(r) => r,
            Err(e) => {
                if !self.quiet() {
                    eprintln!("tokenme: ingest failed ({e}); reporting from the existing index");
                }
                return None;
            }
        };
        for err in self.index.errors() {
            if !self.quiet() {
                eprintln!("tokenme: {err}");
            }
        }
        if self.g.verbose && !self.g.json && !self.quiet() {
            eprintln!(
                "tokenme: ingested {} new event(s) from {}/{} file(s) in {}ms",
                report.new_events, report.files_changed, report.files_scanned, report.took_ms
            );
        }
        Some(report)
    }

    /// Events to report on, clipped to `--since/--until/--tool`.
    pub fn events_since(&self, since_ms: i64) -> Result<Vec<UsageEvent>, Fail> {
        let floor = self.since_ms.map(|s| s.max(since_ms)).unwrap_or(since_ms);
        let events = self
            .index
            .events_since(floor)
            .map_err(|e| Fail(format!("cannot read index: {e}")))?;
        Ok(self.clip(events))
    }

    pub fn all_events(&self) -> Result<Vec<UsageEvent>, Fail> {
        let events = self.index.all_events().map_err(|e| Fail(format!("cannot read index: {e}")))?;
        Ok(self.clip(events))
    }

    /// `--tool` narrows the query as well as the ingest: reading a whole index
    /// and calling it "claude only" would be a lie.
    fn clip(&self, mut events: Vec<UsageEvent>) -> Vec<UsageEvent> {
        if !self.g.tool.is_empty() {
            events.retain(|e| self.g.tool.contains(&e.tool));
        }
        if let Some(since) = self.since_ms {
            events.retain(|e| e.ts_ms >= since);
        }
        if let Some(until) = self.until_ms {
            events.retain(|e| e.ts_ms <= until);
        }
        events
    }

    pub fn options(&self) -> ReportOptions<'_> {
        self.report_options(Vec::new(), 20)
    }

    pub fn report_options(
        &self,
        sources: Vec<SourceStatus>,
        session_limit: usize,
    ) -> ReportOptions<'_> {
        // A report cut off at a past date has no present to poll: today's live
        // window is not that day's, and its sample would also be stamped after the
        // instant the report describes.
        let polled_quota = if self.until_ms.is_some() {
            Vec::new()
        } else {
            let mut q = usage_core::report::poll_quota(&self.adapters);
            // A one-shot command may wait out the slowest probe; the menu-bar
            // app deliberately does not (see `usage_quota::collect_within`).
            q.extend(usage_quota::collect_within(std::time::Duration::from_secs(30)));
            q
        };
        ReportOptions {
            pricing: self.pricing(),
            now_ms: usage_core::report::instant_after_polling(
                self.until_ms.is_some(),
                self.now_ms,
                &polled_quota,
            ),
            sources,
            recent_session_limit: session_limit,
            // Two quota channels: what the adapters surfaced (Codex embeds its
            // window in every record) and what the live probes just answered.
            // The probes are appended last because `summarize` lets a later
            // sample win for the same (tool, window).
            polled_quota,
            budgets: usage_core::budget::load_budgets(),
            // The CLI keeps its historical behavior: whatever the index holds,
            // including rows merged from other machines' bundles (the panel
            // narrows the scope interactively; the CLI has no such switch).
            scope: usage_core::MachineScope::All,
        }
    }

    /// Aggregation for a slice: money and period math stay in `usage-core`, so
    /// the CLI never recomputes a price itself.
    pub fn summary_of(&self, events: &[UsageEvent]) -> Summary {
        summarize(events, &self.options()).all_time
    }

    pub fn pricing_available(&self) -> bool {
        self.pricing().meta().source != PricingSource::Unavailable
    }

    pub fn pricing_meta(&self) -> &PricingMeta {
        self.pricing().meta()
    }

    pub fn pricing_footer(&self) -> String {
        let meta = self.pricing().meta();
        let source = match meta.source {
            PricingSource::ModelsDev => "live from models.dev".to_string(),
            PricingSource::Cache => format!(
                "cached models.dev snapshot ({}), {} models",
                render::ago(self.now_ms.max(meta.fetched_at_ms) - meta.fetched_at_ms),
                render::count(meta.key_count as u64)
            ),
            PricingSource::Bundled => format!(
                "bundled snapshot (may be stale), {} models",
                render::count(meta.key_count as u64)
            ),
            PricingSource::Unavailable => "UNAVAILABLE — costs cannot be computed".to_string(),
        };
        let stale = if meta.stale && meta.source != PricingSource::Bundled {
            " · price data is >24h old"
        } else {
            ""
        };
        format!("prices: {source}{stale}")
    }
}

pub fn print_json<T: serde::Serialize>(value: &T) {
    let mut out = std::io::stdout().lock();
    if serde_json::to_writer(&mut out, value).is_err() {
        eprintln!("tokenme: failed to serialise the report");
        std::process::exit(1);
    }
    let _ = std::io::Write::write_all(&mut out, b"\n");
}

#[cfg(test)]
mod tests {
    use super::day_ms;

    #[test]
    fn dates_become_local_day_edges() {
        let start = day_ms("2026-09-23", false).unwrap();
        let end = day_ms("2026-09-23", true).unwrap();
        assert!(end > start);
        // A whole local day is 23, 24 or 25 hours depending on DST.
        let span = end - start;
        assert!((22..=26).contains(&(span / 3_600_000)), "{span} ms is not a day");
    }

    #[test]
    fn bad_dates_are_reported_not_panicked() {
        for bad in ["", "yesterday", "2026-13-01", "2026/09/03"] {
            assert!(day_ms(bad, false).is_err(), "{bad:?} should not parse");
        }
    }
}

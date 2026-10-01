//! The engine: one background thread owns the `Index` and the `PricingMap`.
//!
//! Nothing here runs on the main thread. Ingest is serialised by a `try_lock`
//! so a burst of watcher events can never stack two passes on top of each
//! other — the second one is simply skipped and the next tick catches up.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, RecvTimeoutError};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use usage_core::pricing::PricingOptions;
use usage_core::report::{now_ms, AggregatePlan, QuotaView, ReportOptions};
use usage_core::{DateFilter, DetectedSource, PricingMap, Report, SourceAdapter, SourceFile, SourceStatus};
use usage_index::{Index, IngestOptions, Watcher, RETENTION_DAYS};

use crate::settings::Settings;
use crate::tray;

/// Emitted on every recomputed report; the payload is a serialised `Report`.
pub const REPORT_EVENT: &str = "report-updated";
/// Emitted when a tray menu entry asks the panel to focus a period.
pub const PERIOD_EVENT: &str = "tray-period";

/// The debounce window that absorbs a burst of write events as one re-index.
/// The idle cadence itself is the persisted `refresh_secs` setting.
const DEBOUNCE: Duration = Duration::from_millis(800);

/// Floor between ingest passes. A full pass re-walks every adapter's tree
/// (codex alone holds hundreds of rollouts inside the retention window), so a
/// writer that touches its store every second — ZCode's own telemetry db —
/// would otherwise keep the engine at a permanent percent-level duty cycle.
/// Scoped passes only restat the previous snapshot, but the floor keeps even
/// those from spinning. A finished call surfaces at most this much later; the
/// manual refresh bypasses the floor because the user asked for "now".
const MIN_PASS_GAP: Duration = Duration::from_secs(3);

/// Minimum gap between report rebuilds on file-change wakes. A rebuild reads
/// the day rollup plus a few live slices, so it is cheap — but every one still
/// emits a report and repaints the panel, so drip writers (one new event per
/// second) get at most one per gap. Ingestion itself stays per-wake and cheap;
/// the cadence timer and the manual refresh publish regardless, so quiet
/// machines still see the time-shaped windows move.
const PUBLISH_GAP: Duration = Duration::from_secs(10);

/// How long one full quota pass (vendor probes + the host-process scan) may be
/// reused. The pass costs HTTPS/CLI round trips per window plus a spawned
/// process scan; file-change wakes land several times a minute on a working
/// machine, and re-running the pass on every publish made the engine the
/// machine's top CPU taxpayer. Vendor answers themselves barely move in a
/// minute; the built-in probes' own cache is five minutes.
const QUOTA_PASS_TTL: Duration = Duration::from_secs(60);
static QUOTA_PASS: Mutex<Option<(Instant, Vec<QuotaView>)>> = Mutex::new(None);
/// Set by the manual refresh: the button promises fresh vendor numbers, so the
/// next pass re-probes for real regardless of the TTL.
static QUOTA_PASS_BUST: AtomicBool = AtomicBool::new(false);

/// How long one discovered-file snapshot is reused (restatted, not re-walked)
/// before the engine pays a full `discover` again. Bounds how long a file the
/// watcher never saw can stay invisible. Any wake naming a declared root
/// forces a full walk for every tool, ahead of the TTL.
const SNAPSHOT_TTL: Duration = Duration::from_secs(30);

pub enum Msg {
    /// A watched root changed; the payload is the root the watcher named.
    Wake(PathBuf),
    /// The user asked for an immediate re-index.
    Refresh,
    /// Swap in a freshly downloaded price table.
    Pricing(PricingMap),
}

/// Cloned into managed state so commands can poke the engine thread.
pub struct EngineChannel(pub Sender<Msg>);

/// Shared between the engine thread, the commands and the tray.
pub struct Shared {
    pub report: Mutex<Option<Report>>,
    pub settings: Mutex<Settings>,
    ingesting: Mutex<()>,
}

impl Shared {
    pub fn new(settings: Settings) -> Self {
        Self {
            report: Mutex::new(None),
            settings: Mutex::new(settings),
            ingesting: Mutex::new(()),
        }
    }

    /// Updates the tray mode in memory only; the caller decides when the
    /// settings file is written, so a slow save never delays the switch.
    pub fn set_tray_mode(&self, mode: crate::settings::TrayMode) -> Result<(), String> {
        let mut settings = self.settings.lock().map_err(|_| "settings busy".to_string())?;
        settings.tray_mode = mode;
        Ok(())
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn report(&self) -> Option<Report> {
        self.report.lock().ok().and_then(|r| r.clone())
    }
}

/// Runs the engine on its own thread; `tx` is the same sender commands hold so
/// the file-watcher wake-ups can be merged into one queue.
pub fn start(app: AppHandle, rx: Receiver<Msg>, tx: Sender<Msg>) {
    // An engine panic must not take the whole panel down: catch it, log it,
    // and let the thread end — the tray and the last report stay alive, the
    // log says exactly which unwind killed the updates.
    std::thread::Builder::new()
        .name("tokenme-engine".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(app, rx, tx)
            }));
            if let Err(panic) = result {
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".into());
                crate::logging::error(&format!("engine thread panicked: {msg} — updates stop until relaunch"));
            }
        })
        .expect("failed to spawn the tokenme engine thread");
}

fn run(app: AppHandle, rx: Receiver<Msg>, tx: Sender<Msg>) {
    let adapters: Vec<Box<dyn SourceAdapter>> = usage_adapter_all::builtin_adapters();
    let detected: Vec<DetectedSource> = usage_adapter_all::detect_all();
    let mut pricing = PricingMap::load(&PricingOptions {
        offline: false,
        cache_dir: PricingOptions::default_cache_dir(),
        overrides: Default::default(),
    });

    let mut index = Index::open(index_path()).ok();

    let roots: Vec<PathBuf> = detected.iter().flat_map(|d| d.roots.clone()).collect();
    let (wake_tx, wake_rx) = mpsc::channel();
    // The watcher only lives as long as this binding; dropping it stops events.
    let _watcher: Option<Watcher> = Watcher::spawn(&roots, wake_tx).ok();
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("tokenme-wake".into())
            .spawn(move || {
                while let Ok(root) = wake_rx.recv() {
                    if tx.send(Msg::Wake(root)).is_err() {
                        break;
                    }
                }
            })
            .ok();
    }

    // The index persists across launches, so last-known numbers are already on
    // disk: publish them before the scan touches anything and the panel renders
    // instantly — the boot screen only shows when there is genuinely no index
    // yet (fresh install). The quota probes are skipped here on purpose; they
    // land with the post-scan publish a moment later.
    if let Some(index) = index.as_mut() {
        if index.event_count().unwrap_or(0) > 0 {
            let sources =
                index.source_statuses(&detected).unwrap_or_else(|_| fallback_sources(&detected));
            publish_with_quota(&app, index, &sources, &pricing, &adapters, false);
            crate::logging::info("boot: published last-known report before the scan");
        }
    }

    // The last discovered file list per tool. Passes that are neither
    // privileged nor TTL-expired restat this snapshot instead of walking every
    // adapter tree again — the discover scoping that keeps idle passes down to
    // a couple of stats per file.
    let mut snapshot: HashMap<String, Vec<SourceFile>> = HashMap::new();
    let mut snapshot_at = Instant::now();
    let mut wake_roots: Vec<PathBuf> = Vec::new();

    let mut last_publish = Instant::now();
    ingest(&app, &mut index, &adapters, &detected, &pricing, "initial scan", &mut last_publish,
        &mut snapshot, &mut snapshot_at, &mut wake_roots);
    let mut last_pass = Instant::now();

    loop {
        // Read per pass, so a cadence change from the panel applies on the next
        // wait without restarting the engine.
        let fallback = {
            let secs = app.state::<Shared>().settings().refresh_secs;
            Duration::from_secs(secs.clamp(10, 3600))
        };
        match wait_for_work(&rx, &mut pricing, fallback, &mut wake_roots) {
            Work::Ingest(reason) => {
                if reason != "manual refresh" {
                    let since = last_pass.elapsed();
                    if since < MIN_PASS_GAP {
                        std::thread::sleep(MIN_PASS_GAP - since);
                    }
                }
                last_pass = Instant::now();
                ingest(&app, &mut index, &adapters, &detected, &pricing, reason, &mut last_publish,
                    &mut snapshot, &mut snapshot_at, &mut wake_roots);
            }
            Work::Resummarize => {
                crate::logging::info("pricing refreshed — re-summarizing indexed events");
                resummarize(&app, &mut index, &adapters, &detected, &pricing)
            }
            Work::Quit => return,
        }
    }
}

enum Work {
    Ingest(&'static str),
    Resummarize,
    Quit,
}

/// Idle until something happens, then debounce a burst of wake-ups into one pass.
fn wait_for_work(
    rx: &Receiver<Msg>,
    pricing: &mut PricingMap,
    fallback: Duration,
    wake_roots: &mut Vec<PathBuf>,
) -> Work {
    loop {
        match rx.recv_timeout(fallback) {
            Ok(Msg::Pricing(map)) => {
                *pricing = map;
                return Work::Resummarize;
            }
            Ok(Msg::Wake(root)) => {
                wake_roots.push(root);
                if !drain_debounce(rx, pricing, wake_roots) {
                    return Work::Quit;
                }
                return Work::Ingest("file change");
            }
            Ok(Msg::Refresh) => {
                // A manual refresh is "everything, now": busting the quota pass
                // makes the next publish re-probe the vendors for real — the
                // idle cadence keeps reusing the cached pass.
                QUOTA_PASS_BUST.store(true, Ordering::Relaxed);
                return Work::Ingest("manual refresh");
            }
            Err(RecvTimeoutError::Timeout) => return Work::Ingest("cadence timer"),
            Err(RecvTimeoutError::Disconnected) => return Work::Quit,
        }
    }
}

/// Keeps pushing the deadline out while events keep arriving; returns false if
/// a pricing swap landed mid-debounce and needs a re-summarize instead.
fn drain_debounce(rx: &Receiver<Msg>, pricing: &mut PricingMap, wake_roots: &mut Vec<PathBuf>) -> bool {
    let mut deadline = Instant::now() + DEBOUNCE;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        match rx.recv_timeout(remaining) {
            Ok(Msg::Wake(root)) => {
                wake_roots.push(root);
                deadline = Instant::now() + DEBOUNCE;
            }
            Ok(Msg::Pricing(map)) => {
                *pricing = map;
                return false;
            }
            Ok(Msg::Refresh) => {
                usage_quota::clear_cache();
                return true;
            }
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn ingest(
    app: &AppHandle,
    index: &mut Option<Index>,
    adapters: &[Box<dyn SourceAdapter>],
    detected: &[DetectedSource],
    pricing: &PricingMap,
    reason: &'static str,
    last_publish: &mut Instant,
    snapshot: &mut HashMap<String, Vec<SourceFile>>,
    snapshot_at: &mut Instant,
    wake_roots: &mut Vec<PathBuf>,
) {
    let shared = app.state::<Shared>();
    // A second ingest while one is running adds nothing but latency.
    let Ok(_guard) = shared.ingesting.try_lock() else { return };
    let Some(index) = index.as_mut() else { return };

    let cutoff = cutoff_ms();
    let filter = DateFilter::new(Some(cutoff), None);
    // Discover scoping: privileged passes and expired snapshots walk every
    // tree; a file-change wake walks only the tool that owns the woken root;
    // every other tool restats its previous list — a couple of stats per file,
    // no walk. A file the watcher missed surfaces within one SNAPSHOT_TTL.
    let full = matches!(reason, "initial scan" | "manual refresh" | "cadence timer")
        || snapshot_at.elapsed() >= SNAPSHOT_TTL
        || snapshot.is_empty();
    let woken: HashSet<&str> = detected
        .iter()
        .filter(|d| wake_roots.iter().any(|r| d.roots.contains(r)))
        .map(|d| d.id.as_str())
        .collect();
    let prepared: HashMap<String, Vec<SourceFile>> = adapters
        .iter()
        .map(|a| {
            let walk = full || woken.contains(a.id());
            let prev =
                if walk { None } else { Some(snapshot.get(a.id()).cloned().unwrap_or_default()) };
            (a.id().to_string(), a.discover_cached(&filter, prev))
        })
        .collect();
    *snapshot = prepared.clone();
    *snapshot_at = Instant::now();
    wake_roots.clear();
    let hints = |id: &str| prepared.get(id).cloned();
    // A failed pass keeps the previous report on screen rather than blanking it.
    let report = match index.ingest_with_hints(adapters, &filter, &IngestOptions::default(), &hints)
    {
        Ok(r) => {
            // An idle file-change pass (nothing moved) stays out of panel.log.
            if r.files_changed > 0 || r.new_events > 0 || reason != "file change" {
                crate::logging::info(&format!(
                    "ingest({reason}): scanned {} changed {} new {} deduped {} purged {} — {} ms",
                    r.files_scanned, r.files_changed, r.new_events, r.deduped, r.purged, r.took_ms
                ));
            }
            if r.new_events > 0 {
                let per_tool: Vec<String> = r.per_tool.iter().map(|(t, n)| format!("{t}:{n}")).collect();
                crate::logging::info(&format!("ingest indexed per tool: {}", per_tool.join(", ")));
            }
            Some(r)
        }
        Err(e) => {
            crate::logging::error(&format!("ingest({reason}) failed: {e}"));
            None
        }
    };
    let _ = index.prune(cutoff);
    // Planner-statistics refresh only on the idle pass: it reads enough of the
    // index that per-wake runs were measurable overhead for no benefit.
    if reason == "cadence timer" {
        index.optimize();
    }
    // A file wake that added nothing new would republish an identical report —
    // on a working machine that is most wakes (editors touch files constantly).
    // A wake that DID add events recomputes the entire report (all indexed
    // history, priced) and that costs hundreds of milliseconds, so drip writers
    // get at most one rebuild per PUBLISH_GAP. The cadence timer and manual
    // refresh always publish, keeping the time-shaped numbers moving.
    if reason == "file change" {
        if report.as_ref().is_some_and(|r| r.new_events == 0) {
            return;
        }
        if last_publish.elapsed() < PUBLISH_GAP {
            return;
        }
    }
    *last_publish = Instant::now();
    let sources = index.source_statuses(detected).unwrap_or_else(|_| fallback_sources(detected));
    if report.is_none() {
        crate::logging::error(&format!("ingest({reason}) failed pass — previous report stays on screen"));
    }
    publish(app, index, &sources, pricing, adapters);
    // The audit trail runs AFTER the publish, never in front of it: the hourly
    // heartbeat re-parses every codex rollout (tens of seconds on a heavy
    // machine) and the panel must not sit on the boot screen for diagnostics.
    if let Some(r) = &report {
        crate::scan_log::pass(reason, detected, r, index);
        // The DSH paper trail: when the ledger moved, one line saying what the
        // index now holds. (The per-session audit lives in scan.log — this used
        // to re-log every historical row on each append, flooding panel.log.)
        if r.new_events > 0 && r.per_tool.iter().any(|(t, _)| t == "dsh") {
            let dsh_count = r.per_tool.get("dsh").copied().unwrap_or(0);
            let newest = index
                .newest_ts_ms("dsh")
                .ok()
                .flatten()
                .and_then(|ts| chrono::DateTime::from_timestamp_millis(ts))
                .map(|d| d.format("%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "none".into());
            crate::logging::info(&format!("dsh ledger: {dsh_count} events indexed, newest at {newest}"));
        }
    }
}

/// Re-runs the aggregation over already-indexed events after a price refresh.
fn resummarize(
    app: &AppHandle,
    index: &mut Option<Index>,
    adapters: &[Box<dyn SourceAdapter>],
    detected: &[DetectedSource],
    pricing: &PricingMap,
) {
    let Some(index) = index.as_mut() else { return };
    let sources = index
        .source_statuses(detected)
        .unwrap_or_else(|_| fallback_sources(detected));
    publish(app, index, &sources, pricing, adapters);
}


fn publish(
    app: &AppHandle,
    index: &mut Index,
    sources: &[SourceStatus],
    pricing: &PricingMap,
    adapters: &[Box<dyn SourceAdapter>],
) {
    publish_with_quota(app, index, sources, pricing, adapters, true)
}

/// `poll_quota = false` publishes without waiting on the vendor probes — the
/// boot path uses it to put last-known numbers on screen instantly; the probes
/// land with the post-scan publish a moment later.
fn publish_with_quota(
    app: &AppHandle,
    index: &mut Index,
    sources: &[SourceStatus],
    pricing: &PricingMap,
    adapters: &[Box<dyn SourceAdapter>],
    poll_quota: bool,
) {
    // Live quota (a keychain read, a vendor CLI, or an HTTPS call) is merged in
    // here. The pass is reused for QUOTA_PASS_TTL — probing every publish made
    // a busy file-watcher cadence burn 30% CPU for numbers that had not moved.
    let polled_quota = if poll_quota { quota_pass(app, adapters) } else { Vec::new() };
    let captured = now_ms();
    let opts = ReportOptions {
        pricing,
        // After the probes, not before them: a vendor CLI that answers late must
        // not leave the report stamped older than a window it is showing.
        now_ms: usage_core::report::instant_after_polling(false, captured, &polled_quota),
        sources: sources.to_vec(),
        recent_session_limit: 12,
        polled_quota,
        budgets: app.state::<Shared>().settings().budgets,
    };
    // Every period's numbers fold from the day rollup (~2k rows) plus the
    // plan's live slices over today — never from re-reading all events.
    let plan = AggregatePlan::build(opts.now_ms, opts.recent_session_limit);
    let facts = match index.report_facts(&plan) {
        Ok(facts) => facts,
        Err(e) => {
            crate::logging::error(&format!(
                "publish: facts query failed — previous report stays on screen: {e}"
            ));
            return;
        }
    };
    if facts.rebuilt {
        crate::logging::info(
            "publish: rebuilt the day rollup from the event table (first publish after an \
             upgrade, or after a fail-safe invalidation)",
        );
    }
    let mut report = usage_core::report::summarize_facts(&facts, &opts);
    // The boot publish deliberately skips the probes: until the post-scan
    // publish lands, the quota strip shows "探测中" instead of vanishing.
    report.quotas_pending = !poll_quota;
    if let Ok(mut slot) = app.state::<Shared>().report.lock() {
        *slot = Some(report.clone());
    }
    let _ = app.emit(REPORT_EVENT, &report);
    // Re-read the mode AFTER the slow quota probes: a switch made while
    // polling ran must not be painted back to the old display (the stale-read
    // race behind "切换有很大的延迟").
    let mode = app.state::<Shared>().settings().tray_mode;
    tray::refresh(app, &report, mode);
}

/// One full quota pass: the per-window adapter probes plus the budget views
/// (host-exit-paused or not), reused for [`QUOTA_PASS_TTL`].
fn quota_pass(app: &AppHandle, adapters: &[Box<dyn SourceAdapter>]) -> Vec<QuotaView> {
    if QUOTA_PASS_BUST.swap(false, Ordering::Relaxed) {
        usage_quota::clear_cache();
        if let Ok(mut slot) = QUOTA_PASS.lock() {
            *slot = None;
        }
    }
    if let Ok(slot) = QUOTA_PASS.lock() {
        if let Some((at, q)) = slot.as_ref() {
            if at.elapsed() < QUOTA_PASS_TTL {
                return q.clone();
            }
        }
    }
    let poll_started = Instant::now();
    let mut q = usage_core::report::poll_quota(adapters);
    // The host-exit pause: a tool whose application has exited stops getting
    // vendor probes (the number cannot change) but keeps its last known answer
    // on screen. Unmapped tools always keep probing.
    if app.state::<Shared>().settings().host_exit_pause {
        let alive = usage_quota::host::running_process_names();
        q.extend(usage_quota::collect_gated(usage_quota::BUDGET, move |tool| {
            usage_quota::host::any_host_running(tool, &alive)
        }));
    } else {
        q.extend(usage_quota::collect());
    }
    let per_tool: Vec<String> = q
        .iter()
        .map(|sample| format!("{}:{}%", sample.tool, format_args!("{:.1}", sample.used_percent)))
        .collect();
    crate::logging::info(&format!(
        "quota poll: {} windows in {} ms [{}]",
        q.len(),
        poll_started.elapsed().as_millis(),
        per_tool.join(", ")
    ));
    if let Ok(mut slot) = QUOTA_PASS.lock() {
        *slot = Some((Instant::now(), q.clone()));
    }
    q
}

fn fallback_sources(detected: &[DetectedSource]) -> Vec<SourceStatus> {
    detected
        .iter()
        .map(|d| SourceStatus {
            id: d.id.clone(),
            display: d.display.clone(),
            detected: true,
            roots: d.roots.clone(),
            hint: d.hint.clone(),
            events_ingested: 0,
        })
        .collect()
}

/// `Index::default_path()` wins; a missing platform dir falls back to cache.
fn index_path() -> PathBuf {
    Index::default_path().unwrap_or_else(|| {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tokenme")
            .join("usage.sqlite")
    })
}

fn cutoff_ms() -> i64 {
    now_ms() - RETENTION_DAYS * 86_400_000
}

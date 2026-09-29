//! The engine: one background thread owns the `Index` and the `PricingMap`.
//!
//! Nothing here runs on the main thread. Ingest is serialised by a `try_lock`
//! so a burst of watcher events can never stack two passes on top of each
//! other — the second one is simply skipped and the next tick catches up.

use std::sync::mpsc::{self, Receiver, Sender, RecvTimeoutError};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use usage_core::pricing::PricingOptions;
use usage_core::report::{now_ms, ReportOptions};
use usage_core::{DateFilter, DetectedSource, PricingMap, Report, SourceAdapter, SourceStatus, UsageEvent};
use usage_index::{Index, Watcher, RETENTION_DAYS};

use crate::settings::Settings;
use crate::tray;

/// Emitted on every recomputed report; the payload is a serialised `Report`.
pub const REPORT_EVENT: &str = "report-updated";
/// Emitted when a tray menu entry asks the panel to focus a period.
pub const PERIOD_EVENT: &str = "tray-period";

/// The debounce window that absorbs a burst of write events as one re-index.
/// The idle cadence itself is the persisted `refresh_secs` setting.
const DEBOUNCE: Duration = Duration::from_millis(800);

pub enum Msg {
    /// A watched root changed.
    Wake,
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
                while wake_rx.recv().is_ok() {
                    if tx.send(Msg::Wake).is_err() {
                        break;
                    }
                }
            })
            .ok();
    }

    ingest(&app, &mut index, &adapters, &detected, &pricing, "initial scan");

    loop {
        // Read per pass, so a cadence change from the panel applies on the next
        // wait without restarting the engine.
        let fallback = {
            let secs = app.state::<Shared>().settings().refresh_secs;
            Duration::from_secs(secs.clamp(10, 3600))
        };
        match wait_for_work(&rx, &mut pricing, fallback) {
            Work::Ingest(reason) => ingest(&app, &mut index, &adapters, &detected, &pricing, reason),
            Work::Resummarize => {
                crate::logging::info("pricing refreshed — re-summarizing indexed events");
                resummarize(&app, &index, &adapters, &detected, &pricing)
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
fn wait_for_work(rx: &Receiver<Msg>, pricing: &mut PricingMap, fallback: Duration) -> Work {
    loop {
        match rx.recv_timeout(fallback) {
            Ok(Msg::Pricing(map)) => {
                *pricing = map;
                return Work::Resummarize;
            }
            Ok(Msg::Wake) => {
                if !drain_debounce(rx, pricing) {
                    return Work::Quit;
                }
                return Work::Ingest("file change");
            }
            Ok(Msg::Refresh) => {
                // A manual refresh is "everything, now": dropping the quota
                // cache makes this pass re-probe the vendors for real — the
                // idle cadence keeps reading their 5-minute TTL answers.
                usage_quota::clear_cache();
                return Work::Ingest("manual refresh");
            }
            Err(RecvTimeoutError::Timeout) => return Work::Ingest("cadence timer"),
            Err(RecvTimeoutError::Disconnected) => return Work::Quit,
        }
    }
}

/// Keeps pushing the deadline out while events keep arriving; returns false if
/// a pricing swap landed mid-debounce and needs a re-summarize instead.
fn drain_debounce(rx: &Receiver<Msg>, pricing: &mut PricingMap) -> bool {
    let mut deadline = Instant::now() + DEBOUNCE;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        match rx.recv_timeout(remaining) {
            Ok(Msg::Wake) => deadline = Instant::now() + DEBOUNCE,
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
) {
    let shared = app.state::<Shared>();
    // A second ingest while one is running adds nothing but latency.
    let Ok(_guard) = shared.ingesting.try_lock() else { return };
    let Some(index) = index.as_mut() else { return };

    let cutoff = cutoff_ms();
    let filter = DateFilter::new(Some(cutoff), None);
    // A failed pass keeps the previous report on screen rather than blanking it.
    let report = match index.ingest(adapters, &filter) {
        Ok(r) => {
            crate::logging::info(&format!(
                "ingest({reason}): scanned {} changed {} new {} deduped {} purged {} — {} ms",
                r.files_scanned, r.files_changed, r.new_events, r.deduped, r.purged, r.took_ms
            ));
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
    let events = index.all_events().unwrap_or_default();
    let sources = index.source_statuses(detected).unwrap_or_else(|_| fallback_sources(detected));
    if report.is_none() {
        crate::logging::error(&format!("ingest({reason}) failed pass — previous report stays on screen"));
    }
    // When the DSH ledger moved, log what actually landed: the projection is
    // cumulative and replaced on one key, so a misread surfaces later as a
    // wrong day-bucket or a missing session — this line is the paper trail.
    if let Some(r) = &report {
        if r.new_events > 0 && r.per_tool.iter().any(|(t, _)| t == "dsh") {
            for e in events.iter().filter(|e| e.tool == "dsh") {
                crate::logging::info(&format!(
                    "dsh ledger: {} in={} cc={} cr={} out={} model={:?} at={}",
                    &e.session[..e.session.len().min(12)],
                    e.counts.input,
                    e.counts.cache_creation,
                    e.counts.cache_read,
                    e.counts.output,
                    e.model,
                    chrono::DateTime::from_timestamp_millis(e.ts_ms)
                        .map(|d| d.format("%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| e.ts_ms.to_string()),
                ));
            }
        }
    }
    let _ = report;
    publish(app, &events, &sources, pricing, adapters);
}

/// Re-runs the aggregation over already-indexed events after a price refresh.
fn resummarize(
    app: &AppHandle,
    index: &Option<Index>,
    adapters: &[Box<dyn SourceAdapter>],
    detected: &[DetectedSource],
    pricing: &PricingMap,
) {
    let Some(index) = index else { return };
    let events = index.all_events().unwrap_or_default();
    let sources = index
        .source_statuses(detected)
        .unwrap_or_else(|_| fallback_sources(detected));
    publish(app, &events, &sources, pricing, adapters);
}


fn publish(
    app: &AppHandle,
    events: &[UsageEvent],
    sources: &[SourceStatus],
    pricing: &PricingMap,
    adapters: &[Box<dyn SourceAdapter>],
) {
    // Live quota (a keychain read, a vendor CLI, or an HTTPS call) is merged in
    // here; `usage-quota` caches each answer for its TTL, so this is at worst one
    // slow call per tool every five minutes, never per refresh.
    let polled_quota = {
        let poll_started = std::time::Instant::now();
        let mut q = usage_core::report::poll_quota(adapters);
        q.extend(usage_quota::collect());
        let per_tool: Vec<String> = q
            .iter()
            .map(|sample| {
                format!(
                    "{}:{}%",
                    sample.tool,
                    format_args!("{:.1}", sample.used_percent)
                )
            })
            .collect();
        crate::logging::info(&format!(
            "quota poll: {} windows in {} ms [{}]",
            q.len(),
            poll_started.elapsed().as_millis(),
            per_tool.join(", ")
        ));
        q
    };
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
    let report = usage_core::report::summarize(events, &opts);
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

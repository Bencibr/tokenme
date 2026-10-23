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

use tauri::{AppHandle, Emitter, Manager, Runtime};
use usage_core::pricing::PricingOptions;
use usage_core::report::{now_ms, AggregatePlan, QuotaView, ReportOptions};
use usage_core::{DateFilter, DetectedSource, MachineScope, PricingMap, Report, SourceAdapter, SourceFile, SourceStatus};
use usage_index::{Index, IngestOptions, Watcher, RETENTION_DAYS};

use crate::notify;
use crate::settings::Settings;
use crate::snapshot;
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
/// The adaptive floor widens with the last pass's cost (×3) and never exceeds
/// this — a pathological pass must not silence the watcher for minutes.
const MAX_PASS_COST: Duration = Duration::from_secs(5);
/// With the panel hidden the numbers have no watcher; ingest and quota both
/// back off to this floor.
const HIDDEN_PASS_GAP: Duration = Duration::from_secs(15);

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
/// With the panel hidden nobody is reading the numbers live; the probe cache
/// serves for three times as long between real vendor rounds.
const QUOTA_HIDDEN_TTL: Duration = Duration::from_secs(180);
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
    /// The panel switched the machine scope (全部/本机/远程). Cheap by design:
    /// the fold over the rollup is re-run, no ingest, no vendor probes.
    Scope,
}

/// Cloned into managed state so commands can poke the engine thread.
pub struct EngineChannel(pub Sender<Msg>);

/// Shared between the engine thread, the commands and the tray.
pub struct Shared {
    pub report: Mutex<Option<Report>>,
    /// A startup/open failure is separate from the report: a cold panel has
    /// no report yet, and returning the generic `indexing` sentinel forever
    /// hides a failure that the user can actually retry.
    pub startup_error: Mutex<Option<String>>,
    pub settings: Mutex<Settings>,
    ingesting: Mutex<()>,
    /// The machine scope every publish folds with. Read at publish time (never
    /// cached in the engine), so a switch applies to the very next report even
    /// if it lands while a slow pass is mid-flight.
    scope: Mutex<MachineScope>,
    scope_refresh_queued: AtomicBool,
}

impl Shared {
    pub fn new(settings: Settings) -> Self {
        Self {
            report: Mutex::new(None),
            startup_error: Mutex::new(None),
            settings: Mutex::new(settings),
            ingesting: Mutex::new(()),
            scope: Mutex::new(MachineScope::All),
            scope_refresh_queued: AtomicBool::new(false),
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

    /// The current scope, falling back to `All` on a poisoned lock: showing
    /// every machine is the report the user can least be surprised by.
    pub fn scope(&self) -> MachineScope {
        self.scope.lock().map(|s| s.clone()).unwrap_or(MachineScope::All)
    }

    pub fn set_scope(
        &self,
        scope: MachineScope,
        tx: Option<&Sender<Msg>>,
    ) -> Result<(bool, bool), String> {
        let mut slot = self.scope.lock().map_err(|_| "scope busy".to_string())?;
        if *slot == scope { return Ok((false, false)); }
        *slot = scope;
        let Some(tx) = tx else { return Ok((true, false)) };
        if self.scope_refresh_queued.swap(true, Ordering::AcqRel) {
            return Ok((true, false));
        }
        if tx.send(Msg::Scope).is_err() {
            self.scope_refresh_queued.store(false, Ordering::Release);
            return Err("engine channel is closed".into());
        }
        Ok((true, true))
    }

    pub fn report(&self) -> Option<Report> {
        self.report.lock().ok().and_then(|r| r.clone())
    }

    pub fn set_startup_error(&self, error: impl Into<String>) {
        if let Ok(mut slot) = self.startup_error.lock() {
            *slot = Some(error.into());
        }
    }

    pub fn clear_startup_error(&self) {
        if let Ok(mut slot) = self.startup_error.lock() {
            *slot = None;
        }
    }

    pub fn startup_error(&self) -> Option<String> {
        self.startup_error.lock().ok().and_then(|error| error.clone())
    }
}

/// Runs the engine on its own thread; `tx` is the same sender commands hold so
/// the file-watcher wake-ups can be merged into one queue.
///
/// The loop is supervised. `run()` returning means the engine *cannot* keep
/// working, and historically it did exactly that — silently — whenever a
/// price-table swap landed while a watcher burst was being debounced; the panel
/// then served frozen numbers forever with nothing in the log to say so. Nothing
/// here can end the application (quitting is `app.exit`, which kills the
/// process), so an unexpected exit is always a defect: log it, restart, and back
/// off so a persistent failure cannot spin the CPU.
pub fn start<R: Runtime>(app: AppHandle<R>, rx: Receiver<Msg>, tx: Sender<Msg>) {
    std::thread::Builder::new()
        .name("tokenme-engine".into())
        .spawn(move || {
            // `run` borrows both ends instead of owning them: a panic unwinds its
            // frame, and a `Receiver` consumed by that frame would leave the
            // restart with nothing to listen to (and every command's send failing
            // for the rest of the session).
            let mut attempt = 0u32;
            loop {
                // An engine panic must not take the whole panel down, and must not
                // end the updates either: catch it, log it, restart.
                let started = Instant::now();
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(app.clone(), &rx, &tx)));
                if matches!(outcome, Ok(Exit::Disconnected)) {
                    // Every sender is gone, which happens only while the app is
                    // being torn down. There is nothing left to serve.
                    crate::logging::info("engine loop ended with the command channel; engine thread exiting");
                    return;
                }
                let reason = match &outcome {
                    Ok(exit) => exit.reason().to_string(),
                    Err(panic) => panic_text(panic),
                };
                // A run that survived the settle window proved the restart works,
                // so the next failure starts counting again.
                if started.elapsed() > RESTART_SETTLE {
                    attempt = 0;
                }
                attempt += 1;
                let wait = restart_delay(attempt);
                crate::logging::error(&format!(
                    "engine stopped ({reason}) — restarting in {}s (attempt {attempt})",
                    wait.as_secs()
                ));
                std::thread::sleep(wait);
            }
        })
        .expect("failed to spawn the tokenme engine thread");
}

/// How long a restarted engine must stay alive before the failure counter resets.
const RESTART_SETTLE: Duration = Duration::from_secs(60);
const RESTART_BACKOFF: Duration = Duration::from_secs(2);
const RESTART_CAP: Duration = Duration::from_secs(300);

/// How long to wait before restart number `attempt` (1-based): 2 s, 4 s, 8 s …
/// until [`RESTART_CAP`] binds, so an engine that fails the instant it starts
/// cannot become a CPU burner — and, capped rather than abandoned, it still
/// comes back once whatever broke it heals.
fn restart_delay(attempt: u32) -> Duration {
    (RESTART_BACKOFF * (1u64 << (attempt - 1).min(10)) as u32).min(RESTART_CAP)
}

/// The panic payload, as far as it can be read. A panic carries `&str` or
/// `String` most of the time; anything else still has to say *that* the engine
/// died, which is the whole point of logging it.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into());
    format!("panicked: {detail}")
}

/// How the pass loop ended. Exactly one way is left: the command channel dying,
/// which means the app is going away. Everything that used to end it — a pricing
/// swap mid-debounce, a wake that outranked the window — is now a pass.
enum Exit {
    Disconnected,
}

impl Exit {
    fn reason(&self) -> &'static str {
        match self {
            Exit::Disconnected => "the command channel is gone (every sender dropped)",
        }
    }
}

fn run<R: Runtime>(app: AppHandle<R>, rx: &Receiver<Msg>, tx: &Sender<Msg>) -> Exit {
    // Cold start is the one latency the user feels directly, so the boot path
    // is measured rather than guessed: this clock is what the "boot:" line
    // below breaks down, and it is where a slow open gets attributed.
    let boot = Instant::now();
    // Last-known numbers from the previous run, before anything is read. The
    // whole boot path below (detect, pricing, the aggregation) costs 6.8–24.9 s
    // measured on this machine, and every number it produces was already on disk
    // when the process started; a file read costs milliseconds. The panel labels
    // this report as restored, so the first frame is faster rather than younger.
    publish_restored(&app);
    let at_restore = boot.elapsed();
    let adapters: Vec<Box<dyn SourceAdapter>> = usage_adapter_all::builtin_adapters();
    let detected: Vec<DetectedSource> = usage_adapter_all::detect_all();
    let at_detect = boot.elapsed();
    let mut pricing = PricingMap::load(&PricingOptions {
        offline: false,
        cache_dir: PricingOptions::default_cache_dir(),
        overrides: Default::default(),
    });
    let at_pricing = boot.elapsed();

    // An index that cannot be opened is not a quiet day either: every pass would
    // return without publishing and the webview would sit on "indexing" forever.
    let mut index = match Index::open_recovering_corruption(index_path()) {
        Ok((index, true)) => {
            crate::logging::error(
                "integrity: the index could not be opened; the original image was preserved and a new index was created for local re-scan",
            );
            Some(index)
        }
        Ok((index, false)) => Some(index),
        Err(e) => {
            app.state::<Shared>().set_startup_error(format!(
                "index could not be opened; local usage is unavailable until it is repaired: {e}"
            ));
            crate::logging::error(&format!(
                "index could not be opened — no data can be published until it is: {e}"
            ));
            None
        }
    };
    let at_index = boot.elapsed();

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

    // The second frame: this run's own fold over the index, before the scan has
    // read a single source file. The snapshot above may already have covered it;
    // this is what replaces it, and on a fresh install (no snapshot, empty index)
    // it is the boot screen's job until the scan lands. The quota probes are
    // skipped here on purpose — they come with the post-scan publish.
    if let Some(index) = index.as_mut() {
        let events = index.has_events().unwrap_or(false);
        let at_count = boot.elapsed();
        if events {
            let sources =
                index.source_statuses(&detected).unwrap_or_else(|_| fallback_sources(&detected));
            let at_sources = boot.elapsed();
            let _ = publish_with_quota(&app, index, &sources, &pricing, &adapters, false, false);
            let ms = |d: Duration| d.as_millis();
            crate::logging::info(&format!(
                "boot: published the index fold before the scan — restore {} ms, detect {} ms, \
                 pricing {} ms, index open {} ms, has-events {} ms, sources {} ms, report {} ms \
                 ({} ms of engine startup spent before the panel could render)",
                ms(at_restore),
                ms(at_detect - at_restore),
                ms(at_pricing - at_detect),
                ms(at_index - at_pricing),
                ms(at_count - at_index),
                ms(at_sources - at_count),
                ms(boot.elapsed() - at_sources),
                ms(boot.elapsed()),
            ));
        }
    }

    // SQLite WAL already guarantees crash-safe atomic transactions.
    // Running full `PRAGMA integrity_check` on every launch is an anti-pattern:
    // it scales linearly with DB size (~10s on 623MB, ~30s on 1GB).
    // The handle opened above is used directly; corruption is caught on actual
    // read/write failure and handled via existing recovery paths.

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
    let mut last_pass_cost = Duration::from_secs(1);
    // The cadence clock runs on its own: a machine whose tools log every
    // second would otherwise reset `recv_timeout` with every wake and starve
    // the timer — measured here, a 15s setting drifted to 30-60s between
    // publishes, which is exactly the "set 15s, data still moves every
    // minute" report. `remaining` shrinks across intervening passes, so the
    // cadence pass lands on schedule however many wakes cut in line.
    let mut last_cadence = Instant::now();

    loop {
        // Read per pass, so a cadence change from the panel applies on the next
        // wait without restarting the engine.
        let fallback = {
            let secs = app.state::<Shared>().settings().refresh_secs;
            Duration::from_secs(secs.clamp(10, 3600))
        };
        let remaining = fallback.saturating_sub(last_cadence.elapsed());
        match wait_for_work(rx, &mut pricing, remaining, &mut wake_roots) {
            Work::Ingest(reason) => {
                // A manual refresh just published fresh numbers too; the next
                // cadence pass is due a full period after it, not sooner.
                if reason == "cadence timer" || reason == "manual refresh" {
                    last_cadence = Instant::now();
                }
                if reason != "manual refresh" {
                    let since = last_pass.elapsed();
                    // Adaptive floor: the wider the last pass, the longer the
                    // breather — a 700ms index pass at 18 wakes a minute was a
                    // third of a core. With the panel tucked away nobody is
                    // watching the numbers, so the floor widens further.
                    let mut gap = MIN_PASS_GAP.max(last_pass_cost * 3);
                    if crate::panel::is_panel_hidden() {
                        gap = gap.max(HIDDEN_PASS_GAP);
                    }
                    if since < gap {
                        let shared = app.state::<Shared>();
                        if wait_for_ingest_gap(&shared.scope_refresh_queued, gap - since) {
                            fold_scope(&app, &mut index, &adapters, &detected, &pricing);
                        }
                    }
                }
                let pass_started = Instant::now();
                last_pass = Instant::now();
                ingest(&app, &mut index, &adapters, &detected, &pricing, reason, &mut last_publish,
                    &mut snapshot, &mut snapshot_at, &mut wake_roots);
                last_pass_cost = last_pass_cost
                    .max(pass_started.elapsed())
                    .min(MAX_PASS_COST);
            }
            Work::Resummarize => {
                crate::logging::info("pricing refreshed — re-summarizing indexed events");
                resummarize(&app, &mut index, &adapters, &detected, &pricing, false);
                // Wakes collected before the table landed are still pending:
                // `ingest` clears `wake_roots`, this arm leaves them for the next
                // pass, which then walks those tools' trees instead of restating
                // a snapshot built from the old prices.
            }
            Work::Scope => {
                fold_scope(&app, &mut index, &adapters, &detected, &pricing);
            }
            Work::Stopped => return Exit::Disconnected,
        }
    }
}

enum Work {
    Ingest(&'static str),
    Resummarize,
    /// The panel picked another machine scope; fold and republish.
    Scope,
    /// Every sender dropped. The only way this loop can end on its own.
    Stopped,
}

// shortcut: scope priority can lag by 25ms; use a wakeable wait if more controls need to interrupt this gap.
fn wait_for_ingest_gap(scope_queued: &AtomicBool, remaining: Duration) -> bool {
    let deadline = Instant::now() + remaining;
    loop {
        if scope_queued.load(Ordering::Acquire) { return true; }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() { return false; }
        std::thread::sleep(remaining.min(Duration::from_millis(25)));
    }
}

fn fold_scope<R: Runtime>(
    app: &AppHandle<R>,
    index: &mut Option<Index>,
    adapters: &[Box<dyn SourceAdapter>],
    detected: &[DetectedSource],
    pricing: &PricingMap,
) {
    let shared = app.state::<Shared>();
    if !shared.scope_refresh_queued.swap(false, Ordering::AcqRel) { return; }
    let scope = shared.scope();
    let started = Instant::now();
    crate::logging::info(&format!("scope_fold: phase=start scope={}", scope_kind(&scope)));
    let published = resummarize(app, index, adapters, detected, pricing, true);
    crate::logging::info(&format!(
        "scope_fold: phase=finish scope={} outcome={} total_ms={}",
        scope_kind(&shared.scope()),
        if published { "published" } else { "failed" },
        started.elapsed().as_millis(),
    ));
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
                return match drain_debounce(rx, pricing, wake_roots) {
                    Drained::Wakes => Work::Ingest("file change"),
                    // The defect that froze the panel for a whole afternoon: a
                    // price table arriving inside the debounce window was read as
                    // "quit". The wakes stay collected, the table is applied, and
                    // the pass that follows publishes both.
                    Drained::Pricing => Work::Resummarize,
                    Drained::Scope => Work::Scope,
                    Drained::Refresh => {
                        QUOTA_PASS_BUST.store(true, Ordering::Relaxed);
                        Work::Ingest("manual refresh")
                    }
                    Drained::Disconnected => Work::Stopped,
                };
            }
            Ok(Msg::Refresh) => {
                // A manual refresh is "everything, now": busting the quota pass
                // makes the next publish re-probe the vendors for real — the
                // idle cadence keeps reusing the cached pass.
                QUOTA_PASS_BUST.store(true, Ordering::Relaxed);
                return Work::Ingest("manual refresh");
            }
            Ok(Msg::Scope) => return Work::Scope,
            Err(RecvTimeoutError::Timeout) => return Work::Ingest("cadence timer"),
            Err(RecvTimeoutError::Disconnected) => return Work::Stopped,
        }
    }
}

/// What ended a debounce window.
enum Drained {
    /// The window closed; every wake in it is collected.
    Wakes,
    /// The user pressed refresh.
    Refresh,
    /// A price table landed mid-window.
    Pricing,
    /// The panel switched machine scope mid-window.
    Scope,
    /// Every sender dropped.
    Disconnected,
}

/// Keeps pushing the deadline out while events keep arriving. Whatever woke the
/// engine before the interrupt stays in `wake_roots` — a file change the watcher
/// reported is never dropped on the floor, and a refresh is never downgraded to
/// a plain file-change pass (which can early-return without publishing, leaving
/// the button looking like it did nothing).
fn drain_debounce(
    rx: &Receiver<Msg>,
    pricing: &mut PricingMap,
    wake_roots: &mut Vec<PathBuf>,
) -> Drained {
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
                return Drained::Pricing;
            }
            Ok(Msg::Scope) => return Drained::Scope,
            Ok(Msg::Refresh) => {
                usage_quota::clear_cache();
                return Drained::Refresh;
            }
            Err(RecvTimeoutError::Timeout) => return Drained::Wakes,
            Err(RecvTimeoutError::Disconnected) => return Drained::Disconnected,
        }
    }
}

fn ingest<R: Runtime>(
    app: &AppHandle<R>,
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
    if index.is_none() {
        match Index::open_recovering_corruption(index_path()) {
            Ok((opened, true)) => {
                crate::logging::error(
                    "integrity: the index was repaired on retry; the original image was preserved",
                );
                *index = Some(opened);
                shared.clear_startup_error();
            }
            Ok((opened, false)) => {
                *index = Some(opened);
                shared.clear_startup_error();
            }
            Err(e) => {
                shared.set_startup_error(format!(
                    "index could not be repaired; local usage is unavailable until it is repaired: {e}"
                ));
                crate::logging::error(&format!("index retry failed: {e}"));
                return;
            }
        }
    }
    let Some(index) = index.as_mut() else { return };

    let cutoff = cutoff_ms();
    let filter = DateFilter::new(Some(cutoff), None);
    // Discover scoping: privileged passes (initial scan, manual refresh) and
    // expired snapshots walk every tree; cadence timer restats the cached list
    // unless the TTL expired; a file-change wake walks only the tool that owns
    // the woken root. Every other tool restats its previous list — a couple of
    // stats per file, no walk. A file the watcher missed surfaces within SNAPSHOT_TTL.
    let full = matches!(reason, "initial scan" | "manual refresh")
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
    let ingest_started = Instant::now();
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
            // The pass's own diagnostics: a failed read (the errors also mark
            // their file for a re-read) and the adapters' silent-failure notes.
            // Rare by design — their presence is the signal.
            if !r.errors.is_empty() {
                crate::logging::error(&format!("ingest({reason}) errors: {}", r.errors.join(" | ")));
            }
            if !r.notes.is_empty() {
                crate::logging::info(&format!("ingest({reason}) notes: {}", r.notes.join(" | ")));
            }
            Some(r)
        }
        Err(e) => {
            crate::logging::error(&format!(
                "ingest({reason}) failed total_ms={}: {e}",
                ingest_started.elapsed().as_millis(),
            ));
            None
        }
    };
    let _ = index.prune(cutoff);
    // Planner-statistics refresh only on the idle pass: it reads enough of the
    // index that per-wake runs were measurable overhead for no benefit.
    if reason == "cadence timer" {
        index.optimize();
    }
    // Sync bundles from other machines land before the publish decision below,
    // so a fresh merge publishes on this same pass. A missing directory is the
    // normal no-sync case; a bad bundle is logged and left for the next round.
    let sync_merged = import_sync_dir(index);
    // A file wake that added nothing new would republish an identical report —
    // on a working machine that is most wakes (editors touch files constantly).
    // A wake that DID add events recomputes the entire report (all indexed
    // history, priced) and that costs hundreds of milliseconds, so drip writers
    // get at most one rebuild per PUBLISH_GAP. The cadence timer and manual
    // refresh always publish, keeping the time-shaped numbers moving. A merged
    // bundle publishes immediately: merges are rare, and the badge's freshness
    // must not wait out a drip writer's gap.
    if reason == "file change" {
        let quiet = report.as_ref().is_some_and(|r| r.new_events == 0);
        if quiet && !sync_merged {
            return;
        }
        if !sync_merged && last_publish.elapsed() < PUBLISH_GAP {
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

/// Merges every bundle sitting in `~/tokenme-sync` that this index has not
/// seen at its current content hash yet. Same function `tokenme import` uses —
/// hash check, key-based reconciliation, one transaction — so a bundle that
/// fails validation is rejected whole and stays on disk for the next pass,
/// while the panel keeps serving the previous numbers. Returns whether any
/// bundle merged, so the caller knows to publish.
fn import_sync_dir(index: &mut Index) -> bool {
    let Some(dir) = usage_index::default_sync_dir() else { return false };
    let Ok(entries) = std::fs::read_dir(&dir) else { return false };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let mut merged = false;
    for entry in entries {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        // Manifests, `.tmp` artifacts and anything else are not payloads.
        if !name.ends_with(".jsonl.gz") {
            continue;
        }
        let sha = match usage_index::sha256_file(&path) {
            Ok(sha) => sha,
            Err(e) => {
                crate::logging::error(&format!("sync: cannot hash {name}: {e}"));
                continue;
            }
        };
        match index.sync_file_memo(&name) {
            Ok(Some(known)) if known.eq_ignore_ascii_case(&sha) => continue,
            Ok(_) => {}
            Err(e) => {
                crate::logging::error(&format!("sync: cannot read the memo for {name}: {e}"));
                continue;
            }
        }
        match index.import_sync(&usage_index::ImportOptions { file: path.clone(), dry_run: false })
        {
            Ok(rep) => {
                crate::logging::info(&format!(
                    "sync: merged {name} from {} — {} rows ({} new, {} updated, {} deduped), calls +{}{}",
                    rep.origin, rep.rows, rep.inserted, rep.updated, rep.deduped, rep.calls_added,
                    if rep.self_import { " — origin equals this machine's hostname" } else { "" }
                ));
                if rep.stale_rows > 0 {
                    crate::logging::info(&format!(
                        "sync: {} older row(s) of {} are outside the bundle window (purged upstream); kept",
                        rep.stale_rows, rep.origin
                    ));
                }
                merged = true;
            }
            Err(e) => {
                crate::logging::error(&format!("sync: {name} rejected (kept for retry): {e}"));
            }
        }
    }
    merged
}

/// Re-runs the aggregation over already-indexed events after a price refresh.
fn resummarize<R: Runtime>(
    app: &AppHandle<R>,
    index: &mut Option<Index>,
    adapters: &[Box<dyn SourceAdapter>],
    detected: &[DetectedSource],
    pricing: &PricingMap,
    cache_only_quota: bool,
) -> bool {
    let Some(index) = index.as_mut() else { return false };
    let sources = index
        .source_statuses(detected)
        .unwrap_or_else(|_| fallback_sources(detected));
    publish_with_quota(app, index, &sources, pricing, adapters, true, cache_only_quota)
}


fn publish<R: Runtime>(
    app: &AppHandle<R>,
    index: &mut Index,
    sources: &[SourceStatus],
    pricing: &PricingMap,
    adapters: &[Box<dyn SourceAdapter>],
) {
    let _ = publish_with_quota(app, index, sources, pricing, adapters, true, false);
}

/// The first frame: the previous run's own published report, read off disk
/// before this run has touched a source file, a price table or a vendor.
///
/// Restoring it is the difference between a first report at milliseconds and one
/// at 6.8–24.9 s measured on this machine, and every number in it was true when
/// it was folded. What it cannot claim is freshness, so `from_previous_run` is set
/// for the panel to label, and the quota strip goes back to pending — a vendor
/// window sampled an hour ago is not this run's answer, and the panel already has
/// an honest state for "not yet". Anything [`snapshot::read`] refuses leaves the
/// panel exactly where it was: the boot note until the index fold lands.
fn publish_restored<R: Runtime>(app: &AppHandle<R>) {
    let scope = app.state::<Shared>().scope();
    let Some(mut report) = snapshot::read(&scope) else {
        crate::logging::info("boot: nothing to restore — the panel waits for the index fold");
        return;
    };
    let published_at = report.generated_at_ms;
    report.from_previous_run = true;
    report.quotas.clear();
    report.quotas_pending = true;
    if let Ok(mut slot) = app.state::<Shared>().report.lock() {
        *slot = Some(report.clone());
    }
    app.state::<Shared>().clear_startup_error();
    let _ = app.emit(REPORT_EVENT, &report);
    let mode = app.state::<Shared>().settings().tray_mode;
    tray::refresh(app, &report, mode);
    crate::logging::info(&format!(
        "boot: restored last-known report folded {} ms ago",
        now_ms() - published_at
    ));
}

/// `poll_quota = false` publishes without waiting on the vendor probes — the
/// boot path uses it to put last-known numbers on screen instantly; the probes
/// land with the post-scan publish a moment later.
fn publish_with_quota<R: Runtime>(
    app: &AppHandle<R>,
    index: &mut Index,
    sources: &[SourceStatus],
    pricing: &PricingMap,
    adapters: &[Box<dyn SourceAdapter>],
    poll_quota: bool,
    cache_only_quota: bool,
) -> bool {
    // Live quota (a keychain read, a vendor CLI, or an HTTPS call) is merged in
    // here. The pass is reused for QUOTA_PASS_TTL — probing every publish made
    // a busy file-watcher cadence burn 30% CPU for numbers that had not moved.
    let (polled_quota, quota_ready) = if poll_quota {
        quota_pass(app, adapters, cache_only_quota)
    } else {
        (Vec::new(), false)
    };
    let captured = now_ms();
    let shared = app.state::<Shared>();
    let mut scope = shared.scope();
    let folded_at = usage_core::report::instant_after_polling(false, captured, &polled_quota);
    // The fold runs at most twice: an Origin scope naming a machine the fresh
    // fold no longer holds resets to All and re-folds once, so the panel
    // self-corrects within one cadence instead of stranding the user on a
    // machine that has no way back (the scope menu collapses when no remote
    // row remains). `report.machines` is unscoped by construction — the rollup
    // spans the whole index whatever the report's own scope — so a missing
    // origin means the index truly holds no rows for it, not a filtered view.
    // Which origins have left the sync promise (a revoked device on the
    // primary, a severed downlink on the secondary). One read-only probe per
    // publish: any detach or re-pair lands within one cadence, no deletes —
    // the fold just stops counting the rows until the pairing is live again.
    let exclude_origins = usage_device_sync::SyncStore::read_detached_origins(&index_path());
    let mut report = loop {
        let opts = ReportOptions {
            pricing,
            // After the probes, not before them: a vendor CLI that answers late must
            // not leave the report stamped older than a window it is showing.
            now_ms: folded_at,
            sources: sources.to_vec(),
            recent_session_limit: 12,
            // The rare second fold re-uses the pass's samples; the vec is a
            // dozen rows, noise next to the fold itself.
            polled_quota: polled_quota.clone(),
            budgets: shared.settings().budgets,
            scope: scope.clone(),
            exclude_origins: exclude_origins.clone(),
        };
        // Every period's numbers fold from the day rollup (~2k rows) plus the
        // plan's live slices over today — never from re-reading all events. The
        // scope travels into the fetch as well: the sessions' top-N is applied in
        // SQL, so it must already be the top-N of the chosen scope — and of the
        // detached set, or a departed origin would fill the slots.
        let plan = AggregatePlan::build(opts.now_ms, opts.recent_session_limit);
        let facts_started = Instant::now();
        let facts = match index.report_facts_excluding(&plan, &scope, &exclude_origins) {
            Ok(facts) => facts,
            Err(e) => {
                crate::logging::error(&format!(
                    "publish: facts query failed scope={} total_ms={} — previous report stays on screen: {e}",
                    scope_kind(&scope), facts_started.elapsed().as_millis(),
                ));
                return false;
            }
        };
        if facts.rebuilt {
            crate::logging::info(
                "publish: rebuilt the day rollup from the event table (first publish after an \
                 upgrade, or after a fail-safe invalidation)",
            );
        }
        let folded = usage_core::report::summarize_facts(&facts, &opts);
        if let MachineScope::Origin { name } = &scope {
            // Correct only the scope this fold actually used: a pick that
            // landed mid-fold has already replaced the stored scope and queued
            // its own Msg::Scope, and clobbering it with All would silently
            // drop the user's click (the same stale-read race the tray-mode
            // refresh guards below). If the newer pick names a dead machine
            // too, its own pass resets it the same way.
            if !folded.machines.iter().any(|machine| &machine.origin == name) && shared.scope() == scope {
                let _ = shared.set_scope(MachineScope::All, None);
                crate::logging::info(&format!(
                    "scope_reset: origin {name:?} matches no machine in the fold — scope back to all"
                ));
                scope = MachineScope::All;
                continue;
            }
        }
        break folded;
    };
    let cycle_usage_started = Instant::now();
    if let Err(e) = index.attach_cycle_usage(&mut report.quotas, report.generated_at_ms) {
        crate::logging::error(&format!(
            "publish: cycle token usage failed total_ms={} — quota rows stay visible without the summary: {e}",
            cycle_usage_started.elapsed().as_millis(),
        ));
    }
    // The boot publish deliberately skips the probes: until the post-scan
    // publish lands, the quota strip shows "探测中" instead of vanishing.
    report.quotas_pending = !quota_ready;
    if let Ok(mut slot) = app.state::<Shared>().report.lock() {
        *slot = Some(report.clone());
    }
    // The next launch's first frame. Written here rather than on the boot paths so
    // that whatever the panel has been shown is also what gets remembered — a
    // snapshot of a report nobody saw would make "last known" mean two things.
    snapshot::write(&report);
    // Threshold banners: the engine only offers the fresh views; the notify
    // worker owns the tier state, the permission flow and its own file. The
    // tier lines and the mute list are the user's, read fresh per publish.
    let displays: HashMap<String, String> =
        sources.iter().map(|s| (s.id.clone(), s.display.clone())).collect();
    let gate = notify::Gate::from(&app.state::<Shared>().settings());
    notify::observe(&report.quotas, &displays, crate::lang::get(), gate);
    let _ = app.emit(REPORT_EVENT, &report);
    // Re-read the mode AFTER the slow quota probes: a switch made while
    // polling ran must not be painted back to the old display (the stale-read
    // race behind "切换有很大的延迟").
    let mode = app.state::<Shared>().settings().tray_mode;
    tray::refresh(app, &report, mode);
    true
}

/// The host-exit half of the pass gate: whether the host process test lets a
/// tool through, or the pause is off altogether.
///
/// A claim-bearing tool with `auto_checkin` on is never host-paused: its daily
/// grant is the one number that still moves while the host app is closed, and
/// the claim itself is an HTTPS call against the account — not a read of the
/// host's state — so a closed IDE must not silence it, and its rows stay live
/// in the strip. The user's own switches compose on top in the caller; this
/// answers the host half alone so the refusal log can ask the same function
/// and never name a tool the pass actually asked.
fn host_gate_open(tool: &str, auto_checkin: bool, alive: &Option<HashSet<String>>) -> bool {
    if auto_checkin && usage_quota::is_checkin_tool(tool) {
        return true;
    }
    match alive {
        Some(names) => usage_quota::host::any_host_running(tool, names),
        None => true,
    }
}

/// One full quota pass: the per-window adapter probes plus the budget views
/// (host-exit-paused or not), reused for [`QUOTA_PASS_TTL`].
fn quota_pass<R: Runtime>(
    app: &AppHandle<R>,
    adapters: &[Box<dyn SourceAdapter>],
    cache_only: bool,
) -> (Vec<QuotaView>, bool) {
    if !cache_only && QUOTA_PASS_BUST.swap(false, Ordering::Relaxed) {
        usage_quota::clear_cache();
        if let Ok(mut slot) = QUOTA_PASS.lock() {
            *slot = None;
        }
    }
    if let Ok(slot) = QUOTA_PASS.lock() {
        if let Some((at, q)) = slot.as_ref() {
            // A manual refresh (QUOTA_PASS_BUST) empties the slot above, so
            // whatever survives here answers the live TTL; with the panel
            // hidden the same answer serves three times as long — the probes
            // are the multi-threaded decrypt work nobody is watching.
            let ttl = if crate::panel::is_panel_hidden() { QUOTA_HIDDEN_TTL } else { QUOTA_PASS_TTL };
            if cache_only || at.elapsed() < ttl {
                if cache_only {
                    crate::logging::info(&format!(
                        "quota poll: scope switch reused cache age_ms={}",
                        at.elapsed().as_millis(),
                    ));
                }
                return (q.clone(), true);
            }
        }
    }
    if cache_only {
        crate::logging::info("quota poll: scope switch skipped vendor probes; no cached result yet");
        return (Vec::new(), false);
    }
    let poll_started = Instant::now();
    let settings = app.state::<Shared>().settings();
    // The check-in master switch rides the settings into the probes: it is
    // read inside their fetches, past this pass's host-gate decisions.
    usage_quota::set_auto_checkin_enabled(settings.auto_checkin);
    let mut q = usage_core::report::poll_quota(adapters);
    // Two gates, one pass. The user's own word (`quota_polling`,
    // `quota_probes_off`) is the standing instruction; the host-exit test is
    // re-decided every pass because a process comes and goes — except for the
    // claim-bearing tools under `auto_checkin`, whose daily grant is the one
    // number that still moves with the host closed (see `host_gate_open`).
    // Either gate refusing means the same thing: no request leaves, and the
    // last known answer stays on screen read from the cache however old it is.
    // The four probes with no host mapping (`copilot`, `gemini`, `kimicode`,
    // `minimaxcode`) are only reachable through the first — which is the whole
    // reason the settings sheet grew a per-tool page.
    let alive = settings
        .host_exit_pause
        .then(usage_quota::host::running_process_names);
    let polling_off = !settings.quota_polling;
    let auto_checkin = settings.auto_checkin;
    let gate = {
        // The gate consumes both; the probe list below re-asks the same
        // questions per tool to say WHICH refusal emptied the windows.
        let alive = alive.clone();
        let settings = settings.clone();
        move |tool: &str| -> bool {
            settings.probe_allowed(tool) && host_gate_open(tool, auto_checkin, &alive)
        }
    };
    q.extend(usage_quota::collect_gated(usage_quota::BUDGET, gate));
    // Off means off: a tool whose probe switch is closed leaves the panel
    // entirely — the switch reads "I don't use this", and a frozen last
    // answer under it is clutter, not information. Budget rows are the
    // user's own construction and stay. Only the switch hides here: a
    // host that merely quit keeps the freeze, because its rows are the
    // "last known answer" the strip promises while the app is closed.
    q.retain(|view| {
        view.origin == usage_core::QuotaOrigin::Budget || settings.probe_allowed(&view.tool)
    });
    // Edge-triggered, because the gate stays off for hours and a line every 60 s
    // would bury the pass timings this log exists to show.
    static OFF_LOGGED: AtomicBool = AtomicBool::new(false);
    if polling_off {
        if !OFF_LOGGED.swap(true, Ordering::Relaxed) {
            crate::logging::info("quota poll: 总闸已关 · 只读缓存不再探测 / polling off: cache only, no vendor request");
        }
    } else {
        OFF_LOGGED.store(false, Ordering::Relaxed);
    }
    let per_tool: Vec<String> = q
        .iter()
        .map(|sample| format!("{}:{}%", sample.tool, format_args!("{:.1}", sample.used_percent)))
        .collect();
    // Why a probe could not answer: its own switch (`off`) or its host process
    // gone (`host`) — the gate's two refusals, named per tool. The empty
    // brackets with an empty off-list is the different case: probes ran and
    // nobody answered, which is the one worth chasing in a user's log.
    let mut off: Vec<String> = Vec::new();
    if !polling_off {
        for probe in usage_quota::inventory() {
            if !settings.probe_allowed(&probe.tool) {
                off.push(format!("{}:off", probe.tool));
            } else if !host_gate_open(probe.tool, auto_checkin, &alive) {
                off.push(format!("{}:host", probe.tool));
            }
        }
    }
    let off_note =
        if off.is_empty() { String::new() } else { format!(" probes-off: [{}]", off.join(", ")) };
    crate::logging::info(&format!(
        "quota poll: {} windows in {} ms [{}]{}",
        q.len(),
        poll_started.elapsed().as_millis(),
        per_tool.join(", "),
        off_note
    ));
    if let Ok(mut slot) = QUOTA_PASS.lock() {
        *slot = Some((Instant::now(), q.clone()));
    }
    (q, true)
}

fn scope_kind(scope: &MachineScope) -> &'static str {
    match scope {
        MachineScope::All => "all",
        MachineScope::Local => "local",
        MachineScope::Origin { .. } => "origin",
    }
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
pub(crate) fn index_path() -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Two tables the tests can tell apart without fetching anything: `cache_dir`
    /// is echoed into the meta, and an offline load with no dir falls back to the
    /// bundled snapshot.
    fn table(mark: Option<&str>) -> PricingMap {
        PricingMap::load(&PricingOptions {
            offline: true,
            cache_dir: mark.map(PathBuf::from),
            overrides: Default::default(),
        })
    }

    fn marked(pricing: &PricingMap) -> bool {
        pricing.meta().cache_dir.is_some()
    }

    /// The exact shape that froze the panel on 2026-10-04: the watcher wakes the
    /// engine and a price table lands before the debounce window closes. The loop
    /// has to survive it, apply the table, and still owe the wake a read.
    #[test]
    fn a_price_table_mid_debounce_never_ends_the_loop() {
        let (tx, rx) = mpsc::channel();
        let mut pricing = table(None);
        let mut wakes = Vec::new();
        tx.send(Msg::Wake(PathBuf::from("/root/codex"))).unwrap();
        tx.send(Msg::Pricing(table(Some("/tmp/pricing-swap")))).unwrap();

        let work = wait_for_work(&rx, &mut pricing, DEBOUNCE * 10, &mut wakes);
        assert!(matches!(work, Work::Resummarize), "a pricing swap is a pass, not an exit");
        assert!(marked(&pricing), "the table that landed must be the live one");
        assert_eq!(wakes, vec![PathBuf::from("/root/codex")], "the wake is still owed a read");
        // The channel must still be usable afterwards — that is the whole
        // difference between "restarts" and "the panel goes quiet forever".
        tx.send(Msg::Refresh).unwrap();
        assert!(matches!(
            wait_for_work(&rx, &mut pricing, DEBOUNCE, &mut wakes),
            Work::Ingest("manual refresh")
        ));
    }

    #[test]
    fn a_burst_of_wakes_is_one_pass_and_keeps_every_root() {
        let (tx, rx) = mpsc::channel();
        let mut pricing = table(None);
        let mut wakes = Vec::new();
        for root in ["/root/a", "/root/b", "/root/c"] {
            tx.send(Msg::Wake(PathBuf::from(root))).unwrap();
        }
        let started = Instant::now();
        let work = wait_for_work(&rx, &mut pricing, DEBOUNCE * 10, &mut wakes);
        assert!(matches!(work, Work::Ingest("file change")));
        assert_eq!(wakes.len(), 3, "the debounce merges passes, never events");
        assert!(started.elapsed() >= DEBOUNCE, "the window has to actually hold");
    }

    /// A refresh arriving inside the window used to be answered with a plain
    /// `file change` pass — which is allowed to early-return without publishing,
    /// so the button the user pressed when the numbers looked frozen could do
    /// nothing at all.
    #[test]
    fn a_refresh_inside_the_debounce_window_is_still_a_manual_pass() {
        let (tx, rx) = mpsc::channel();
        let mut pricing = table(None);
        let mut wakes = Vec::new();
        tx.send(Msg::Wake(PathBuf::from("/root/a"))).unwrap();
        tx.send(Msg::Refresh).unwrap();
        let work = wait_for_work(&rx, &mut pricing, DEBOUNCE * 10, &mut wakes);
        assert!(matches!(work, Work::Ingest("manual refresh")), "not a file-change pass");
        assert_eq!(wakes.len(), 1);
    }

    #[test]
    fn only_a_lost_channel_stops_the_loop() {
        let (tx, rx) = mpsc::channel::<Msg>();
        drop(tx);
        let mut pricing = table(None);
        let mut wakes = Vec::new();
        assert!(matches!(
            wait_for_work(&rx, &mut pricing, DEBOUNCE, &mut wakes),
            Work::Stopped
        ));

        // Everything else stays a pass: a table on its own, then quiet.
        let (tx, rx) = mpsc::channel();
        tx.send(Msg::Pricing(table(Some("/tmp/other")))).unwrap();
        assert!(matches!(
            wait_for_work(&rx, &mut pricing, DEBOUNCE, &mut wakes),
            Work::Resummarize
        ));
        assert!(matches!(
            wait_for_work(&rx, &mut pricing, Duration::from_millis(1), &mut wakes),
            Work::Ingest("cadence timer")
        ));
    }

    #[test]
    fn a_panic_always_reads_as_a_reason() {
        let text: Box<dyn std::any::Any + Send> = Box::new("index went away");
        assert_eq!(panic_text(&*text), "panicked: index went away");
        let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("lock poisoned"));
        assert_eq!(panic_text(&*owned), "panicked: lock poisoned");
        // A payload nobody can read still has to say that the engine died.
        let opaque: Box<dyn std::any::Any + Send> = Box::new(7u8);
        assert_eq!(panic_text(&*opaque), "panicked: non-string panic payload");
    }

    #[test]
    fn startup_error_is_cleared_after_a_successful_index_open() {
        let shared = Shared::new(Settings::default());
        assert!(shared.startup_error().is_none());
        shared.set_startup_error("index could not be opened");
        assert_eq!(shared.startup_error().as_deref(), Some("index could not be opened"));
        shared.clear_startup_error();
        assert!(shared.startup_error().is_none());
    }

    #[test]
    fn scope_switch_burst_queues_once_and_keeps_latest_scope() {
        let shared = Shared::new(Settings::default());
        let (tx, rx) = mpsc::channel();
        let remote = MachineScope::Origin { name: "remote-device".into() };

        assert_eq!(shared.set_scope(MachineScope::Local, Some(&tx)), Ok((true, true)));
        assert_eq!(shared.set_scope(remote.clone(), Some(&tx)), Ok((true, false)));
        assert_eq!(shared.scope(), remote.clone());
        assert!(matches!(rx.try_recv(), Ok(Msg::Scope)));
        assert!(rx.try_recv().is_err(), "a scope burst must queue only one fold");

        shared.scope_refresh_queued.store(false, Ordering::Release);
        assert_eq!(shared.set_scope(remote, Some(&tx)), Ok((false, false)));
        assert!(rx.try_recv().is_err(), "selecting the active scope must be a no-op");
    }

    #[test]
    fn queued_scope_interrupts_adaptive_ingest_gap() {
        let queued = std::sync::Arc::new(AtomicBool::new(false));
        let wake = std::sync::Arc::clone(&queued);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            wake.store(true, Ordering::Release);
        });
        let started = Instant::now();
        assert!(wait_for_ingest_gap(&queued, Duration::from_secs(15)));
        assert!(started.elapsed() < Duration::from_millis(500));
        setter.join().unwrap();
    }

    /// The `auto_checkin` exemption is exactly as wide as the claim registry:
    /// with the switch on, a check-in host that is NOT running still passes
    /// the host gate — the claim is HTTPS and a closed IDE is no reason to
    /// miss the day — while every other tool keeps following its process.
    #[test]
    fn auto_checkin_exempts_only_the_claim_bearers_from_the_host_pause() {
        let alive = Some(HashSet::new());
        assert!(!host_gate_open("trae_cn", false, &alive), "off keeps today's rule");
        assert!(host_gate_open("trae_cn", true, &alive), "on: the claim fires with the IDE closed");
        assert!(host_gate_open("qoder", true, &alive));
        assert!(host_gate_open("qoder_cn", true, &alive));
        assert!(!host_gate_open("qoder_cn", false, &alive));
        assert!(!host_gate_open("claude", true, &alive), "not a claim bearer");
        assert!(!host_gate_open("zcode", true, &alive));
        assert!(host_gate_open("copilot", true, &alive), "no host mapping — never paused");
        assert!(host_gate_open("trae_cn", false, &None), "pause off means everything passes");

        let mut running: HashSet<String> = HashSet::new();
        running.insert("Trae CN Helper".to_string());
        assert!(host_gate_open("trae_cn", false, &Some(running)), "a running host passes as before");
    }

    #[test]
    fn restart_delay_grows_and_caps() {
        assert_eq!(restart_delay(1), Duration::from_secs(2));
        assert_eq!(restart_delay(4), Duration::from_secs(16));
        // The cap has to bind — a decorative `RESTART_CAP` would leave a
        // hard-failing engine restarting every 512 s forever — and an unbounded
        // attempt counter must not overflow the shift.
        assert_eq!(restart_delay(9), RESTART_CAP);
        assert_eq!(restart_delay(u32::MAX), RESTART_CAP);
    }
}

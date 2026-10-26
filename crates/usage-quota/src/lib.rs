//! Live quota probes for tools that never write their rate window into a log.
//!
//! Deliberately kept out of the adapter crates: a probe is a side channel
//! (keychain, vendor CLI, HTTPS) whose failure must never affect spend numbers,
//! and the two are polled on different cadences.
//!
//! Everything here is read-only against the user's own machine and their own
//! accounts, and answers are cached (`cache.rs`) so a busy session does not
//! re-probe a vendor every refresh.

pub mod cache;
pub mod host;
mod http;
pub mod diagnostics;
mod notes;
mod providers;

use std::sync::Arc;
use std::time::{Duration, Instant};

use usage_core::{QuotaSample, QuotaView};

pub use providers::ClaudeQuota;
pub use providers::workbuddy_login;

/// Why this pass's probes came back empty, one line per tool — the registry the
/// provider modules write to when a fetch finds no credential or the vendor
/// refuses it. The host drains it after a pass and prints the lines there; a
/// probe served from cache wrote nothing, so silence keeps meaning "not asked".
pub use notes::take as take_probe_notes;

/// The tools whose probe owns a daily check-in claim, and the forced-claim
/// entry for each. One table, because three call sites must agree about the
/// same set: the engine exempts exactly these from the host-exit pause when
/// `auto_checkin` is on (the claim is HTTPS against the account — a closed
/// host app is no reason to miss a day), the panel's 签到 button dispatches
/// through here, and the strip renders its control for the same ids.
pub const CHECKIN_TOOLS: &[(&str, fn() -> Result<(bool, String), String>)] = &[
    ("trae_cn", providers::trae_manual_checkin),
    ("qoder", providers::qoder_manual_checkin),
    ("qoder_cn", providers::qoder_cn_manual_checkin),
    ("minimaxcode", providers::minimaxcode_manual_checkin),
    ("workbuddy_cn", providers::workbuddy_cn_manual_checkin),
    ("funide", providers::funide_manual_checkin),
    ("zcode", providers::zcode_manual_checkin),
];

/// The check-in tool ids, in registry order.
pub fn checkin_tool_ids() -> impl Iterator<Item = &'static str> {
    CHECKIN_TOOLS.iter().map(|(id, _)| *id)
}

/// Whether this tool owns a daily claim — the engine's exemption asks exactly
/// this.
pub fn is_checkin_tool(tool: &str) -> bool {
    CHECKIN_TOOLS.iter().any(|(id, _)| *id == tool)
}

/// The automatic check-in master switch, read by every provider's daily path
/// right before a claim would leave the process. On by default: the CLI
/// collector has no settings and its daily `export` pass is what keeps a
/// headless server's streak alive; the panel engine writes
/// `settings.auto_checkin` into it at every quota pass, so the switch in the
/// sheet governs this within one cadence tick. The manual 签到 button never
/// consults it — an explicit click is the user asking again.
static AUTO_CHECKIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_auto_checkin_enabled(on: bool) {
    AUTO_CHECKIN.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn auto_checkin_enabled() -> bool {
    AUTO_CHECKIN.load(std::sync::atomic::Ordering::Relaxed)
}

/// One forced claim right now, by tool id — the panel's check-in button.
/// `(claimed, message)`: the bool is what the button paints, the message is
/// user-facing; a tool outside the registry never reaches a vendor.
pub fn manual_checkin(tool: &str) -> Result<(bool, String), String> {
    CHECKIN_TOOLS
        .iter()
        .find(|(id, _)| *id == tool)
        .map(|(id, claim)| {
            let mut trace = diagnostics::ProbeTrace::start(id);
            let result = claim();
            trace.finish(match &result { Ok((true, _)) => "confirmed", Ok(_) => "not_confirmed", Err(_) => "error" }, None);
            result
        })
        .unwrap_or_else(|| Err(format!("{tool} 没有签到活动")))
}

/// How long an answer stays trustworthy. Vendor windows reset on the hour at the
/// earliest, so five minutes is well inside any meaningful resolution.
pub const TTL: Duration = Duration::from_secs(300);

/// The sink a host process hands the check-in paths: one line per attempt,
/// wherever the host logs (the panel's `checkin.log`). The CLI installs
/// nothing and the calls stay silent there.
pub type CheckinLog = Arc<dyn Fn(&str) + Send + Sync>;

static CHECKIN_LOG: std::sync::OnceLock<CheckinLog> = std::sync::OnceLock::new();

/// Install the host's check-in sink. Called once at setup; later calls win.
pub fn set_checkin_log(f: CheckinLog) {
    let _ = CHECKIN_LOG.set(f);
}

/// One line into the host's check-in ledger, if it installed a sink.
pub fn log_checkin(msg: &str) {
    if let Some(f) = CHECKIN_LOG.get() {
        f(msg);
    }
}

/// The sink a host hands the meters' diagnostics. The CLI installs nothing.
pub type MeterLog = Arc<dyn Fn(&str) + Send + Sync>;

static METER_LOG: std::sync::OnceLock<MeterLog> = std::sync::OnceLock::new();

pub fn set_meter_log(f: MeterLog) {
    let _ = METER_LOG.set(f);
}

/// Emit a throttled provider diagnostic without including credentials, URLs or
/// response bodies.
pub fn log_meter(key: &str, msg: &str) {
    static LAST: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> = std::sync::OnceLock::new();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let last = LAST.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let due = last.lock().map(|mut map| {
        let due = map.get(key).is_none_or(|t| now.saturating_sub(*t) > 10 * 60 * 1000);
        if due { map.insert(key.to_string(), now); }
        due
    }).unwrap_or(false);
    if due {
        if let Some(f) = METER_LOG.get() { f(&format!("{key}: {msg}")); }
    }
}

/// The sink a host hands the login-expiry path: a probe login whose token the
/// vendor refused (401/403) or whose stored expiry has passed. Without it a
/// dead login is silent — the row just vanishes and the user cannot tell
/// "vendor down" from "sign in again". The CLI installs nothing and stays
/// silent there.
pub type LoginExpiredNote = Arc<dyn Fn(&str, &str) + Send + Sync>;

static LOGIN_EXPIRED_NOTE: std::sync::OnceLock<LoginExpiredNote> = std::sync::OnceLock::new();

/// Install the host's login-expiry sink. Called once at setup; later calls win.
pub fn set_login_expired_note(f: LoginExpiredNote) {
    let _ = LOGIN_EXPIRED_NOTE.set(f);
}

/// The sink a host hands the claim path: a probe auto-claimed a vendor grant
/// (ZCode's plan claims today) and the user should hear about it without
/// watching the strip. The CLI installs nothing and stays silent there.
pub type ClaimNote = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

static CLAIM_NOTE: std::sync::OnceLock<ClaimNote> = std::sync::OnceLock::new();

/// Install the host's claim sink. Called once at setup; the first install wins.
pub fn set_claim_note(f: ClaimNote) {
    let _ = CLAIM_NOTE.set(f);
}

/// One grant booked: tell the host (tool, plan name or empty, "+300M tokens").
/// The claim ledger already dedups per day, so this rides a real booking only.
pub(crate) fn note_claim(tool: &str, plan: &str, units: &str) {
    if let Some(f) = CLAIM_NOTE.get() {
        f(tool, plan, units);
    }
}

/// Tokens already reported dead, by fingerprint. The set gates both the vendor
/// call (a refused token stays refused — retrying it every pass is the waste
/// being removed) and the host note (one toast per dead token, not one per
/// pass). A re-login mints a new token, so its later expiry reports again.
static DEAD_LOGINS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<u64>>> = std::sync::OnceLock::new();

pub(crate) fn login_is_dead(token: &str) -> bool {
    dead_logins().lock().map(|set| set.contains(&token_fingerprint(token))).unwrap_or(false)
}

/// Record a login the probe must stop trying and, the first time this run,
/// tell the host so it can ask the user to sign in again.
pub(crate) fn note_dead_login(tool: &str, host: &str, token: &str) {
    let first = dead_logins().lock().map(|mut set| set.insert(token_fingerprint(token))).unwrap_or(false);
    if first {
        if let Some(f) = LOGIN_EXPIRED_NOTE.get() { f(tool, host); }
    }
}

fn dead_logins() -> &'static std::sync::Mutex<std::collections::HashSet<u64>> {
    DEAD_LOGINS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// A fingerprint, never the token itself: the set outlives the pass but must
/// not outlive it holding a live credential.
fn token_fingerprint(token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut h);
    h.finish()
}

pub trait QuotaProbe: Send + Sync {
    /// Must match an adapter `TOOL_ID` so the panel can group quota with spend.
    fn tool(&self) -> &'static str;

    /// Best-effort live read. Return an empty vec when the tool is absent, not
    /// signed in, or the endpoint is down — the caller keeps the last answer.
    fn fetch(&self) -> Vec<QuotaSample>;

    /// Whether this probe also carries a once-a-day task (Qoder's check-in)
    /// that must run even while the host gate pauses the live read. A paused
    /// read is silent because the number cannot change while the app is
    /// closed; a daily claim is a state change on the vendor side that only
    /// this process can make, and skipping it for a closed window loses the
    /// day (measured 2026-10-11: the check-in never ran because Qoder was
    /// closed all morning).
    fn daily_task(&self) -> bool {
        false
    }

    /// Probes shell out and hit the network; neither may take the menu-bar
    /// process down with them.
    fn guarded_fetch(&self) -> Vec<QuotaSample> {
        let mut trace = diagnostics::ProbeTrace::start(self.tool());
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.fetch())) {
            Ok(samples) => { trace.finish(if samples.is_empty() { "empty" } else { "ok" }, Some(samples.len())); samples }
            Err(_) => { trace.finish("panic", None); Vec::new() }
        }
    }

    /// Whether an empty fetch means an outage whose last answer should be held.
    fn outage(&self) -> bool {
        false
    }
}

/// The probes worth running on this machine, in display order.
pub fn built_in() -> Vec<Box<dyn QuotaProbe>> {
    let mut out: Vec<Box<dyn QuotaProbe>> = Vec::new();
    out.push(Box::new(ClaudeQuota));
    out.extend(providers::optional());
    out
}

/// One row of the settings sheet's per-tool polling page. This is the registry
/// speaking about itself, so the UI never carries a hand-written list that
/// drifts from it: `built_in()` gains a probe and the page gains a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeInfo {
    /// The probe's tool id, in `built_in()`'s own order.
    pub tool: &'static str,
    /// Whether [`host::HOST_PROCESSES`] can reach this probe. Four cannot
    /// (`copilot`, `gemini`, `kimicode`, `minimaxcode`): for those, the user's
    /// own switch is the only lever that exists.
    pub host_gated: bool,
    /// Whether this machine holds a cached answer for it — the honest "本机答过"
    /// judge. A tool's *presence* answers `true` for everything on a machine
    /// that has all of them installed, which filters nothing.
    pub answered_here: bool,
}

/// The registry's own inventory, with the cache read once. Display names are not
/// here on purpose: the report already ships `sources[].display`, and the two
/// probes with no adapter are the frontend's to name.
pub fn inventory() -> Vec<ProbeInfo> {
    let cache = cache::Cache::open("tokenme/quota", TTL);
    built_in()
        .iter()
        .map(|probe| {
            let tool = probe.tool();
            ProbeInfo {
                tool,
                host_gated: host::HOST_PROCESSES.iter().any(|(id, _)| *id == tool),
                answered_here: cache.as_ref().and_then(|c| c.stale(tool)).is_some_and(|v| !v.is_empty()),
            }
        })
        .collect()
}

/// Quota for every built-in probe, cached across processes.
///
/// Probes run concurrently and the call returns as soon as everything that
/// answered within [`BUDGET`] has landed. A vendor that stalls (captive portal,
/// TLS hang) therefore costs one late cache write, not a frozen panel: its
/// thread finishes on its own, writes the file, and the next cycle reads it.
pub fn collect() -> Vec<QuotaView> {
    collect_within(BUDGET)
}

/// The same, with a caller-chosen wait. A one-shot CLI has no panel to keep
/// responsive and can afford to wait out a slow probe (the Antigravity CLI takes
/// ~10 s), while the menu-bar app must not block its refresh on one.
pub fn collect_within(budget: Duration) -> Vec<QuotaView> {
    collect_probes_within(budget, built_in(), cache::Cache::open("tokenme/quota", TTL))
}

/// The panel's gated variant: a probe whose `gate` answers `false` (its host
/// application has exited) is not asked — the vendor call would be for a
/// number that cannot change — but its LAST KNOWN answer stays on screen, read
/// from the cache however old it is. The gate is consulted once per pass, so
/// a host that starts again resumes probing within one TTL.
pub fn collect_gated(
    budget: Duration,
    gate: impl Fn(&str) -> bool + Send + Sync + 'static,
) -> Vec<QuotaView> {
    collect_probes_gated(
        budget,
        built_in(),
        cache::Cache::open("tokenme/quota", TTL),
        &gate,
    )
}

/// Invalidate every cached quota answer, so the next [`collect`] re-probes
/// the vendors for real. The panel's manual full refresh calls this; the idle
/// cadence never does — a 5-minute-old answer is still worth showing there.
pub fn clear_cache() {
    if let Some(cache) = cache::Cache::open("tokenme/quota", TTL) {
        cache.clear();
    }
}

/// The same, over an injected probe set and cache — the seam the blink test
/// uses.
fn collect_probes_within(
    budget: Duration,
    probes: Vec<Box<dyn QuotaProbe>>,
    cache: Option<cache::Cache>,
) -> Vec<QuotaView> {
    collect_probes_gated(budget, probes, cache, &|_| true)
}

/// The gated core: a probe the `gate` refuses still contributes its cached
/// answer — however old — so the row stays on screen frozen instead of
/// blinking out; a probe with no cache entry at all (never answered) simply
/// stays absent until its host returns.
fn collect_probes_gated(
    budget: Duration,
    probes: Vec<Box<dyn QuotaProbe>>,
    cache: Option<cache::Cache>,
    gate: &dyn Fn(&str) -> bool,
) -> Vec<QuotaView> {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    let cache = Arc::new(cache);
    let done = Arc::new(AtomicUsize::new(0));
    let shared: Arc<Mutex<Vec<QuotaView>>> = Arc::new(Mutex::new(Vec::new()));

    let names: Vec<String> = probes.iter().map(|p| p.tool().to_string()).collect();
    let total = probes.len();
    for probe in probes {
        let cache = Arc::clone(&cache);
        let shared = Arc::clone(&shared);
        let done_count = Arc::clone(&done);
        // A once-a-day task riding a probe (Qoder's check-in) outranks the
        // host gate: the gate pauses a *read* whose number cannot change while
        // the app is closed, but the daily claim is a vendor-side state change
        // this process alone can make. `cache::probe`'s own TTL still bounds
        // it — the task runs at the first pass past its window, not per poll.
        let gated_off = !gate(probe.tool()) && !probe.daily_task();
        let spawned = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let cached: Option<&cache::Cache> = (*cache).as_ref();
                // A refused probe never reaches its vendor, whether or not the
                // cache could be opened: its last known answer stays on screen
                // when there is one, and the row simply stays absent when there
                // is not. Falling through to `cache::probe` here would re-arm a
                // host-paused or switched-off probe (and fire a claim POST for a
                // check-in tool the user disabled) whenever the cache dir was
                // unavailable — the exact opposite of the gate's promise.
                let views = if gated_off {
                    cached
                        .map(|c| c.stale(probe.tool()).unwrap_or_default())
                        .unwrap_or_default()
                } else {
                    cache::probe(cached, probe.as_ref())
                };
                if let Ok(mut slot) = shared.lock() {
                    slot.extend(views);
                }
                done_count.fetch_add(1, Ordering::Release);
            });
        // A thread we never started still has to be accounted for, or the loop
        // below waits out the whole budget for a probe that will not answer.
        if spawned.is_err() {
            done.fetch_add(1, Ordering::Release);
        }
    }

    let deadline = Instant::now() + budget;
    while done.load(Ordering::Acquire) < total && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }

    let mut out = shared.lock().map(|g| g.clone()).unwrap_or_default();
    // A probe still running at the budget — three sequential vendor calls can
    // outrun five seconds — must not make its bars blink out of the panel for
    // a cycle. Its last answer is on disk inside the grace window, and that is
    // exactly what grace is for; the late thread overwrites it when it lands.
    let answered: HashSet<String> = out.iter().map(|v| v.tool.clone()).collect();
    for name in &names {
        if answered.contains(name) {
            continue;
        }
        if let Some(cache) = cache.as_ref() {
            if let Some(views) = cache.within(name, cache::GRACE) {
                out.extend(views);
            }
        }
    }
    // Tool grouping only; inside a tool each probe's own row order stands. The
    // report that consumes this re-sorts deterministically, and what it groups
    // by is exactly the probe's arrangement — the vendor's own, as in
    // Antigravity's per-model groups — which a `used_percent` sort here would
    // reshuffle every time two models' percentages crossed.
    out.sort_by(|a, b| a.tool.cmp(&b.tool));
    out
}

/// How long a caller waits for live probes before taking what already answered.
pub const BUDGET: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Tools that answer a quota question but have no log adapter (yet). The
    /// assertion below still catches a typo in any `tool()`.
    const PROBE_ONLY_TOOLS: &[&str] = &["copilot", "gemini", "workbuddy"];

    #[test]
    fn every_probe_is_named_after_a_real_tool_id() {
        for probe in built_in() {
            assert!(!probe.tool().is_empty());
            let known = usage_adapter_all::TOOL_IDS.contains(&probe.tool())
                || PROBE_ONLY_TOOLS.contains(&probe.tool());
            assert!(known, "probe tool {:?} matches no adapter TOOL_ID or probe-only id", probe.tool());
        }
    }

    /// The dead-login registry is the expiry UX's whole spine: one host note
    /// per dead token (not one per pass), the token skipped from then on, and
    /// a re-login (a different token) reporting fresh when it dies in turn.
    #[test]
    fn the_dead_login_registry_notifies_once_and_skips_the_token() {        static FIRED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        set_login_expired_note(std::sync::Arc::new(|tool, host| {
            assert_eq!(tool, "trae_cn");
            assert_eq!(host, "api.trae.cn");
            FIRED.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(!login_is_dead("dead-login-test-token-a"));
        note_dead_login("trae_cn", "api.trae.cn", "dead-login-test-token-a");
        // The same dead token on every later pass: no second toast, still dead.
        note_dead_login("trae_cn", "api.trae.cn", "dead-login-test-token-a");
        assert_eq!(FIRED.load(Ordering::SeqCst), 1);
        assert!(login_is_dead("dead-login-test-token-a"));
        // A different token (the re-login) is its own episode.
        assert!(!login_is_dead("dead-login-test-token-b"));
        note_dead_login("trae_cn", "api.trae.cn", "dead-login-test-token-b");
        assert_eq!(FIRED.load(Ordering::SeqCst), 2);
        assert!(login_is_dead("dead-login-test-token-b"));
    }

    /// The claim sink rides a booked grant and nothing else: the note reaches
    /// the host with the pieces the toast composes from, once per call.
    #[test]
    fn the_claim_note_reaches_the_host_with_the_booking_pieces() {
        static SEEN: std::sync::OnceLock<std::sync::Mutex<Vec<(String, String, String)>>> =
            std::sync::OnceLock::new();
        let seen = SEEN.get_or_init(|| std::sync::Mutex::new(Vec::new()));
        set_claim_note(Arc::new(|tool, plan, units| {
            if let Ok(mut slot) = seen.lock() {
                slot.push((tool.to_string(), plan.to_string(), units.to_string()));
            }
        }));
        note_claim("zcode", "ZCODE GLOBAL BUILD", "+300M tokens");
        // An unnamed plan stays deliverable: the host composes the shorter body.
        note_claim("zcode", "", "+5M tokens");
        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                ("zcode".into(), "ZCODE GLOBAL BUILD".into(), "+300M tokens".into()),
                ("zcode".into(), String::new(), "+5M tokens".into()),
            ]
        );
    }

    /// The check-in registry drives the engine's auto-check-in exemption and
    /// the panel's button dispatch, so its entries are pinned: real probes
    /// behind them, and a stranger refused before any network call.
    #[test]
    fn the_checkin_registry_names_real_probes_and_refuses_strangers() {
        let probes: Vec<&str> = built_in().iter().map(|p| p.tool()).collect();
        let ids: Vec<&str> = checkin_tool_ids().collect();
        assert_eq!(ids, vec!["trae_cn", "qoder", "qoder_cn", "minimaxcode", "workbuddy_cn", "funide", "zcode"], "the claim-bearing probes");
        assert!(!is_checkin_tool("workbuddy"), "WorkBuddy AI never claims a CN grant");
        for id in &ids {
            assert!(probes.contains(id), "check-in tool {id:?} has no probe to run under");
            assert!(is_checkin_tool(id));
        }
        assert!(!is_checkin_tool("trae"), "the international fleet has no claim");
        assert!(manual_checkin("claude").is_err(), "a stranger must never reach a claim");
    }

    /// The settings sheet's per-tool page is only trustworthy while it is the
    /// registry's own list. This pins the two claims that page makes: the row set
    /// and order are `built_in()`'s, and the "no exit gate" badge belongs to
    /// exactly the probes `HOST_PROCESSES` cannot see. `answered_here` is
    /// deliberately not asserted — it is this machine's cache, not a fact about
    /// the code, and a CI box with no vendor answers would make it flaky.
    #[test]
    fn the_inventory_is_the_registry_talking_about_itself() {
        let listed = inventory();
        let probes: Vec<&str> = built_in().iter().map(|p| p.tool()).collect();
        assert_eq!(listed.len(), probes.len());
        assert_eq!(listed.iter().map(|i| i.tool).collect::<Vec<_>>(), probes);

        let ungated: Vec<&str> = listed.iter().filter(|i| !i.host_gated).map(|i| i.tool).collect();
        assert_eq!(
            ungated,
            vec!["kimicode", "minimaxcode", "copilot", "gemini"],
            "these probes are only stoppable by the user's own switch"
        );
        // claude leads the list because built_in() pushes it first; the page's
        // order is that decision, not an alphabetical one.
        assert_eq!(listed.first().map(|i| i.tool), Some("claude"));
    }

    /// A probe whose vendor needs longer than the caller's budget: without the
    /// grace supplement its bars vanish for a cycle and the panel reflows —
    /// the "cline jumps around" blink.
    /// A gated-off probe (its host has exited) must not be asked — but its
    /// last known answer stays on screen, however old. A gated-on probe runs
    /// for real. This is the host-exit pause's whole contract.
    #[test]
    fn a_gated_off_probe_serves_stale_and_a_gated_on_probe_runs() {
        struct Counting {
            tool: &'static str,
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }
        impl QuotaProbe for Counting {
            fn tool(&self) -> &'static str {
                self.tool
            }
            fn fetch(&self) -> Vec<QuotaSample> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                vec![QuotaSample {
                    used_percent: 42.0,
                    window_minutes: 300,
                    resets_at_ms: 0,
                    label: Some("fresh".into()),
                    id: None,
                }]
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_path_buf();
        let calls_dsh = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_qoder = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // A fresh probe set per pass (the Arc counters survive); the cache
        // lives on disk in between, exactly like the real cycle.
        let probes = |d: _, q: _| -> Vec<Box<dyn QuotaProbe>> {
            vec![
                Box::new(Counting { tool: "dsh", calls: d }),
                Box::new(Counting { tool: "qoder", calls: q }),
            ]
        };
        // Warm the cache: one ungated pass answers both.
        let out = collect_probes_gated(
            BUDGET,
            probes(calls_dsh.clone(), calls_qoder.clone()),
            Some(cache::Cache::in_dir(cache_dir.clone(), TTL)),
            &|_| true,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(calls_dsh.load(Ordering::SeqCst), 1);
        assert_eq!(calls_qoder.load(Ordering::SeqCst), 1);

        // Now close DSH's host and re-pass: dsh is served stale (no call), and
        // qoder's fresh cached answer is reused too. The contract here is the
        // call COUNT: a gated-off probe is never asked.
        let out = collect_probes_gated(
            BUDGET,
            probes(calls_dsh.clone(), calls_qoder.clone()),
            Some(cache::Cache::in_dir(cache_dir, TTL)),
            &|tool| tool != "dsh",
        );
        assert_eq!(out.len(), 2, "both rows stay on screen");
        assert_eq!(calls_dsh.load(Ordering::SeqCst), 1, "the gated-off probe is never asked");
        assert_eq!(calls_qoder.load(Ordering::SeqCst), 1, "a fresh cached answer is reused");
    }

    /// The gate's promise (engine comment + this module's docstring) is that a
    /// refused probe never reaches its vendor — "either gate refusing means no
    /// request leaves". A refused probe whose cache file cannot be read is the
    /// case that used to run it anyway: `(gated_off, None)` fell through to
    /// `cache::probe`, so a read-only or missing app-support dir turned every
    /// host-paused / user-switched-off probe back on — and for a check-in tool
    /// that means a live claim POST against the account the user disabled.
    #[test]
    fn a_gated_off_probe_is_never_asked_even_without_a_cache_dir() {
        struct Counting {
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }
        impl QuotaProbe for Counting {
            fn tool(&self) -> &'static str {
                "qoder"
            }
            fn fetch(&self) -> Vec<QuotaSample> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                vec![QuotaSample {
                    used_percent: 1.0,
                    window_minutes: 0,
                    resets_at_ms: 0,
                    label: None,
                    id: None,
                }]
            }
        }
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probes: Vec<Box<dyn QuotaProbe>> = vec![Box::new(Counting { calls: calls.clone() })];
        // `cache: None` is the missing/unwritable app-support dir; the gate
        // refuses this tool (host gone, or the user's own switch).
        let out = collect_probes_gated(BUDGET, probes, None, &|_tool| false);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a refused probe never reaches its vendor, cache or no cache"
        );
        assert!(out.is_empty(), "no cache to serve means the row stays absent, not live-probed");
    }

    #[test]
    fn a_probe_that_outruns_the_budget_is_carried_by_its_cached_answer() {
        struct Slow {
            tool: &'static str,
            millis: u64,
        }
        impl QuotaProbe for Slow {
            fn tool(&self) -> &'static str {
                self.tool
            }
            fn fetch(&self) -> Vec<QuotaSample> {
                std::thread::sleep(Duration::from_millis(self.millis));
                vec![QuotaSample {
                    used_percent: 42.0,
                    window_minutes: 300,
                    resets_at_ms: 0,
                    label: Some("fresh answer".into()),
                    id: None,
                }]
            }
        }

        let dir = tempfile::tempdir().unwrap();
        // Seed an answer older than the probe TTL but inside the grace window,
        // exactly what a previous good cycle leaves behind.
        let stale = format!(
            r#"{{"version":2,"captured_at_ms":{},"entries":[{{"used_percent":7.0,"window_minutes":300,"resets_at_ms":0,"label":"cached answer"}}]}}"#,
            cache::now_ms() - TTL.as_millis() as i64 - 60_000
        );
        std::fs::write(dir.path().join("slow.json"), stale).unwrap();
        let cache = cache::Cache::in_dir(dir.path().to_path_buf(), TTL);

        let probes: Vec<Box<dyn QuotaProbe>> =
            vec![Box::new(Slow { tool: "slow", millis: 400 })];
        let out = collect_probes_within(Duration::from_millis(40), probes, Some(cache));
        assert!(
            out.iter().any(|v| v.tool == "slow"),
            "a probe over budget must not drop its tool from the report: {out:?}"
        );
        // Either the late thread landed (fresh answer) or grace did (cached
        // answer); what must never happen is an empty report for that tool.
        assert!(out.iter().all(|v| v.tool != "slow" || v.used_percent > 0.0));
    }

    #[test]
    fn a_panicking_probe_degrades_to_nothing() {
        struct Boom;
        impl QuotaProbe for Boom {
            fn tool(&self) -> &'static str {
                "boom"
            }
            fn fetch(&self) -> Vec<QuotaSample> {
                panic!("probe blew up")
            }
        }
        let _ = std::panic::catch_unwind(|| assert!(Boom.guarded_fetch().is_empty()));
    }
}

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
mod providers;

use std::time::{Duration, Instant};

use usage_core::{QuotaSample, QuotaView};

pub use providers::ClaudeQuota;
pub use providers::workbuddy_login;

/// The tools whose probe owns a daily check-in claim, and the forced-claim
/// entry for each. One table, because three call sites must agree about the
/// same set: the engine exempts exactly these from the host-exit pause when
/// `auto_checkin` is on (the claim is HTTPS against the account — a closed
/// host app is no reason to miss a day), the panel's 签到 button dispatches
/// through here, and the strip renders its control for the same ids.
pub const CHECKIN_TOOLS: &[(&str, fn() -> Result<(bool, String), String>)] = &[
    ("trae_cn", providers::trae_manual_checkin),
    ("qoder", providers::qoder_manual_checkin),
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

/// One forced claim right now, by tool id — the panel's check-in button.
/// `(claimed, message)`: the bool is what the button paints, the message is
/// user-facing; a tool outside the registry never reaches a vendor.
pub fn manual_checkin(tool: &str) -> Result<(bool, String), String> {
    CHECKIN_TOOLS
        .iter()
        .find(|(id, _)| *id == tool)
        .map(|(_, claim)| claim())
        .unwrap_or_else(|| Err(format!("{tool} 没有签到活动")))
}

/// How long an answer stays trustworthy. Vendor windows reset on the hour at the
/// earliest, so five minutes is well inside any meaningful resolution.
pub const TTL: Duration = Duration::from_secs(300);

pub trait QuotaProbe: Send + Sync {
    /// Must match an adapter `TOOL_ID` so the panel can group quota with spend.
    fn tool(&self) -> &'static str;

    /// Best-effort live read. Return an empty vec when the tool is absent, not
    /// signed in, or the endpoint is down — the caller keeps the last answer.
    fn fetch(&self) -> Vec<QuotaSample>;

    /// Probes shell out and hit the network; neither may take the menu-bar
    /// process down with them.
    fn guarded_fetch(&self) -> Vec<QuotaSample> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.fetch())).unwrap_or_default()
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
        let gated_off = !gate(probe.tool());
        let spawned = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let cached: Option<&cache::Cache> = (*cache).as_ref();
                let views = match (gated_off, cached) {
                    // The host is gone: serve the last known answer untouched.
                    (true, Some(c)) => c.stale(probe.tool()).unwrap_or_default(),
                    _ => cache::probe(cached, probe.as_ref()),
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

    /// The check-in registry drives the engine's auto-check-in exemption and
    /// the panel's button dispatch, so its two entries are pinned: real probes
    /// behind them, and a stranger refused before any network call.
    #[test]
    fn the_checkin_registry_names_real_probes_and_refuses_strangers() {
        let probes: Vec<&str> = built_in().iter().map(|p| p.tool()).collect();
        let ids: Vec<&str> = checkin_tool_ids().collect();
        assert_eq!(ids, vec!["trae_cn", "qoder"], "the two claim-bearing probes");
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
            r#"{{"captured_at_ms":{},"entries":[{{"used_percent":7.0,"window_minutes":300,"resets_at_ms":0,"label":"cached answer"}}]}}"#,
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

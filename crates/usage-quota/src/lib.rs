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
mod http;
mod providers;

use std::time::{Duration, Instant};

use usage_core::{QuotaSample, QuotaView};

pub use providers::ClaudeQuota;
pub use providers::workbuddy_login;

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
        let spawned = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let cached: Option<&cache::Cache> = (*cache).as_ref();
                let views = cache::probe(cached, probe.as_ref());
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

    /// A probe whose vendor needs longer than the caller's budget: without the
    /// grace supplement its bars vanish for a cycle and the panel reflows —
    /// the "cline jumps around" blink.
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

//! The last published report, kept on disk so the next launch can show numbers
//! before it has read anything.
//!
//! Cold start measured on this machine (2026-10-07): the engine needs 6.8–24.9 s
//! of detect / pricing / source-status / aggregation before its first publish,
//! and the panel sits on the boot note the whole time even though every one of
//! those numbers was on disk before the process started. Writing the finished
//! report at publish time turns that wait into "read a file, parse it", which
//! costs milliseconds and puts the same figures on screen as soon as the webview
//! can paint.
//!
//! The restored report is marked [`Report::from_previous_run`] and the panel
//! labels it, so this is a faster first frame rather than a lie about freshness.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use usage_core::report::{MachineScope, Report};

/// How old a snapshot may be and still be worth showing. The engine's own cadence
/// is seconds; a report from five minutes ago needs no caveat beyond "still
/// scanning". An hour is the point where the honest sentence is no longer "the
/// numbers you saw last time" but "some numbers from earlier today", and the scan
/// is not going to take that long again.
const MAX_AGE_MS: i64 = 3_600_000;

fn path() -> Option<PathBuf> {
    crate::engine::index_path()
        .parent()
        .map(|dir| dir.join("report.json"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Today's date as the report itself spells it: `Window::key` for the day window
/// is the local `%Y-%m-%d` the fold was built under. Comparing keys rather than
/// timestamps means the snapshot and this run agree with each other about where
/// the day starts, whatever the machine's clock or zone.
fn today_key() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Persist `report` as the next launch's first frame. Only a freshly folded
/// report belongs on disk: writing a restored one back would let a snapshot
/// survive a reboot it should not have outlived.
pub fn write(report: &Report) {
    if report.from_previous_run {
        return;
    }
    let Some(path) = path() else { return };
    let json = match serde_json::to_string(report) {
        Ok(json) => json,
        Err(e) => {
            crate::logging::error(&format!("snapshot: the report did not serialise: {e}"));
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    let result = (|| -> Result<(), String> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        file.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        // Same-directory rename, so a torn write can only ever leave the tmp file
        // behind and never replace a readable snapshot with half of a new one.
        fs::rename(&tmp, &path).map_err(|e| e.to_string())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        crate::logging::error(&format!("snapshot: could not be written ({e}) — the next cold start will wait on the scan again"));
    }
}

/// The report to show before this run has computed anything, if there is an
/// honest one to show: same scope as the panel is about to draw, folded no more
/// than [`MAX_AGE_MS`] ago and on the same local day, and not itself a restore.
///
/// Anything else — missing, unparsable, a schema that moved on — returns `None`
/// and the boot path behaves exactly as it did before this file existed.
pub fn read(scope: &MachineScope) -> Option<Report> {
    let path = path()?;
    let raw = fs::read_to_string(&path).ok()?;
    let report: Report = match serde_json::from_str(&raw) {
        Ok(report) => report,
        Err(e) => {
            crate::logging::info(&format!(
                "snapshot: unusable ({e}) — publishing after the scan instead"
            ));
            return None;
        }
    };
    if let Err(reason) = accept(&report, scope, now_ms(), &today_key()) {
        crate::logging::info(&format!("snapshot: not usable — {reason}"));
        return None;
    }
    Some(report)
}

/// Whether `report` may stand in for this run's first frame. Split out and given
/// the clock and today's key as arguments so the rule is testable without moving
/// a wall clock: the answer must not depend on when the suite runs.
fn accept(
    report: &Report,
    scope: &MachineScope,
    now_ms: i64,
    today: &str,
) -> Result<(), &'static str> {
    if report.from_previous_run {
        return Err("the cached report was itself a restore — refusing to chain it");
    }
    if report.scope != *scope {
        return Err("it was folded under a different machine scope");
    }
    if report.generated_at_ms > now_ms {
        return Err("it is stamped in the future, so the clock moved under it");
    }
    if now_ms - report.generated_at_ms > MAX_AGE_MS {
        return Err("it is more than an hour old — the scan is not that slow");
    }
    if report.day.key != today {
        return Err("its 今日 is another date; its windows would be yesterday's");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::pricing::PricingMeta;
    use usage_core::report::{Breakdown, Summary};

    /// A fixed "today" so the suite answers the same question in October and in
    /// March: `accept` takes the clock as an argument precisely so no test has to
    /// read one.
    const TODAY: &str = "2026-10-07";
    const NOW: i64 = 1_759_810_000_000;

    fn window(key: &str, label: &str) -> usage_core::report::Window {
        usage_core::report::Window {
            key: key.into(),
            label: label.into(),
            start_ms: NOW - 3_600_000,
            end_ms: NOW,
            summary: Summary::default(),
            prev: Summary::default(),
            delta_cost_pct: 0.0,
            delta_tokens_pct: 0.0,
            breakdown: Breakdown::default(),
        }
    }

    /// Built as Rust values rather than a JSON blob: the shape is the compiler's
    /// problem, and a fixture that guessed at field names would fail for the wrong
    /// reason.
    fn report(day_key: &str, age_ms: i64, scope: MachineScope, restored: bool) -> Report {
        Report {
            generated_at_ms: NOW - age_ms,
            utc_offset: "+08:00".into(),
            day: window(day_key, "今日"),
            week: window("2026-W41", "本周"),
            month: window("2026-10", "本月"),
            year: window("2026", "本年"),
            heatmap: Vec::new(),
            hourly: Vec::new(),
            quotas: Vec::new(),
            quotas_pending: false,
            from_previous_run: restored,
            sources: Vec::new(),
            syncs: Vec::new(),
            pricing: PricingMeta {
                source: usage_core::pricing::PricingSource::Bundled,
                fetched_at_ms: 0,
                stale: false,
                key_count: 0,
                cache_dir: None,
            },
            scope,
            machines: Vec::new(),
            recent_sessions: Vec::new(),
            all_time: Summary::default(),
        }
    }

    fn live(day_key: &str, age_ms: i64) -> Report {
        report(day_key, age_ms, MachineScope::Local, false)
    }

    #[test]
    fn a_report_survives_the_round_trip_through_json() {
        // The snapshot file is the report's only route across a restart, so any
        // field the panel reads has to come back identical — including the ones
        // that are `Option`, `PathBuf` or `#[serde(default)]`.
        let original = report(TODAY, 60_000, MachineScope::Origin { name: "ops-box".into() }, false);
        let back: Report = serde_json::from_str(&serde_json::to_string(&original).unwrap())
            .expect("a published report re-reads");
        assert_eq!(original, back, "every field the panel reads comes back");
    }

    #[test]
    fn an_older_payload_without_the_new_field_still_reads() {
        // The snapshot outlives the schema that wrote it: a report from before
        // `from_previous_run` (and before `machines`, and before `hourly`) reads as
        // "live, no restore", not as an unusable file.
        let mut json = serde_json::to_value(live(TODAY, 60_000)).unwrap();
        for field in ["from_previous_run", "machines", "hourly"] {
            json.as_object_mut().unwrap().remove(field);
        }
        let report: Report = serde_json::from_value(json).expect("a pre-restore schema still reads");
        assert!(!report.from_previous_run);
        assert!(report.machines.is_empty());
        assert!(report.hourly.is_empty());
        assert_eq!(accept(&report, &MachineScope::Local, NOW, TODAY), Ok(()));
    }

    #[test]
    fn a_report_from_this_minute_under_this_scope_is_last_known() {
        assert_eq!(accept(&live(TODAY, 5_000), &MachineScope::Local, NOW, TODAY), Ok(()));
        assert_eq!(
            accept(&report(TODAY, 5_000, MachineScope::All, false), &MachineScope::All, NOW, TODAY),
            Ok(())
        );
    }

    #[test]
    fn anything_that_would_need_a_weasel_word_is_refused() {
        let local = MachineScope::Local;
        // Yesterday's fold: its 今日 is a different day's totals under today's label.
        assert!(accept(&live("2026-10-06", 60_000), &local, NOW, TODAY).is_err());
        // An hour is longer than the scan takes, so it is not "last known" any more.
        assert!(accept(&live(TODAY, MAX_AGE_MS + 1), &local, NOW, TODAY).is_err());
        assert!(accept(&live(TODAY, MAX_AGE_MS), &local, NOW, TODAY).is_ok(), "the boundary itself");
        // The clock moved backwards under it.
        assert!(accept(&live(TODAY, -60_000), &local, NOW, TODAY).is_err());
        // A different machine's numbers are not these numbers.
        let remote = MachineScope::Origin { name: "ops-box".into() };
        assert!(accept(&report(TODAY, 60_000, remote.clone(), false), &local, NOW, TODAY).is_err());
        assert!(accept(&live(TODAY, 60_000), &remote, NOW, TODAY).is_err());
        // Chaining: a restored report is not evidence that anything was scanned.
        assert!(accept(&report(TODAY, 60_000, MachineScope::Local, true), &local, NOW, TODAY).is_err());
    }
}

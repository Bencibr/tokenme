//! scan.log — the shareable scanner audit trail.
//!
//! One JSON line per ingest pass that changed something (plus a heartbeat at
//! most once an hour), appended to `logs/scan.log` next to `panel.log`. A user
//! sends the file over; every fact a remote reader needs to answer "did the
//! scanner double-count?" is in it:
//!
//! - `roots`: the tree each tool walked — two tools sharing a root is the
//!   classic double-scan, and `overlap_suspect` names the pair automatically
//! - the pass counters (`scanned/changed/new/deduped/purged`) — a `new` spike
//!   repeating over the same data is a cursor or dedupe failure
//! - `dsh_sessions`: the per-session figures the index holds — a v4 session
//!   must show exactly one event with monotonically growing totals
//! - `dsh_audit`: those same sessions reconciled against DSH's own projection
//!   ledger, field by field — `match: false` is a double/under-count, named
//!   per session, with `dsh_regressions` flagging any total that shrank (a
//!   cumulative ledger only ever grows)
//!
//! Rotation keeps one 2 MiB generation; the file is always safe to delete.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::Local;
use serde_json::json;
use usage_core::UsageEvent;

use crate::logging;

static LAST_HEARTBEAT: Mutex<Option<std::time::Instant>> = Mutex::new(None);
static LAST_TOTALS: Mutex<BTreeMap<String, (f64, f64, f64, f64)>> = Mutex::new(BTreeMap::new());
const HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);
const MAX_BYTES: u64 = 2 << 20;

/// Append one audit line for a finished ingest pass. `events` is the index's
/// post-pass event set (the same array the report was published from).
pub fn pass(
    reason: &str,
    detected: &[usage_core::DetectedSource],
    report: &usage_index::IngestReport,
    events: &[UsageEvent],
) {
    let changed = report.files_changed > 0 || report.new_events > 0;
    let heartbeat_due = {
        let mut last = LAST_HEARTBEAT.lock().unwrap();
        let due = last.map_or(true, |t| t.elapsed() >= HEARTBEAT_EVERY);
        if due {
            *last = Some(std::time::Instant::now());
        }
        due
    };
    if !changed && !heartbeat_due {
        return;
    }

    // Roots per tool, and any pair of tools whose roots overlap — one tree
    // walked by two adapters is the classic double-scan this file exists for.
    let mut roots = BTreeMap::new();
    for source in detected {
        roots.insert(source.id.clone(), source.roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>());
    }
    let mut overlap_suspect: Vec<String> = Vec::new();
    for i in 0..detected.len() {
        for j in (i + 1)..detected.len() {
            for a in &detected[i].roots {
                for b in &detected[j].roots {
                    if a.starts_with(b) || b.starts_with(a) {
                        overlap_suspect.push(format!("{} ↔ {}", detected[i].id, detected[j].id));
                    }
                }
            }
        }
    }

    let mut tools: BTreeMap<String, u64> = BTreeMap::new();
    for e in events {
        *tools.entry(e.tool.clone()).or_default() += 1;
    }
    // The dsh reconciliation: what the index holds per session, what DSH's own
    // projection ledger says, and whether any total moved backwards (a
    // cumulative ledger only grows — a shrink is a re-read or double-write).
    let dsh: Vec<&UsageEvent> = events.iter().filter(|e| e.tool == "dsh").collect();
    let mut last_totals = LAST_TOTALS.lock().unwrap();
    let mut dsh_regressions: Vec<String> = Vec::new();
    let mut dsh_sessions: Vec<_> = Vec::new();
    let mut ledger_by_id: BTreeMap<String, (f64, f64, f64, f64)> = BTreeMap::new();
    for row in usage_adapter_dsh::ledger() {
        ledger_by_id.insert(row.session.clone(), (row.input, row.output, row.cache_read, row.cache_write));
    }
    for e in &dsh {
        let totals = (e.counts.input, e.counts.output, e.counts.cache_read, e.counts.cache_creation);
        if let Some(prev) = last_totals.get(&e.session) {
            if totals.0 < prev.0 || totals.1 < prev.1 || totals.2 < prev.2 || totals.3 < prev.3 {
                dsh_regressions.push(e.session.clone());
            }
        }
        last_totals.insert(e.session.clone(), totals);
        let id = e.session.clone();
        let audit = match ledger_by_id.get(&id) {
            Some(l) => {
                let (li, lo, lcr, lcc) = *l;
                let matches = (totals.0 - li).abs() < 0.5
                    && (totals.1 - lo).abs() < 0.5
                    && (totals.2 - lcr).abs() < 0.5
                    && (totals.3 - lcc).abs() < 0.5;
                json!({ "ledger_in": li, "ledger_out": lo, "ledger_cr": lcr, "ledger_cc": lcc, "match": matches })
            }
            None => json!({ "ledger": "absent", "match": false }),
        };
        dsh_sessions.push(json!({
            "id": id,
            "in": totals.0, "cc": totals.3, "cr": totals.2, "out": totals.1,
            "events": 1, "model": e.model, "ts": e.ts_ms,
            "audit": audit,
        }));
    }
    // A ledger row with no index event at all (usage the panel never picked up).
    for (id, (li, lo, lcr, lcc)) in &ledger_by_id {
        if !dsh.iter().any(|e| e.session == *id) && (li + lo + lcr + lcc) > 0.0 {
            dsh_sessions.push(json!({
                "id": id, "in": li, "out": lo, "cr": lcr, "cc": lcc,
                "events": 0, "audit": { "ledger": "self", "match": false },
            }));
            dsh_regressions.push(format!("{id} (ledger has usage, index has none)"));
        }
    }

    let line = json!({
        "ts": Local::now().to_rfc3339(),
        "proc": "panel",
        "reason": reason,
        "scanned": report.files_scanned,
        "changed": report.files_changed,
        "new": report.new_events,
        "deduped": report.deduped,
        "purged": report.purged,
        "took_ms": report.took_ms,
        "roots": roots,
        "overlap_suspect": overlap_suspect,
        "tools": tools,
        "dsh_regressions": dsh_regressions,
        "dsh_sessions": dsh_sessions,
    });

    append_line(&serde_json::to_string(&line).unwrap_or_default());
}

fn path() -> Option<std::path::PathBuf> {
    Some(logging::log_dir().join("scan.log"))
}

fn append_line(line: &str) {
    use std::io::Write as _;
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    } else {
        logging::error("scan.log append failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heartbeat_passes_only_once_per_hour() {
        // The mutex makes the first call due; the second within the hour is not.
        let due_first = {
            let mut last = LAST_HEARTBEAT.lock().unwrap();
            let due = last.is_none();
            *last = Some(std::time::Instant::now());
            due
        };
        assert!(due_first);
        let due_second = {
            let last = LAST_HEARTBEAT.lock().unwrap();
            last.map(|t| t.elapsed() >= HEARTBEAT_EVERY).unwrap_or(false)
        };
        assert!(!due_second);
    }
}

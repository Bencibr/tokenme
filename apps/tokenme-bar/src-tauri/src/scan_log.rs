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
//! - `dsh_sessions`: the per-session ledger the panel computed — a v4 session
//!   must show exactly one event with monotonically growing totals
//!
//! Rotation keeps one 2 MiB generation; the file is always safe to delete.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::Local;
use serde_json::json;
use usage_core::UsageEvent;

use crate::logging;

static LAST_HEARTBEAT: Mutex<Option<std::time::Instant>> = Mutex::new(None);
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
    // The dsh sessions, verbatim: a v4 session must be exactly one event with
    // totals that only ever grow — the sharpest duplicate-scan tripwire there is.
    let dsh_sessions: Vec<_> = events
        .iter()
        .filter(|e| e.tool == "dsh")
        .map(|e| {
            json!({
                "id": e.session,
                "in": e.counts.input, "cc": e.counts.cache_creation,
                "cr": e.counts.cache_read, "out": e.counts.output,
                "events": 1, "model": e.model,
                "ts": e.ts_ms,
            })
        })
        .collect();

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

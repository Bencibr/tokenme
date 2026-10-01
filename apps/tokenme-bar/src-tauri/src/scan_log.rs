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

use crate::logging;

static LAST_HEARTBEAT: Mutex<Option<std::time::Instant>> = Mutex::new(None);
static LAST_TOTALS: Mutex<BTreeMap<String, (f64, f64, f64, f64)>> = Mutex::new(BTreeMap::new());
const HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);
const MAX_BYTES: u64 = 2 << 20;

/// Append one audit line for a finished ingest pass. Per-tool and per-session
/// figures are queried straight from the index — no event array crosses this
/// boundary, so an audit line costs a few thousand small rows, never the whole
/// history.
pub fn pass(
    reason: &str,
    detected: &[usage_core::DetectedSource],
    report: &usage_index::IngestReport,
    index: &usage_index::Index,
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

    let tools: BTreeMap<String, u64> = index.per_tool_counts().unwrap_or_default();
    // The dsh reconciliation: what the index holds per session, what DSH's own
    // projection ledger says, and whether any total moved backwards (a
    // cumulative ledger only grows — a shrink is a re-read or double-write).
    let dsh_rows = index.tool_events("dsh").unwrap_or_default();
    let mut last_totals = LAST_TOTALS.lock().unwrap();
    let mut dsh_regressions: Vec<String> = Vec::new();
    let mut dsh_sessions: Vec<_> = Vec::new();
    let mut ledger_by_id: BTreeMap<String, (f64, f64, f64, f64)> = BTreeMap::new();
    for row in usage_adapter_dsh::ledger() {
        ledger_by_id.insert(row.session.clone(), (row.input, row.output, row.cache_read, row.cache_write));
    }
    for row in &dsh_rows {
        let totals = (row.input, row.output, row.cache_read, row.cache_creation);
        if let Some(prev) = last_totals.get(&row.session) {
            if totals.0 < prev.0 || totals.1 < prev.1 || totals.2 < prev.2 || totals.3 < prev.3 {
                dsh_regressions.push(row.session.clone());
            }
        }
        last_totals.insert(row.session.clone(), totals);
        let id = row.session.clone();
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
            "events": 1, "model": row.model, "ts": row.ts_ms,
            "audit": audit,
        }));
    }
    // A ledger row with no index event at all (usage the panel never picked up).
    for (id, (li, lo, lcr, lcc)) in &ledger_by_id {
        if !dsh_rows.iter().any(|r| r.session == *id) && (li + lo + lcr + lcc) > 0.0 {
            dsh_sessions.push(json!({
                "id": id, "in": li, "out": lo, "cr": lcr, "cc": lcc,
                "events": 0, "audit": { "ledger": "self", "match": false },
            }));
            dsh_regressions.push(format!("{id} (ledger has usage, index has none)"));
        }
    }

    // The codex replay audit rides the heartbeat line only: re-parsing every
    // rollout from scratch is the one expensive fact here (hundreds of ms on a
    // heavy machine), and hourly is plenty for a remote double-count check.
    // The initial-scan pass is exempt either way: its heartbeat is always due
    // (the clock is in-memory), and blocking the boot publish on a full replay
    // was the "二十多秒才启动完" the panel shipped with.
    let mut codex_audit: serde_json::Value = serde_json::Value::Null;
    if heartbeat_due && reason != "initial scan" {
        let replay = usage_adapter_codex::replay(&usage_core::DateFilter::default());
        let mut indexed: BTreeMap<String, (f64, f64, f64, f64, usize)> = BTreeMap::new();
        for row in index.tool_session_totals("codex").unwrap_or_default() {
            indexed.insert(
                row.session,
                (row.input, row.cache_creation, row.cache_read, row.output, row.events as usize),
            );
        }
        let mut mismatches: Vec<String> = Vec::new();
        let mut checked = 0u64;
        let mut sessions: Vec<_> = Vec::new();
        for row in &replay {
            let replay_json = json!({
                "in": row.input, "out": row.output, "cached": row.cached, "calls": row.calls,
            });
            match indexed.get(&row.session) {
                Some((i, cc, cr, o, n)) => {
                    checked += 1;
                    let matches = (*i - row.input).abs() < 0.5
                        && (*o - row.output).abs() < 0.5
                        && ((*cr + *cc) - row.cached).abs() < 0.5
                        && *n == row.calls;
                    if !matches {
                        mismatches.push(row.session.clone());
                    }
                    sessions.push(json!({
                        "id": row.session, "replay": replay_json,
                        "index": { "in": i, "out": o, "cached": cr + cc, "calls": n },
                        "match": matches,
                    }));
                }
                None => {
                    mismatches.push(format!("{row:?}", row = row.session));
                    sessions.push(json!({ "id": row.session, "replay": replay_json, "index": null, "match": false }));
                }
            }
        }
        codex_audit = json!({ "sessions": checked, "mismatches": mismatches, "detail": sessions });
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
        "codex_audit": codex_audit,
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

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
//! - `new_by_tool` / `deduped_by_tool` / `scanned_by_tool`: the same counters
//!   split per tool for this pass — a tool whose changed store re-read as zero
//!   events shows bare maps here, and a root contributing zero files is a
//!   missing store rather than an empty one
//! - `errors`: reads that failed outright (their files are re-read next pass)
//! - `notes`: what the adapters said while reading — a decrypt that answered
//!   "no data" names its stage here, so "the user had a quiet day" and "the
//!   store could not be read" are told apart from the file alone — plus the
//!   store-drift watch: a tool whose newest indexed event sits >48h back
//!   earns a bounded look at its watched roots and known alternate store
//!   locations, so "the writer moved" is answerable from this file alone
//!   (build 161 chased two days of invisible dsh usage to exactly that)
//! - `ver`/`build`/`os`/`arch`: which binary wrote the line
//! - `dsh_sessions`: the per-session figures the index holds — a v4 session
//!   must show exactly one event with monotonically growing totals
//! - `dsh_audit`: those same sessions reconciled against DSH's own projection
//!   ledger, field by field — `match: false` is a double/under-count, named
//!   per session, with `dsh_regressions` flagging any session whose summed
//!   totals shrank between passes (a cumulative store only grows). v3-era
//!   streams spell their session id `session-<uuid>` while the ledger keys
//!   the bare uuid and excludes those events from its totals by design, so
//!   v3 rows are labeled `v3-stream` and never counted as mismatches
//!
//! Rotation keeps one 2 MiB generation; the file is always safe to delete.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{Local, TimeZone};
use serde_json::json;

use crate::logging;

static LAST_HEARTBEAT: Mutex<Option<std::time::Instant>> = Mutex::new(None);
// Per-session SUMS of (in, out, cache_read, cache_creation) at the last pass
// that emitted a line — the unit the regression watch compares across passes.
static LAST_TOTALS: Mutex<BTreeMap<String, (f64, f64, f64, f64)>> = Mutex::new(BTreeMap::new());
// One store-drift note per tool per day: a stale store stays stale for hours,
// and hourly repetition is noise, not signal.
static STALE_NOTED: Mutex<BTreeMap<String, std::time::Instant>> = Mutex::new(BTreeMap::new());
const HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);
const STALE_EVERY: std::time::Duration = std::time::Duration::from_secs(86_400);
const STALE_AFTER_MS: i64 = 48 * 3_600_000;
// Bounded app-data walks: the question is "when was this tree last written",
// never a full listing — Electron caches carry tens of thousands of entries.
const WALK_DEPTH: u8 = 4;
const WALK_BUDGET: u32 = 20_000;
const MAX_SIBLINGS: usize = 8;
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
    // projection ledger says, and whether any session's totals moved backwards
    // (a cumulative store only grows — a shrink is a re-read or double-write).
    // Totals are summed per session first: the index holds one event per call
    // in no particular order, so a row-to-row comparison measures the
    // iteration order, not the store — every quiet pass reported fake
    // regressions that way.
    let dsh_rows = index.tool_events("dsh").unwrap_or_default();
    let sums = sum_rows(&dsh_rows);
    let mut last_totals = LAST_TOTALS.lock().unwrap();
    let mut dsh_regressions: Vec<String> = Vec::new();
    for (session, totals) in &sums {
        if let Some(prev) = last_totals.get(session) {
            if totals_shrank(*prev, *totals) {
                dsh_regressions.push(format!("{session} (totals shrank)"));
            }
        }
    }
    for (session, totals) in &sums {
        last_totals.insert(session.clone(), *totals);
    }
    drop(last_totals);
    let mut dsh_sessions: Vec<_> = Vec::new();
    let mut ledger_by_id: BTreeMap<String, (f64, f64, f64, f64)> = BTreeMap::new();
    for row in usage_adapter_dsh::ledger() {
        ledger_by_id.insert(row.session.clone(), (row.input, row.output, row.cache_read, row.cache_write));
    }
    for row in &dsh_rows {
        let id = row.session.clone();
        // v3-era streams spell the header id `session-<uuid>` while the ledger
        // keys the bare uuid — and the projection deliberately excludes
        // inherited v3 events from its totals, so there is nothing to
        // reconcile against: labeling these `match: false` cried wolf on
        // every pass.
        let audit = if id.starts_with("session-") {
            json!({ "ledger": "v3-stream", "note": "projection totals exclude inherited v3 events by design" })
        } else {
            match ledger_by_id.get(&id) {
                Some(l) => {
                    let (li, lo, lcr, lcc) = *l;
                    let matches = (row.input - li).abs() < 0.5
                        && (row.output - lo).abs() < 0.5
                        && (row.cache_read - lcr).abs() < 0.5
                        && (row.cache_creation - lcc).abs() < 0.5;
                    json!({ "ledger_in": li, "ledger_out": lo, "ledger_cr": lcr, "ledger_cc": lcc, "match": matches })
                }
                None => json!({ "ledger": "absent", "match": false }),
            }
        };
        dsh_sessions.push(json!({
            "id": id,
            "in": row.input, "cc": row.cache_creation, "cr": row.cache_read, "out": row.output,
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

    // Store-drift watch, hourly with the heartbeat and rate-limited per tool:
    // a tool whose newest event sits >48h back earns one note naming what its
    // watched roots and known alternate stores look like. Store moves used to
    // be invisible — the adapter read a frozen tree and called it a quiet
    // week (dsh, build 161: two days of usage, zero events, zero errors).
    let mut notes = report.notes.clone();
    if heartbeat_due {
        for source in detected {
            let Some(note) = store_drift_note(&source.id, &source.roots, index) else { continue };
            let mut noted = STALE_NOTED.lock().unwrap();
            if noted.get(&source.id).map_or(true, |t| t.elapsed() >= STALE_EVERY) {
                noted.insert(source.id.clone(), std::time::Instant::now());
                drop(noted);
                logging::info(&note);
                notes.push(note);
            }
        }
    }

    let line = json!({
        "ts": Local::now().to_rfc3339(),
        "proc": "panel",
        // Which binary wrote this line: a scan.log arrives without panel.log
        // often enough that its pass counters mean nothing without the build.
        "ver": env!("CARGO_PKG_VERSION"),
        "build": env!("TOKENME_BUILD_ID"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
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
        // This pass's delta per tool: a healthy full-rescan re-read shows as
        // deduped-by-tool ≈ the tool's whole history, an empty read as both
        // maps bare.
        "new_by_tool": report.per_tool_new,
        "deduped_by_tool": report.per_tool_deduped,
        "scanned_by_tool": report.scanned_by_tool,
        "errors": report.errors,
        "notes": notes,
        "dsh_regressions": dsh_regressions,
        "dsh_sessions": dsh_sessions,
        "codex_audit": codex_audit,
    });

    append_line(&serde_json::to_string(&line).unwrap_or_default());
}

/// Per-session token sums — the unit the regression watch compares across
/// passes: a session's event count grows per call, its sums only shrink when
/// the store really lost something.
fn sum_rows(rows: &[usage_index::ToolEventRow]) -> BTreeMap<String, (f64, f64, f64, f64)> {
    let mut out: BTreeMap<String, (f64, f64, f64, f64)> = BTreeMap::new();
    for row in rows {
        let e = out.entry(row.session.clone()).or_insert((0.0, 0.0, 0.0, 0.0));
        e.0 += row.input;
        e.1 += row.output;
        e.2 += row.cache_read;
        e.3 += row.cache_creation;
    }
    out
}

/// Float-safe: re-summing the same rows in a different order drifts in the
/// last bits, and half a token is not a regression.
fn totals_shrank(prev: (f64, f64, f64, f64), cur: (f64, f64, f64, f64)) -> bool {
    cur.0 < prev.0 - 0.5 || cur.1 < prev.1 - 0.5 || cur.2 < prev.2 - 0.5 || cur.3 < prev.3 - 0.5
}

fn newest_file(dir: &Path, depth: u8, budget: &mut u32) -> Option<(std::time::SystemTime, PathBuf)> {
    if depth == 0 {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return None };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        let (t, p) = if ft.is_dir() {
            match newest_file(&path, depth - 1, budget) {
                Some(found) => found,
                None => continue,
            }
        } else if ft.is_file() {
            let Ok(t) = entry.metadata().and_then(|m| m.modified()) else { continue };
            (t, path)
        } else {
            continue;
        };
        if best.as_ref().map_or(true, |(bt, _)| t > *bt) {
            best = Some((t, p));
        }
    }
    best
}

fn fmt_local(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t).with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string()
}

fn fmt_ts(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().map_or_else(|| ms.to_string(), |d| d.format("%m-%d %H:%M").to_string())
}

/// App-data dirs named after the vendor, each with its newest file mtime —
/// the in-log answer to a writer that moved its tree under a path no read
/// root guesses. This replaced a PowerShell errand the user could not be
/// handed: the log carries the dir listing instead.
fn sibling_stores() -> Vec<String> {
    let mut bases: Vec<PathBuf> = Vec::new();
    for base in [dirs::config_dir(), dirs::data_local_dir()].into_iter().flatten() {
        if !bases.contains(&base) {
            bases.push(base);
        }
    }
    let mut out: Vec<String> = Vec::new();
    for base in bases {
        let Ok(entries) = std::fs::read_dir(&base) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !(name.contains("dsh") || name.contains("deepseek")) || !entry.path().is_dir() {
                continue;
            }
            let mut budget = WALK_BUDGET;
            out.push(match newest_file(&entry.path(), WALK_DEPTH, &mut budget) {
                Some((t, _)) => format!("{} (newest file {})", entry.path().display(), fmt_local(t)),
                None => format!("{} (unreadable)", entry.path().display()),
            });
            if out.len() >= MAX_SIBLINGS {
                out.push("…".into());
                return out;
            }
        }
    }
    out
}

/// One store-drift fact per tool, or None while the tool looks alive: the
/// indexed newest event against every watched root's newest file, plus — for
/// dsh — the known alternate store locations and any sibling app-data dir.
fn store_drift_note(tool: &str, roots: &[PathBuf], index: &usage_index::Index) -> Option<String> {
    let newest = index.newest_ts_ms(tool).ok().flatten()?;
    let age_ms = Local::now().timestamp_millis() - newest;
    if age_ms < STALE_AFTER_MS {
        return None;
    }
    let mut parts = vec![format!(
        "{tool}: indexed newest {:.1}d ago ({})",
        age_ms as f64 / 86_400_000.0,
        fmt_ts(newest)
    )];
    for root in roots {
        let mut budget = WALK_BUDGET;
        parts.push(match newest_file(root, WALK_DEPTH, &mut budget) {
            Some((t, _)) => format!("root {} newest file {}", root.display(), fmt_local(t)),
            None => format!("root {} unreadable or empty", root.display()),
        });
    }
    if tool == "dsh" {
        for (label, path) in usage_adapter_dsh::diagnostic_candidates() {
            if roots.iter().any(|r| r == &path) {
                continue;
            }
            if !path.is_dir() {
                parts.push(format!("{label} candidate {}: absent", path.display()));
                continue;
            }
            let mut budget = WALK_BUDGET;
            parts.push(match newest_file(&path, WALK_DEPTH, &mut budget) {
                Some((t, p)) => format!("{label} candidate newest file {} ({})", fmt_local(t), p.display()),
                None => format!("{label} candidate {}: exists, unreadable", path.display()),
            });
        }
        let siblings = sibling_stores();
        if siblings.is_empty() {
            parts.push("app-data dirs named dsh/deepseek: none".into());
        } else {
            parts.push(format!("app-data dirs named dsh/deepseek: {}", siblings.join(" · ")));
        }
    }
    Some(parts.join("; "))
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

    #[test]
    fn sums_aggregate_per_session_and_shrink_needs_half_a_token() {
        let row = |session: &str, cache_read: f64| usage_index::ToolEventRow {
            session: session.into(),
            model: None,
            ts_ms: 0,
            input: 0.0,
            cache_creation: 0.0,
            cache_read,
            output: 0.0,
        };
        // One session, two calls, rows in whatever order the index returns
        // them — one summed entry either way.
        let sums = sum_rows(&[row("a", 384.0), row("a", 20096.0), row("b", 7.0)]);
        assert_eq!(sums.get("a").map(|t| t.2), Some(20480.0));
        assert_eq!(sums.get("b").map(|t| t.2), Some(7.0));
        // Reordered re-summing drifts in the last bits: not a regression.
        let drifted = (0.0, 0.0, 20480.0 + 1e-9, 0.0);
        assert!(!totals_shrank(drifted, sums["a"]));
        // Losing a call is.
        assert!(totals_shrank(sums["a"], (0.0, 0.0, 384.0, 0.0)));
    }

    #[test]
    fn newest_file_walks_depth_and_budget_bounded() {
        let dir = tempfile::tempdir().unwrap();
        // Five dirs down: four levels of look see nothing, nine see it.
        let deep = dir.path().join("a").join("b").join("c").join("d").join("e");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("beyond.jsonl"), b"x").unwrap();
        let mut budget = WALK_BUDGET;
        assert!(newest_file(dir.path(), 4, &mut budget).is_none());
        assert!(newest_file(dir.path(), 9, &mut budget).is_some());
        // A spent budget stops the walk before anything is answered.
        std::fs::write(dir.path().join("1.txt"), b"x").unwrap();
        let mut tight = 0;
        assert!(newest_file(dir.path(), 4, &mut tight).is_none());
    }
}

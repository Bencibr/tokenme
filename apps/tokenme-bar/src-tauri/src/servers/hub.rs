//! The pull scheduler: one async task that wakes when a server is due, runs
//! the fetch pipeline, records the outcome, and backs off on failure.
//!
//! It lives on Tauri's tokio runtime. Every pull is sequential — a handful of
//! servers is the expected fleet, and one slow export must not stack memory
//! from five concurrent ones. Store edits arrive as `HubMsg`s so "sync now"
//! and enable/disable take effect within a tick instead of after the sleep.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Runtime};

use super::fetch;
use super::schedule;
use super::{ServerRecord, ServersState, SshError, SyncEvent};

pub enum HubMsg {
    /// The store changed (add/remove/update) — recompute what is due.
    Changed,
    /// Pull this server as soon as possible.
    SyncNow(u64),
}

/// One attempt's whole budget; must exceed `ssh::EXPORT_TIMEOUT` (the first
/// export on a machine ingests its full log retention).
const PULL_TOTAL: Duration = Duration::from_secs(960);

/// Newly seen servers (startup, or just added) first pull after this grace —
/// long enough for the app to finish painting.
const START_GRACE: Duration = Duration::from_secs(15);

/// Upper bound on one sleep, so external store edits (or a machine wake-up)
/// are noticed within a minute even if no message arrived.
const MAX_TICK: Duration = Duration::from_secs(60);

pub fn start<R: Runtime>(app: AppHandle<R>, state: Arc<ServersState>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<HubMsg>();
    state.set_hub_tx(tx);

    tauri::async_runtime::spawn(async move {
        {
            let now = Instant::now();
            let servers = server_list(&state);
            if let Ok(mut due) = state.next_due.lock() {
                due.clear();
                for rec in servers.iter().filter(|r| r.enabled) {
                    due.insert(rec.id, now + START_GRACE);
                }
            }
        }
        state.emit_updated(&app);

        loop {
            let now = Instant::now();
            let due = {
                let servers = server_list(&state);
                let due_map = state.next_due.lock().map(|d| d.clone()).unwrap_or_default();
                let pulling = state.pulling.lock().map(|p| p.clone()).unwrap_or_default();
                schedule::due_ids(&servers, &due_map, &pulling, now)
            };

            if due.is_empty() {
                let wait = next_wait(&state, now);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    msg = rx.recv() => handle_msg(&state, msg),
                }
            } else {
                for id in due {
                    pull_one(&app, &state, id).await;
                    // Between servers, absorb queued edits so a "sync now"
                    // click lands in this pass, not the next tick.
                    while let Ok(msg) = rx.try_recv() {
                        handle_msg(&state, Some(msg));
                    }
                }
            }
        }
    });
}

fn server_list(state: &Arc<ServersState>) -> Vec<ServerRecord> {
    state.store.lock().map(|s| s.servers.clone()).unwrap_or_default()
}

fn handle_msg(state: &Arc<ServersState>, msg: Option<HubMsg>) {
    match msg {
        Some(HubMsg::SyncNow(id)) => {
            if let Ok(mut due) = state.next_due.lock() {
                due.insert(id, Instant::now());
            }
            // The id may be unknown to `next_due` yet (just added): inserting
            // is already "due now", so nothing else to do.
        }
        Some(HubMsg::Changed) => {}
        // The channel only closes when the state drops the sender — app exit.
        None => {}
    }
}

fn next_wait(state: &Arc<ServersState>, now: Instant) -> Duration {
    let servers = server_list(state);
    let due_map = state.next_due.lock().map(|d| d.clone()).unwrap_or_default();
    servers
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| due_map.get(&r.id))
        .map(|t| t.saturating_duration_since(now))
        .min()
        .unwrap_or(MAX_TICK)
        .clamp(Duration::from_secs(1), MAX_TICK)
}

async fn pull_one<R: Runtime>(app: &AppHandle<R>, state: &Arc<ServersState>, id: u64) {
    let Some(rec) = server_list(state).into_iter().find(|r| r.id == id) else {
        return;
    };

    if let Ok(mut pulling) = state.pulling.lock() {
        pulling.insert(id);
    }
    state.emit_updated(app);

    let result = tokio::time::timeout(PULL_TOTAL, fetch::pull_once(app, &rec)).await;
    let now_ms = usage_core::report::now_ms();

    match result {
        Ok(Ok(pulled)) => {
            let _ = state.mutate(|store| {
                if let Some(r) = store.servers.iter_mut().find(|r| r.id == id) {
                    r.last_ok_ms = Some(now_ms);
                    r.last_error = None;
                    r.fail_count = 0;
                    r.last_rows = pulled.rows;
                    r.last_took_ms = pulled.took_ms;
                    r.push_history(SyncEvent {
                        at_ms: now_ms,
                        rows: pulled.rows,
                        took_ms: pulled.took_ms,
                        ok: true,
                    });
                }
            });
            if let Ok(mut due) = state.next_due.lock() {
                due.insert(id, Instant::now() + Duration::from_secs(rec.every_secs.max(60)));
            }
            crate::logging::info(&format!(
                "servers: pulled {} ({}) — {} rows{} in {} ms{}",
                rec.name,
                rec.url(),
                pulled.rows,
                if pulled.delivered { "" } else { " unchanged" },
                pulled.took_ms,
                if pulled.uploaded { ", collector updated" } else { "" }
            ));
        }
        Ok(Err(e)) => record_failure(state, &rec, &e, now_ms),
        Err(_) => record_failure(
            state,
            &rec,
            &SshError::timeout(format!("the pull of {} ({} s budget)", rec.name, PULL_TOTAL.as_secs())),
            now_ms,
        ),
    }

    if let Ok(mut pulling) = state.pulling.lock() {
        pulling.remove(&id);
    }
    state.emit_updated(app);
}

fn record_failure(state: &Arc<ServersState>, rec: &ServerRecord, err: &SshError, now_ms: i64) {
    let fail_count = state
        .mutate(|store| {
            if let Some(r) = store.servers.iter_mut().find(|r| r.id == rec.id) {
                r.fail_count = r.fail_count.saturating_add(1);
                r.last_error = Some(err.clone());
                r.push_history(SyncEvent { at_ms: now_ms, rows: 0, took_ms: 0, ok: false });
                return r.fail_count;
            }
            0
        })
        .unwrap_or(0);
    let backoff = schedule::backoff_secs(fail_count);
    if let Ok(mut due) = state.next_due.lock() {
        due.insert(rec.id, Instant::now() + Duration::from_secs(backoff));
    }
    crate::logging::error(&format!(
        "servers: pull of {} ({}) failed ({}) — retry in {} s: {}",
        rec.name, rec.url(), err.kind, backoff, err.detail
    ));
}

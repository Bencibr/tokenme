//! The single-owner sync runtime used by CLI, Panel and a user service.
//!
//! It deliberately owns no timer thread by itself: callers choose `run_once`
//! or `run_until`, while the SQLite lease is the process-wide singleton gate.
//! Both directions ride one resident WebSocket session: the uplink is one
//! symmetric rollup push per changed local state (a cheap change fingerprint
//! keeps an idle device silent), the downlink is one authenticated rollup
//! fetch per interval. Pairing state arrives as the server's `DeviceState`
//! push; between pushes the runtime trusts its local config, its last known
//! state, and the errors the session calls surface — a revoked, paused or
//! unpaired device's frames are refused, and that refusal is the cut-off
//! trigger. No outbox, no pages, no per-pass status poll.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    DeviceIdentity, ClientTransportCancellation, SyncError, SyncResult, SyncStore, Tls13HttpsTransport,
    TransportError,
};

// Keep the scheduler lease longer than the transport's 10-minute request
// deadline. A live sync pass must not become reclaimable by a second process
// while its owner is still waiting on the primary.
const DEFAULT_LEASE_TTL_MS: i64 = 15 * 60 * 1000;
const DEFAULT_POLL_MS: u64 = 5_000;
// The rollup downlink is a full snapshot of a few thousand aggregate rows —
// a ~300 KB response. Thirty seconds keeps it fresh without hammering the
// primary; there is no backfill to resume because there are no rows.
const ROLLUP_FETCH_INTERVAL_MS: i64 = 30_000;
// A paired device with nothing to exchange (sharing refused, or the uplink
// fingerprint idle with the fetch interval not yet due) does not keep its
// session leg open forever: after this much idle time the next pass drops it,
// and the next due call rebuilds it. Errors close the leg inside the
// transport already.
const SESSION_IDLE_CLOSE_MS: i64 = 10 * 60 * 1000;
const RETRY_MAX: Duration = Duration::from_secs(5 * 60);
const POLL_JITTER_PERCENT: u64 = 20;

#[derive(Debug, Clone)]
pub struct SyncRuntimeConfig {
    pub owner: String,
    pub lease_ttl_ms: i64,
    pub poll_interval: Duration,
    /// Legacy knob of the row-level snapshot pipeline: rollup sync ships
    /// aggregate rows whose coverage the index already bounds, so nothing
    /// reads this any more. The field stays because the config is public
    /// API and existing callers still set it.
    pub snapshot_days: i64,
}

impl SyncRuntimeConfig {
    pub fn new(owner: impl Into<String>) -> SyncResult<Self> {
        let label = owner.into();
        if label.is_empty() { return Err(SyncError::Invalid("runtime owner must not be empty".into())); }
        let mut instance = [0u8; 16];
        getrandom::fill(&mut instance).map_err(|error| SyncError::Crypto(format!("OS randomness unavailable: {error}")))?;
        let suffix = instance.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        Ok(Self { owner: format!("{label}-{suffix}"), lease_ttl_ms: DEFAULT_LEASE_TTL_MS, poll_interval: Duration::from_millis(DEFAULT_POLL_MS), snapshot_days: 30 })
    }
}

/// One sync pass's counters, rollup-era. The rollup pipeline keeps no
/// claim/ack bookkeeping for the sender — the push's `Ok` return IS the
/// acknowledgment, and push failures surface as pass errors with backoff —
/// so the only things a pass reports are what it pushed and what the
/// session moved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRunReport {
    /// Rollup rows carried by this pass's uplink push — the whole local
    /// snapshot, so the snapshot's size whenever local data changed; 0 when
    /// the change fingerprint matched the last delivered push (or the pass
    /// had nothing paired to do). `run_until` totals its passes' pushes.
    pub uploaded: u64,
    /// Rollup payload bytes this pass moved over the session: the uplink
    /// push's rows plus the downlink snapshot's rows. `run_until` totals its
    /// passes.
    pub bytes: u64,
    pub last_error: Option<String>,
}

pub struct SyncRuntime {
    store: SyncStore,
    transport: Tls13HttpsTransport,
    config: SyncRuntimeConfig,
    secondary_identity: Option<DeviceIdentity>,
    secondary_display_name: Option<String>,
    /// Whether the downlink should run. The status poll used to set this
    /// from the primary's answer; the session model starts it optimistically
    /// at `true` for a paired device and learns from the primary's refusal:
    /// a sharing-off primary answers the fetch with "the primary is not
    /// sharing data with secondaries", which the pass records here and then
    /// stops asking (no error, no backoff). The session's `DeviceState` push
    /// would correct it live once the transport records those frames —
    /// today they are read-loop advisories, so no slot is exposed to
    /// consume here.
    primary_data_sharing: bool,
    primary_download_complete: bool,
    /// Embedding-UI wake for the moment the primary cuts this device off
    /// (revoked/blocked) and the runtime self-disables. Receives a short
    /// reason tag; the panel turns it into a toast.
    on_cut_off: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    last_rollup_fetch: Option<i64>,
    /// (row count, summed in_tok, latest max_ts) of the local-origin rollup
    /// rows at the moment of the last delivered push. None until the first
    /// push; equal to the live tuple → the uplink push is skipped as
    /// unchanged.
    last_upload_fingerprint: Option<(i64, f64, i64)>,
    /// Pass clock of the last delivered push. Does not move while pushes are
    /// skipped as unchanged, so it doubles as the "something actually went
    /// out" marker tests and the panel read.
    last_upload_pushed_at: Option<i64>,
    /// Pass clock of the last session call that actually round-tripped (a
    /// delivered push or an applied fetch). Drives the idle-leg close; 0
    /// until the first exchange.
    last_session_activity_ms: i64,
    /// The display name the primary last acknowledged via the session's
    /// `DisplayNameUpdate` frame. The session's Auth frame announces the name
    /// on every fresh leg; this frame covers renames — sent once per change,
    /// never per pass like the HTTP call it replaced.
    last_display_name_sent: Option<String>,
    /// Last instant the scheduler lease was actually rewritten. Renewing on
    /// every pass committed a write transaction twice per 5 s poll while the
    /// 15-minute lease was nowhere near expiry.
    lease_renewed_at_ms: i64,
}

impl SyncRuntime {
    pub fn new(store: SyncStore, transport: Tls13HttpsTransport, config: SyncRuntimeConfig) -> Self {
        Self { store, transport, config, secondary_identity: None, secondary_display_name: None, primary_data_sharing: false, primary_download_complete: false, last_rollup_fetch: None, last_upload_fingerprint: None, last_upload_pushed_at: None, last_session_activity_ms: 0, last_display_name_sent: None, lease_renewed_at_ms: 0, on_cut_off: None }
    }

    pub fn new_secondary(
        store: SyncStore,
        transport: Tls13HttpsTransport,
        config: SyncRuntimeConfig,
        identity: DeviceIdentity,
    ) -> Self {
        // Every session frame authenticates with this identity; pinning it
        // once here covers rebuilt legs too.
        transport.set_ws_session_identity(identity.device_id.clone(), identity.origin.clone());
        Self { store, transport, config, secondary_identity: Some(identity), secondary_display_name: None, primary_data_sharing: true, primary_download_complete: false, last_rollup_fetch: None, last_upload_fingerprint: None, last_upload_pushed_at: None, last_session_activity_ms: 0, last_display_name_sent: None, lease_renewed_at_ms: 0, on_cut_off: None }
    }

    pub fn new_secondary_with_display_name(
        store: SyncStore,
        transport: Tls13HttpsTransport,
        config: SyncRuntimeConfig,
        identity: DeviceIdentity,
        display_name: impl Into<String>,
    ) -> Self {
        let mut runtime = Self::new_secondary(store, transport, config, identity);
        runtime.secondary_display_name = Some(display_name.into());
        runtime
    }

    pub fn store(&self) -> &SyncStore { &self.store }
    pub fn store_mut(&mut self) -> &mut SyncStore { &mut self.store }

    /// Registers the embedding UI's wake for cut-off events (see the field).
    pub fn set_cut_off_notify(&mut self, hook: impl Fn(&str) + Send + Sync + 'static) {
        self.on_cut_off = Some(Arc::new(hook));
    }
    pub fn owner(&self) -> &str { &self.config.owner }

    /// The pass clock of the last delivered rollup push, if any. It does not
    /// move while pushes are skipped as unchanged (see the field's doc).
    pub fn last_upload_pushed_at(&self) -> Option<i64> { self.last_upload_pushed_at }

    pub fn transport_cancellation(&self) -> ClientTransportCancellation { self.transport.cancellation() }

    pub fn run_until_real(&mut self, stop: &AtomicBool) -> SyncResult<RuntimeRunReport> {
        self.run_until(stop, current_time_ms)
    }

    pub fn run_until_with_ingest<F>(&mut self, stop: &AtomicBool, now_ms: impl FnMut() -> i64, ingest: F) -> SyncResult<RuntimeRunReport>
    where
        F: FnMut(&mut SyncStore) -> SyncResult<()>,
    {
        self.run_until_inner(stop, now_ms, ingest)
    }

    /// One foreground pass. A caller that wants a resident service should use
    /// `run_until`, so the lease remains held between passes.
    pub fn run_once(&mut self) -> SyncResult<RuntimeRunReport> {
        self.run_once_with_clock(current_time_ms)
    }

    /// Deterministic clock seam for lifecycle tests; production callers use
    /// `run_once`, so lease checks never reuse the pre-push time.
    pub fn run_once_with_clock(&mut self, mut now_ms: impl FnMut() -> i64) -> SyncResult<RuntimeRunReport> {
        let first_now = now_ms();
        if !self.store.acquire_scheduler_lease(&self.config.owner, first_now, self.config.lease_ttl_ms)? {
            return Err(SyncError::Security("another sync runtime owns the scheduler lease".into()));
        }
        self.lease_renewed_at_ms = first_now;
        let result = (|| {
            let (pushed, push_bytes) = self.prepare_secondary(&mut now_ms)?;
            let mut report = self.run_owned_once(None, &mut now_ms)?;
            report.uploaded = pushed;
            report.bytes = report.bytes.saturating_add(push_bytes);
            Ok(report)
        })();
        if let Err(error) = &result {
            self.remember_secondary_error(error, now_ms());
        }
        let _ = self.store.release_scheduler_lease(&self.config.owner);
        result
    }

    /// Resident scheduler lifecycle. It holds the SQLite lease across passes,
    /// pushes the uplink rollup when the local fingerprint moves, fetches the
    /// primary's downlink snapshot on its interval, and stops between passes
    /// when the caller sets the flag. A failed session call surfaces as a
    /// pass error with backoff.
    pub fn run_until(&mut self, stop: &AtomicBool, mut now_ms: impl FnMut() -> i64) -> SyncResult<RuntimeRunReport> {
        self.run_until_inner(stop, &mut now_ms, |_| Ok(()))
    }

    fn run_until_inner<F>(&mut self, stop: &AtomicBool, mut now_ms: impl FnMut() -> i64, mut ingest: F) -> SyncResult<RuntimeRunReport>
    where
        F: FnMut(&mut SyncStore) -> SyncResult<()>,
    {
        // A lease held by another owner means a previous host process died
        // without releasing it (or a sibling runtime is live). Both are
        // wait-out situations, not failures: wait one poll interval between
        // attempts until the lease frees (crash: ≤ one TTL) or the owner
        // renews (a live peer is doing the work). Failing hard here is what
        // used to strand a crash-restarted secondary in a futile respawn
        // loop for the whole TTL.
        let mut first_now = now_ms();
        while !self.store.acquire_scheduler_lease(&self.config.owner, first_now, self.config.lease_ttl_ms)? {
            if stop.load(Ordering::Acquire) {
                return Ok(RuntimeRunReport::default());
            }
            let wait_started = std::time::Instant::now();
            let wait = Duration::from_secs(5);
            while wait_started.elapsed() < wait && !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(50).min(wait));
            }
            if stop.load(Ordering::Acquire) {
                return Ok(RuntimeRunReport::default());
            }
            first_now = now_ms();
        }
        self.lease_renewed_at_ms = first_now;
        let mut total = RuntimeRunReport::default();
        let mut consecutive_errors = 0_u32;
        while !stop.load(Ordering::Acquire) {
            let lease_now = now_ms();
            if !self.renew_lease_if_due(lease_now)? {
                let _ = self.store.release_scheduler_lease(&self.config.owner);
                return Err(SyncError::Security("sync scheduler lease was lost before preparing the next pass".into()));
            }
            if let Err(error) = ingest(&mut self.store) {
                let _ = self.store.release_scheduler_lease(&self.config.owner);
                return Err(error);
            }
            let mut pushed_rows = 0_u64;
            let mut push_bytes = 0_u64;
            let pass = match self.prepare_secondary(&mut now_ms) {
                Ok((pushed, bytes)) => {
                    pushed_rows = pushed;
                    push_bytes = bytes;
                    self.run_owned_once(Some(stop), &mut now_ms)
                }
                Err(error) if self.secondary_identity.is_some() && retryable_secondary_error(&error) => {
                    let message = error.to_string();
                    let _ = self.store.set_secondary_last_error(Some(&message), now_ms());
                    Ok(RuntimeRunReport { last_error: Some(message), ..RuntimeRunReport::default() })
                }
                Err(error) => Err(error),
            };
            let pass = match pass {
                Ok(pass) => pass,
                Err(error) => {
                    self.remember_secondary_error(&error, now_ms());
                    let _ = self.store.release_scheduler_lease(&self.config.owner);
                    return Err(error);
                }
            };
            total.uploaded = total.uploaded.saturating_add(pushed_rows);
            total.bytes = total.bytes.saturating_add(pass.bytes).saturating_add(push_bytes);
            let pass_had_error = pass.last_error.is_some();
            let waiting_for_primary = pass.last_error.as_deref().is_some_and(|message| message.contains("awaiting approval") || message.contains("primary paused this device"));
            consecutive_errors = if pass_had_error && !waiting_for_primary { consecutive_errors.saturating_add(1) } else { 0 };
            total.last_error = pass.last_error;
            if !pass_had_error && self.secondary_identity.is_some() {
                let _ = self.store.set_secondary_last_error(None, now_ms());
            }
            let renew_now = now_ms();
            if !self.renew_lease_if_due(renew_now)? {
                let _ = self.store.release_scheduler_lease(&self.config.owner);
                return Err(SyncError::Security("sync scheduler lease was lost".into()));
            }
            if stop.load(Ordering::Acquire) { break; }
            // There is no bounded catch-up continuation any more: the uplink
            // push and the rollup downlink are both one-shot session calls,
            // so a delivered-or-skipped push IS complete catch-up, and a
            // failed one surfaces as a pass error with the backoff below
            // instead of a fast continuation loop. Every pass here is a timed
            // pass — it ingests, exchanges and sleeps the full (jittered)
            // interval.
            let started = std::time::Instant::now();
            let interval = idle_interval(self.config.poll_interval, consecutive_errors);
            self.primary_download_complete = false;
            while started.elapsed() < interval && !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(50).min(interval));
            }
        }
        // A stop is not a departure the primary should act on: the resident
        // socket closing reports the device offline through the server's own
        // session guard, and the session's DisconnectNotify frame means
        // "leave the pairing" (the store's disconnect_device revokes the
        // registry entry) — an unpair decision that belongs to the embedder's
        // unpair flow, never to a stop or a drop.
        let _ = self.store.release_scheduler_lease(&self.config.owner);
        Ok(total)
    }

    /// Renews at most once per half TTL. A renew that is not due still answers
    /// `true`: the lease cannot be taken by another owner before it expires.
    fn renew_lease_if_due(&mut self, now_ms: i64) -> SyncResult<bool> {
        let half_ttl = self.config.lease_ttl_ms / 2;
        if self.lease_renewed_at_ms != 0 && now_ms.saturating_sub(self.lease_renewed_at_ms) < half_ttl {
            return Ok(true);
        }
        let renewed = self.store.renew_scheduler_lease(&self.config.owner, now_ms, self.config.lease_ttl_ms)?;
        if renewed { self.lease_renewed_at_ms = now_ms; }
        Ok(renewed)
    }

    fn run_owned_once(&mut self, stop: Option<&AtomicBool>, now_ms: &mut impl FnMut() -> i64) -> SyncResult<RuntimeRunReport> {
        let mut report = RuntimeRunReport::default();
        if stop.is_some_and(|flag| flag.load(Ordering::Acquire)) { return Ok(report); }
        if self.secondary_identity.is_some()
            && self.store.secondary_config()?.is_some_and(|config| !config.enabled)
        {
            self.close_idle_session(now_ms());
            return Ok(report);
        }
        // The uplink push already ran inside `prepare_secondary`; this half
        // owns the downlink. The session leg is built by the first session
        // call of a pass and stays resident across passes — a LAN keepalive
        // link beats a fresh TCP+TLS+WS handshake every 30 seconds, and with
        // the status poll gone there is no per-pass HTTP call left to starve
        // behind a held slot. A pass with nothing to exchange drops the leg
        // once it has idled past SESSION_IDLE_CLOSE_MS.
        //
        // Rollup-level downlink: one authenticated full-snapshot fetch per
        // ROLLUP_FETCH_INTERVAL over the session replaces the row-level page
        // walk entirely — a 500k-event primary lands in one ~300 KB response,
        // so there is no backfill and nothing to resume.
        if self.secondary_identity.is_some() && self.primary_data_sharing {
            let due = self.last_rollup_fetch.map_or(true, |at| {
                now_ms().saturating_sub(at) >= ROLLUP_FETCH_INTERVAL_MS
            });
            if due {
                match self.fetch_primary_rollup(now_ms()) {
                    Ok(payload_bytes) => {
                        self.primary_download_complete = true;
                        self.last_rollup_fetch = Some(now_ms());
                        report.bytes = payload_bytes;
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if message.to_ascii_lowercase().contains("sharing") {
                            // The primary refuses the downlink over data
                            // sharing ("the primary is not sharing data with
                            // secondaries"): a configuration answer, not a
                            // device-state problem — learn it and stop
                            // asking, no last_error, no backoff. The session's
                            // DeviceState push (or a restart) re-opens the
                            // question.
                            self.primary_data_sharing = false;
                            self.primary_download_complete = true;
                        } else if is_cut_off_error(&error) {
                            // A refused fetch is also a revocation sensor for
                            // a device whose push was skipped as unchanged.
                            self.remember_secondary_error(&error, now_ms());
                            report.last_error = Some(message);
                        } else {
                            let message = format!("primary rollup sync failed: {error}");
                            let _ = self.store.set_secondary_last_error(Some(&message), now_ms());
                            report.last_error = Some(message);
                        }
                    }
                }
            } else {
                self.primary_download_complete = true;
            }
        } else if self.secondary_identity.is_some() {
            self.close_idle_session(now_ms());
        }
        if self.secondary_identity.is_some() && report.last_error.is_none() {
            let _ = self.store.set_secondary_last_error(None, now_ms());
        }
        Ok(report)
    }

    /// One authenticated full-snapshot fetch of the primary's own rollup
    /// rows over the resident session, applied as a full replace for the
    /// primary's origin label. This is the whole downlink: no pages, no
    /// cursor, no backfill. Returns the snapshot's payload size for the pass
    /// report.
    fn fetch_primary_rollup(&mut self, now_ms: i64) -> SyncResult<u64> {
        let snapshot = self.transport.ws_rollup_fetch().map_err(|error| map_session_error(self.secondary_identity.as_ref(), error))?;
        let payload_bytes = serde_json::to_vec(&snapshot.rows).map(|bytes| bytes.len() as u64).unwrap_or(0);
        self.store.apply_remote_rollup(&snapshot.origin, &snapshot.rows, now_ms)?;
        self.last_session_activity_ms = now_ms;
        self.store.mark_secondary_connected(now_ms)?;
        Ok(payload_bytes)
    }

    /// The paired preamble every pass runs, closed by the uplink push.
    /// Returns (rollup rows the push carried, push payload bytes) — (0, 0)
    /// when the pass had nothing paired to do or the fingerprint matched the
    /// last delivered push. Errors are the caller's record-and-backoff input,
    /// and a revoked/unpaired refusal reaches the cut-off path through them.
    fn prepare_secondary(&mut self, now_ms: &mut impl FnMut() -> i64) -> SyncResult<(u64, u64)> {
        let Some(identity) = self.secondary_identity.clone() else { return Ok((0, 0)); };
        let config = self.store.secondary_config()?.ok_or_else(|| SyncError::NotPaired(identity.device_id.clone()))?;
        if !config.enabled {
            // Stop is the network scheduler switch.  A paired device keeps
            // rolling local rows up while offline so resume cannot skip
            // changes made before the last push.
            return Ok((0, 0));
        }
        self.send_display_name_update_once();
        self.push_local_rollup(now_ms())
    }

    fn remember_secondary_error(&mut self, error: &SyncError, now_ms: i64) {
        if self.secondary_identity.is_none() { return; }
        let message = error.to_string();
        let _ = self.store.set_secondary_last_error(Some(&message), now_ms);
        if is_cut_off_error(error) {
            let _ = self.store.set_secondary_enabled(false, now_ms);
            // The primary cut this device off (revoked or blocked): drop the
            // session leg and wake the owner directly — the panel polls on
            // its own cadence, so this is the same door-knock courtesy the
            // primary's pairing hook provides.
            self.transport.ws_close();
            if let Some(hook) = self.on_cut_off.as_ref() {
                let reason = match error {
                    SyncError::Revoked(_) => "revoked",
                    SyncError::Security(_) => "security",
                    _ => "rejected",
                };
                hook(reason);
            }
        }
    }

    /// The uplink half of symmetric rollup sync: this device's own rollup
    /// rows pushed to the primary over the resident WebSocket session. A
    /// cheap change fingerprint — (row count, summed in_tok, latest max_ts)
    /// over the local-origin rows — keeps an idle device from pushing
    /// anything: the snapshot is re-sent only when the tuple moves, or on
    /// the first pass so a fresh pairing establishes its uplink even with
    /// zero rows. The primary's apply is idempotent (INSERT OR REPLACE per
    /// row plus the ack stamp), so a push whose ack was lost is safely
    /// re-sent — the fingerprint skip is an optimization, not a correctness
    /// gate. Returns (rows carried, payload bytes) — (0, 0) when skipped as
    /// unchanged.
    fn push_local_rollup(&mut self, now_ms: i64) -> SyncResult<(u64, u64)> {
        let fingerprint = self.local_rollup_fingerprint()?;
        if self.last_upload_fingerprint == Some(fingerprint) {
            return Ok((0, 0));
        }
        let rows = self.store.local_rollup_snapshot()?;
        let payload_bytes = serde_json::to_vec(&rows).map(|bytes| bytes.len() as u64).unwrap_or(0);
        // Error handling is the old outbox drain's: a version-incompatible
        // primary is disabled — resetting would loop reset → re-push → 426
        // forever, so only an upgrade fixes it — a broken sync state is
        // auto-recovered by resetting it locally, and anything else is mapped
        // onto the status poll's taxonomy so a revoked or unpaired device
        // still reaches the cut-off path through `remember_secondary_error`.
        match self.transport.ws_rollup_sync(&rows) {
            Ok(_origin) => {
                // The ack stamp is a PERSISTENT write that can lose the
                // ingest engine's lock race; it must land before the
                // in-memory fingerprint latches, or a lost race skips every
                // future push and the stamp never lands at all.
                self.store.mark_uplink_rollup_acked(now_ms)?;
                self.last_upload_fingerprint = Some(fingerprint);
                self.last_upload_pushed_at = Some(now_ms);
                self.last_session_activity_ms = now_ms;
                self.store.mark_secondary_connected(now_ms)?;
                Ok((rows.len() as u64, payload_bytes))
            }
            Err(error) => {
                let raw = error.to_string();
                if is_version_incompatible_error(&raw) {
                    let _ = self.store.reset_secondary_sync_state(now_ms);
                    let _ = self.store.set_secondary_enabled(false, now_ms);
                    let _ = self.store.set_secondary_last_error(Some(&raw), now_ms);
                    return Err(SyncError::Protocol(raw));
                }
                if is_unrecoverable_sync_error(&raw) {
                    let _ = self.store.reset_secondary_sync_state(now_ms);
                    return Err(SyncError::Protocol(format!("auto-recovering: {raw}")));
                }
                Err(map_session_error(self.secondary_identity.as_ref(), error))
            }
        }
    }

    /// The uplink's change fingerprint: (n, sum(in_tok), max(max_ts)) over
    /// this device's local-origin rollup rows. `event_rollup.origin` IS the
    /// materialized `ORIGIN_SQL` of each event's source — the index folds it
    /// in at ingest and the table carries no `source` column to re-evaluate
    /// the expression against — so the local rows are exactly `origin = ''`,
    /// the same predicate `SyncStore::local_rollup_snapshot` serves. A row
    /// edit that keeps all three equal is invisible to the push; aggregate
    /// rows are append-mostly, so that trade is the cheap idleness gate's
    /// accepted cost.
    fn local_rollup_fingerprint(&self) -> SyncResult<(i64, f64, i64)> {
        self.store.index().conn()
            .query_row(
                "SELECT count(*), IFNULL(sum(in_tok), 0), IFNULL(MAX(max_ts), 0) FROM event_rollup WHERE origin = ''",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|error| SyncError::Sqlite(error.to_string()))
    }

    /// Sends the session's `DisplayNameUpdate` frame only when the known name
    /// is not the one the primary already acknowledged — once per name
    /// change, not once per pass like the HTTP call it replaced. The frame
    /// carries just the name: the session's Auth frame proves the sender is
    /// the paired device, and the primary applies the same validity rule the
    /// signed HTTP arm enforced. Best-effort: a failed update is logged and
    /// retried on the next pass.
    fn send_display_name_update_once(&mut self) {
        let Some(name) = self.secondary_display_name.clone() else { return; };
        if self.last_display_name_sent.as_deref() == Some(name.as_str()) { return; }
        match self.transport.ws_update_display_name(&name) {
            Ok(()) => self.last_display_name_sent = Some(name),
            Err(error) => eprintln!("tokenme sync: display-name update failed: {error}"),
        }
    }

    /// Drops the resident session leg after SESSION_IDLE_CLOSE_MS without a
    /// round-trip. The next due call rebuilds it, so a device with nothing
    /// to exchange stops holding a primary slot.
    fn close_idle_session(&mut self, now_ms: i64) {
        if self.last_session_activity_ms != 0 && now_ms.saturating_sub(self.last_session_activity_ms) >= SESSION_IDLE_CLOSE_MS {
            self.transport.ws_close();
        }
    }
}

/// The session-model replacement for the status poll's terminal verdicts: a
/// revoked (or security-rejected, or finally rejected/expired pairing) device
/// must self-disable and wake the embedding UI instead of retrying forever.
fn is_cut_off_error(error: &SyncError) -> bool {
    matches!(error, SyncError::Revoked(_) | SyncError::Security(_))
        || matches!(error, SyncError::NotPaired(message) if message == "pairing request was rejected" || message == "pairing invite expired")
}

/// Maps a refused session call onto the same taxonomy the status poll used,
/// so `remember_secondary_error`'s cut-off logic keeps working unchanged.
/// The wire `Error` frame's message is the only carrier of device state on a
/// refused call, so the mapping is keyword-based: the canonical status words
/// (the registry's own vocabulary) map precisely, and the generic refusal —
/// an in-session frame after the registry changed under an authenticated
/// leg — is treated as revoked: the device lost its paired privileges and
/// must self-disable rather than spin on a retryable error forever. The
/// session's `DeviceState` push disambiguates paused from revoked once its
/// recorded slot is exposed by the transport.
fn map_session_error(identity: Option<&DeviceIdentity>, error: TransportError) -> SyncError {
    if let TransportError::Unsupported(message) = &error { return SyncError::Unsupported(message.clone()); }
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    let device_id = identity.map(|identity| identity.device_id.clone()).unwrap_or_default();
    if lower.contains("revoked") { return SyncError::Revoked(device_id); }
    if lower.contains("paused") { return SyncError::Conflict("primary paused this device".into()); }
    if lower.contains("awaiting approval") || lower.contains("pending") { return SyncError::NotPaired("pairing request is awaiting approval".into()); }
    if lower.contains("rejected") { return SyncError::NotPaired("pairing request was rejected".into()); }
    if lower.contains("expired") { return SyncError::NotPaired("pairing invite expired".into()); }
    if lower.contains("not authorized") || lower.contains("not in paired state") || lower.contains("unpaired") || lower.contains("not paired") {
        return SyncError::Revoked(device_id);
    }
    SyncError::Protocol(message)
}

fn retryable_secondary_error(error: &SyncError) -> bool {
    match error {
        SyncError::Conflict(message) if message == "secondary sync is disabled" => false,
        SyncError::NotPaired(message) if message == "pairing request was rejected" || message == "pairing invite expired" => false,
        SyncError::Protocol(_) | SyncError::Auth(_) | SyncError::NotPaired(_) | SyncError::Conflict(_)
        | SyncError::EpochStale | SyncError::ReconcileRequired | SyncError::Sqlite(_) | SyncError::Io(_, _)
        | SyncError::Limit(_) => true,
        _ => false,
    }
}

/// A primary (or secondary) on an incompatible schema version answers
/// "update tokenme…" — no amount of resetting or re-snapshotting fixes that;
/// only an upgrade does.
fn is_version_incompatible_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("update tokenme") || lower.contains("unsupported") || lower.contains("schema is not supported")
}

fn is_unrecoverable_sync_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("reconcile")
        || lower.contains("epoch")
        || lower.contains("generation")
        || lower.contains("conflict")
        || lower.contains("stream closed")
        || lower.contains("stream_closed")
        || lower.contains("invalid manifest")
        || lower.contains("decompression")
        || lower.contains("corrupt")
        || lower.contains("signature")
        || lower.contains("package marker")
        || lower.contains("read package")
        || lower.contains("unexpected end of file")
}

fn idle_interval(poll_interval: Duration, consecutive_errors: u32) -> Duration {
    let base = poll_interval.saturating_mul(1_u32 << consecutive_errors.min(6));
    jittered(base.min(RETRY_MAX))
}

/// Spreads retry and poll instants so devices that lost the same primary do
/// not reconnect in lockstep. A missing RNG keeps the caller's interval.
fn jittered(interval: Duration) -> Duration {
    let millis = interval.as_millis().min(u64::MAX as u128) as u64;
    let span = millis.saturating_mul(POLL_JITTER_PERCENT) / 100;
    if span == 0 { return interval; }
    let mut random = [0u8; 8];
    if getrandom::fill(&mut random).is_err() { return interval; }
    let offset = u64::from_le_bytes(random) % (2 * span + 1);
    Duration::from_millis(millis.saturating_sub(span).saturating_add(offset).max(1))
}

fn current_time_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        hash_bytes, DeviceIdentity, HttpsServerConfig, HttpsTransportConfig, PrimarySyncConfig,
        PairingRequest, SecondarySyncConfig, SyncStore, Tls13HttpsTransport, MAX_COMPRESSED_BYTES,
    };
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;

    fn certificate() -> (Vec<u8>, Vec<u8>) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.der().to_vec(), key.serialize_der())
    }

    fn paired_primary_without_stream(device: &DeviceIdentity) -> SyncStore {
        let mut primary = SyncStore::open_in_memory("primary-1").unwrap();
        let invite = primary.create_pairing_invite(
            "https://localhost:47821",
            &"1".repeat(64),
            i64::MAX,
        ).unwrap();
        let request = PairingRequest::new(&invite, device, "linux").unwrap();
        let state = primary.receive_pairing_request(&request, 1).unwrap();
        primary.approve_pairing(&state.request_id, 2).unwrap();
        primary
    }

    fn server_config(cert_der: &[u8], key_der: &[u8], staging_dir: std::path::PathBuf) -> HttpsServerConfig {
        HttpsServerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            certificate_der: cert_der.to_vec(), private_key_der: key_der.to_vec(),
            allow_lan: false, allowed_peer_ips: vec![], max_body_bytes: MAX_COMPRESSED_BYTES,
            // The session model needs one slot: the resident WebSocket serves
            // the push, the fetch and the display-name frame on one leg.
            max_concurrent: 4, staging_dir, idle_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(600), staging_quota_bytes: 512 * 1024 * 1024,
        }
    }

    /// A budgeted server loop outlives the requests under test; empty TCP
    /// connections drain the remaining budget so the listener can exit.
    fn drain_server(addr: std::net::SocketAddr, thread: thread::JoinHandle<Result<(), crate::TransportError>>) {
        for _ in 0..64 {
            if std::net::TcpStream::connect(addr).is_err() { break; }
        }
        assert!(thread.join().unwrap().is_ok());
    }

    #[test]
    fn poll_jitter_stays_inside_the_configured_band() {
        for interval in [Duration::from_millis(5), Duration::from_millis(500), Duration::from_secs(300)] {
            let millis = interval.as_millis() as u64;
            let span = millis * POLL_JITTER_PERCENT / 100;
            for _ in 0..200 {
                let jittered = jittered(interval);
                assert!(jittered.as_millis() as u64 >= millis - span, "{jittered:?} below the band of {interval:?}");
                assert!(jittered.as_millis() as u64 <= millis + span, "{jittered:?} above the band of {interval:?}");
            }
        }
        assert_eq!(jittered(Duration::from_millis(4)), Duration::from_millis(4));
    }

    #[test]
    fn error_backoff_doubles_to_the_cap() {
        let steady = idle_interval(Duration::from_secs(5), 0);
        assert!(steady >= Duration::from_millis(4_000) && steady <= Duration::from_millis(6_000), "{steady:?}");
        let capped = idle_interval(Duration::from_secs(60), 10);
        assert!(capped >= Duration::from_secs(240) && capped <= Duration::from_secs(360), "{capped:?}");
        // The default 5s cadence must still reach the documented 5-minute cap.
        let default_capped = idle_interval(Duration::from_secs(5), 10);
        assert!(default_capped >= Duration::from_secs(240) && default_capped <= Duration::from_secs(360), "{default_capped:?}");
    }

    #[test]
    fn secondary_runtime_syncs_primary_rollup_over_loopback_tls() {
        let device = DeviceIdentity::create("runtime-downlink", "Secondary").unwrap();
        let mut primary = paired_primary_without_stream(&device);
        let now = usage_core::report::now_ms();
        primary.save_primary_config(&PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
            advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: true,
            share_data_with_secondaries: true,
        }, now).unwrap();
        // The primary's own rollup rows, plus a foreign-origin row that must
        // never travel: the fetch serves local-origin rows only.
        let day = usage_core::local_day_of(now).unwrap().format("%Y-%m-%d").to_string();
        primary.index().conn().execute(
            "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter,                     in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts)              VALUES(?1, '', 'codex', 'primary-session', '', '', 'tokens', 9, 0, 0, 0, 0, 0, 1, 1, ?2, ?2)",
            rusqlite::params![day, now - 1_000],
        ).unwrap();
        primary.index().conn().execute(
            "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter,                     in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts)              VALUES(?1, 'other-device', 'codex', 's', '', '', 'tokens', 99, 0, 0, 0, 0, 0, 1, 1, ?2, ?2)",
            rusqlite::params![day, now - 1_000],
        ).unwrap();

        let (cert_der, key_der) = certificate();
        let staging = tempfile::tempdir().unwrap();
        let server = crate::HttpsSyncServer::bind(
            server_config(&cert_der, &key_der, staging.path().join("staging")), primary,
        ).unwrap();
        let addr = server.local_addr().unwrap();
        let server_thread = thread::spawn(move || server.run_connections_concurrent(6));

        let mut secondary = SyncStore::open_in_memory("primary-1").unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        secondary.save_secondary_config(&SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: format!("https://127.0.0.1:{}", addr.port()), server_name: "localhost".into(),
            certificate_pin: hash_bytes(&cert_der), certificate_der: cert_der.clone(),
            identity_path: identity_dir.path().join("secondary-identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }, now).unwrap();
        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert_der,
        )).unwrap();
        let mut runtime = SyncRuntime::new_secondary(
            secondary, transport, SyncRuntimeConfig::new("secondary-downlink-test").unwrap(), device,
        );

        let report = runtime.run_once_with_clock(|| now + 10).unwrap();
        assert_eq!(report.uploaded, 0, "the secondary owns no local rows: the establishing push is empty");
        assert!(report.bytes > 0, "the fetched snapshot's payload counts into the pass report");
        assert!(report.last_error.is_none());
        assert!(runtime.primary_download_complete, "one session rollup fetch completes the downlink");
        let (origin, in_tok): (String, f64) = runtime.store().index().conn().query_row(
            "SELECT origin, in_tok FROM remote_rollup", [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        ).unwrap();
        assert_eq!(origin, usage_index::hostname());
        assert_eq!(in_tok, 9.0);
        assert_eq!(runtime.store().index().event_count().unwrap(), 0, "raw event rows are no longer downloaded");
        let config = runtime.store().secondary_config().unwrap().unwrap();
        assert_eq!(config.last_error, None);
        drop(runtime);
        drain_server(addr, server_thread);
    }

    #[test]
    fn secondary_runtime_pushes_its_rollup_snapshot_once_and_skips_unchanged() {
        let device = DeviceIdentity::create("runtime-rollup-push", "Secondary").unwrap();
        // The primary is file-backed so the test can read `remote_rollup`
        // while the server holds its own connection to the same store.
        let primary_dir = tempfile::tempdir().unwrap();
        let primary_path = primary_dir.path().join("index.db");
        let mut primary_admin = SyncStore::open(&primary_path, "primary-1").unwrap();
        let invite = primary_admin.create_pairing_invite("https://localhost:47821", &"1".repeat(64), i64::MAX).unwrap();
        let request = PairingRequest::new(&invite, &device, "linux").unwrap();
        let state = primary_admin.receive_pairing_request(&request, 1).unwrap();
        primary_admin.approve_pairing(&state.request_id, 2).unwrap();
        // The downlink rides the same session; without an explicit sharing
        // answer the runtime would optimistically fetch and learn the refusal.
        primary_admin.save_primary_config(&PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
            advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: true,
            share_data_with_secondaries: true,
        }, usage_core::report::now_ms()).unwrap();

        let mut secondary = SyncStore::open_in_memory("primary-1").unwrap();
        let (cert_der, key_der) = certificate();
        let staging = tempfile::tempdir().unwrap();
        let server = crate::HttpsSyncServer::bind(
            server_config(&cert_der, &key_der, staging.path().join("staging")),
            SyncStore::open(&primary_path, "primary-1").unwrap(),
        ).unwrap();
        let addr = server.local_addr().unwrap();
        let server_thread = thread::spawn(move || server.run_connections_concurrent(16));
        let identity_dir = tempfile::tempdir().unwrap();
        let first_now = usage_core::report::now_ms();
        secondary.save_secondary_config(&SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: format!("https://127.0.0.1:{}", addr.port()), server_name: "localhost".into(),
            certificate_pin: hash_bytes(&cert_der), certificate_der: cert_der.clone(),
            identity_path: identity_dir.path().join("secondary-identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }, first_now).unwrap();
        // Five local-origin rollup rows — this device's whole uplink.
        for index in 1..=5 {
            secondary.index_mut().conn().execute(
                "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter,                 in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts)              VALUES('2026-10-22', '', 'codex', ?1, '', '', 'tokens', 3, 0, 0, 0, 0, 0, 1, 1, ?2, ?2)",
                rusqlite::params![format!("session-{index}"), first_now - 1_000],
            ).unwrap();
        }
        let remote = |admin: &SyncStore| -> (i64, f64) {
            admin.index().conn().query_row(
                "SELECT count(*), IFNULL(sum(in_tok), 0) FROM remote_rollup WHERE origin = ?1",
                rusqlite::params![device.origin], |row| Ok((row.get(0)?, row.get(1)?)),
            ).unwrap()
        };

        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert_der,
        )).unwrap();
        let mut runtime = SyncRuntime::new_secondary(
            secondary, transport, SyncRuntimeConfig::new("secondary-rollup-owner").unwrap(), device.clone(),
        );
        let report = runtime.run_once_with_clock(|| first_now).unwrap();
        assert_eq!(report.uploaded, 5, "the first pass pushes the whole local snapshot");
        let stamped: i64 = runtime.store().index().conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key LIKE 'device_sync_uplink_rollup_acked_at_ms:%')",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(stamped, 1, "the ack stamp must persist for the panel");
        assert!(report.bytes > 0, "the pushed snapshot's payload counts into the pass report");
        assert_eq!(remote(&primary_admin), (5, 15.0), "the primary stored the snapshot under the device origin");
        assert!(runtime.last_upload_fingerprint.is_some(), "the delivered push left its fingerprint behind");
        assert_eq!(runtime.last_upload_pushed_at, Some(first_now));

        let idle = runtime.run_once_with_clock(|| first_now + 5_000).unwrap();
        assert_eq!(idle.uploaded, 0, "an unchanged local rollup pushes nothing");
        assert_eq!(idle.last_error, None);
        assert_eq!(remote(&primary_admin), (5, 15.0));
        assert_eq!(runtime.last_upload_pushed_at, Some(first_now), "the push timestamp did not move: nothing was re-sent");

        runtime.store_mut().index_mut().conn().execute(
            "INSERT INTO event_rollup(day, origin, tool, session, project, model, meter,                 in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, n, nonzero, min_ts, max_ts)              VALUES('2026-10-23', '', 'codex', 'session-1', '', '', 'tokens', 7, 0, 0, 0, 0, 0, 1, 1, ?1, ?1)",
            rusqlite::params![first_now - 500],
        ).unwrap();
        let changed = runtime.run_once_with_clock(|| first_now + 10_000).unwrap();
        assert_eq!(changed.uploaded, 6, "new local data moves the fingerprint and pushes the full snapshot again");
        assert_eq!(remote(&primary_admin), (6, 22.0));
        assert_eq!(runtime.last_upload_pushed_at, Some(first_now + 10_000));

        drop(runtime);
        drain_server(addr, server_thread);
    }

    #[test]
    fn revoked_device_is_cut_off_through_the_session_error() {
        let device = DeviceIdentity::create("runtime-revoke", "Secondary").unwrap();
        let mut primary = paired_primary_without_stream(&device);
        let now = usage_core::report::now_ms();
        primary.save_primary_config(&PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
            advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: true,
            share_data_with_secondaries: true,
        }, now).unwrap();
        // The registry revocation happens before the first session attempt:
        // the Auth handshake must refuse, and that refusal is the cut-off
        // trigger the status poll used to be.
        primary.revoke_device(&device.device_id, now).unwrap();

        let (cert_der, key_der) = certificate();
        let staging = tempfile::tempdir().unwrap();
        let server = crate::HttpsSyncServer::bind(
            server_config(&cert_der, &key_der, staging.path().join("staging")), primary,
        ).unwrap();
        let addr = server.local_addr().unwrap();
        let server_thread = thread::spawn(move || server.run_connections_concurrent(6));

        let mut secondary = SyncStore::open_in_memory("primary-1").unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        secondary.save_secondary_config(&SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: format!("https://127.0.0.1:{}", addr.port()), server_name: "localhost".into(),
            certificate_pin: hash_bytes(&cert_der), certificate_der: cert_der.clone(),
            identity_path: identity_dir.path().join("secondary-identity.json"),
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }, now).unwrap();
        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert_der,
        )).unwrap();
        let mut runtime = SyncRuntime::new_secondary(
            secondary, transport, SyncRuntimeConfig::new("secondary-revoke-test").unwrap(), device,
        );
        let cut_offs = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&cut_offs);
        runtime.set_cut_off_notify(move |reason| sink.lock().unwrap().push(reason.into()));

        let error = runtime.run_once_with_clock(|| now + 10).unwrap_err();
        assert!(matches!(error, SyncError::Revoked(_)), "{error}");
        assert_eq!(*cut_offs.lock().unwrap(), vec!["revoked".to_string()], "the embedding UI wake fired");
        assert_eq!(runtime.store().secondary_config().unwrap().unwrap().enabled, false, "the device self-disabled");
        drop(runtime);
        drain_server(addr, server_thread);
    }

    #[test]
    fn session_refusals_map_onto_the_status_taxonomy() {
        let device = DeviceIdentity::create("runtime-mapper", "Secondary").unwrap();
        let identity = Some(&device);
        // The generic in-session refusal (registry changed under an
        // authenticated leg) is the revocation sensor.
        assert!(matches!(map_session_error(identity, TransportError::Protocol("device is not authorized to upload".into())), SyncError::Revoked(_)));
        assert!(matches!(map_session_error(identity, TransportError::Protocol("auth failed: device status revoked by primary".into())), SyncError::Revoked(_)));
        assert!(matches!(map_session_error(identity, TransportError::Protocol("primary paused this device".into())), SyncError::Conflict(message) if message == "primary paused this device"));
        assert!(matches!(map_session_error(identity, TransportError::Protocol("pairing request is awaiting approval".into())), SyncError::NotPaired(message) if message == "pairing request is awaiting approval"));
        // The canonical terminal pair must survive verbatim: the cut-off
        // matcher compares these exact strings.
        assert!(matches!(map_session_error(identity, TransportError::Protocol("pairing request was rejected".into())), SyncError::NotPaired(message) if message == "pairing request was rejected"));
        assert!(matches!(map_session_error(identity, TransportError::Protocol("pairing invite expired".into())), SyncError::NotPaired(message) if message == "pairing invite expired"));
        assert!(matches!(map_session_error(identity, TransportError::Unsupported("schema is not supported".into())), SyncError::Unsupported(_)));
        assert!(matches!(map_session_error(identity, TransportError::Timeout), SyncError::Protocol(_)));
        assert!(is_cut_off_error(&map_session_error(identity, TransportError::Protocol("device is not in paired state".into()))));
    }

    #[test]
    fn resident_runtime_releases_the_single_scheduler_lease_on_stop() {
        let sender = SyncStore::open_in_memory("primary-1").unwrap();
        let (cert_der, _key_der) = certificate();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{port}"), "localhost", cert_der,
        )).unwrap();
        let mut runtime = SyncRuntime::new(sender, transport, SyncRuntimeConfig::new("runtime-owner").unwrap());
        let stop = Arc::new(AtomicBool::new(true));
        runtime.run_until(&stop, || 10).unwrap();
        assert!(runtime.store_mut().acquire_scheduler_lease("other-owner", 11, 1_000).unwrap());
    }

    #[test]
    fn each_runtime_instance_gets_a_distinct_scheduler_owner() {
        let first = SyncRuntimeConfig::new("secondary").unwrap();
        let second = SyncRuntimeConfig::new("secondary").unwrap();
        assert_ne!(first.owner, second.owner);
        assert!(first.owner.starts_with("secondary-"));
        assert!(second.owner.starts_with("secondary-"));
    }
}

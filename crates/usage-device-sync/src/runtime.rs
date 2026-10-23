//! The single-owner sync runtime used by CLI, Panel and a user service.
//!
//! It deliberately owns no timer thread by itself: callers choose `run_once`
//! or `run_until`, while the SQLite lease is the process-wide singleton gate.
//! The uplink is one symmetric rollup push per changed local state (a cheap
//! change fingerprint keeps an idle device silent); the downlink is one
//! authenticated rollup-snapshot fetch per interval. No outbox, no pages.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use usage_core::{CallKind, Meter, UsageEvent};
use usage_index::Index;

use crate::{
    AuthenticatedDisplayNameRequest, AuthenticatedRollupRequest, AuthenticatedStatus,
    AuthenticatedStatusRequest, DeviceIdentity,
    NetworkCall, NetworkCounts, NetworkPackage, NetworkRow, StreamOpenReceipt,
    ClientTransportCancellation, SyncError, SyncResult, SyncStore, Tls13HttpsTransport,
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
// While the primary materializes historical identities it serves an empty
// preparing page. Poll that state like a progress bar: fast enough to finish
// initialization promptly, slow enough that the secondary is not a busy loop.
const PREPARING_POLL: Duration = Duration::from_millis(500);
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

/// One sync pass's counters. The outbox-era fields survive as public API,
/// but the rollup pipeline has no claim/ack bookkeeping for the sender: the
/// push's `Ok` return IS the acknowledgment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRunReport {
    /// Always 0: the outbox claim loop is gone.
    pub claimed: u64,
    /// Rollup rows carried by this pass's uplink push — the whole local
    /// snapshot, so the snapshot's size whenever local data changed; 0 when
    /// the change fingerprint matched the last delivered push (or the pass
    /// had nothing paired to do). `run_until` totals its passes' pushes.
    pub uploaded: u64,
    /// Always 0: a delivered rollup push needs no separate ack bookkeeping.
    pub acknowledged: u64,
    /// Always 0: push failures surface as pass errors with backoff instead
    /// of outbox retries.
    pub retried: u64,
    pub pending: u64,
    pub in_flight: u64,
    pub bytes: u64,
    pub last_error: Option<String>,
}

pub struct SyncRuntime {
    store: SyncStore,
    transport: Tls13HttpsTransport,
    config: SyncRuntimeConfig,
    secondary_identity: Option<DeviceIdentity>,
    secondary_display_name: Option<String>,
    primary_data_sharing: bool,
    primary_download_preparing: bool,
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
    /// Last instant the scheduler lease was actually rewritten. Renewing on
    /// every pass committed a write transaction twice per 5 s poll while the
    /// 15-minute lease was nowhere near expiry.
    lease_renewed_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct LocalSnapshot {
    pub package: NetworkPackage,
    pub rows: Vec<NetworkRow>,
}

impl SyncRuntime {
    pub fn new(store: SyncStore, transport: Tls13HttpsTransport, config: SyncRuntimeConfig) -> Self {
        Self { store, transport, config, secondary_identity: None, secondary_display_name: None, primary_data_sharing: false, primary_download_preparing: false, primary_download_complete: false, last_upload_fingerprint: None, last_upload_pushed_at: None, lease_renewed_at_ms: 0, last_rollup_fetch: None, on_cut_off: None }
    }

    pub fn new_secondary(
        store: SyncStore,
        transport: Tls13HttpsTransport,
        config: SyncRuntimeConfig,
        identity: DeviceIdentity,
    ) -> Self {
        // Every uplink push authenticates the resident ws session with this
        // identity; pinning it once here covers rebuilt legs too.
        transport.set_ws_session_identity(identity.device_id.clone(), identity.origin.clone());
        Self { store, transport, config, secondary_identity: Some(identity), secondary_display_name: None, primary_data_sharing: false, primary_download_preparing: false, primary_download_complete: false, last_upload_fingerprint: None, last_upload_pushed_at: None, lease_renewed_at_ms: 0, last_rollup_fetch: None, on_cut_off: None }
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
            let pushed = self.prepare_secondary(&mut now_ms)?;
            let mut report = self.run_owned_once(None, &mut now_ms)?;
            report.uploaded = pushed;
            Ok(report)
        })();
        if let Err(error) = &result {
            self.remember_secondary_error(error, now_ms());
        }
        let _ = self.store.release_scheduler_lease(&self.config.owner);
        result
    }

    /// Resident scheduler lifecycle. It recovers only its own lease's stale
    /// in-flight rows, sends FIFO bytes, and stops between requests when the
    /// caller sets the flag. A failed network request remains in outbox with
    /// backoff; it never advances to a later generation.
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
            self.primary_download_preparing = false;
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
            let pass = match self.prepare_secondary(&mut now_ms) {
                Ok(pushed) => {
                    pushed_rows = pushed;
                    self.run_owned_once(Some(stop), &mut now_ms)
                }
                Err(error) if self.secondary_identity.is_some() && retryable_secondary_error(&error) => {
                    let message = error.to_string();
                    let _ = self.store.set_secondary_last_error(Some(&message), now_ms());
                    let summary = self.store.outbox_summary()?;
                    Ok(RuntimeRunReport { pending: summary.pending, in_flight: summary.in_flight, bytes: summary.bytes, last_error: Some(message), ..RuntimeRunReport::default() })
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
            total.claimed = total.claimed.saturating_add(pass.claimed);
            total.uploaded = total.uploaded.saturating_add(pushed_rows);
            total.acknowledged = total.acknowledged.saturating_add(pass.acknowledged);
            total.retried = total.retried.saturating_add(pass.retried);
            total.pending = pass.pending;
            total.in_flight = pass.in_flight;
            total.bytes = pass.bytes;
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
            // push and the rollup downlink are both one-shot calls, so a
            // delivered-or-skipped push IS complete catch-up, and a failed
            // one surfaces as a pass error with the backoff below instead of
            // a fast continuation loop. Every pass here is a timed pass —
            // it ingests, polls and sleeps the full (jittered) interval.
            let started = std::time::Instant::now();
            let interval = idle_interval(self.config.poll_interval, self.primary_download_preparing, pass_had_error, consecutive_errors);
            self.primary_download_complete = false;
            while started.elapsed() < interval && !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(50).min(interval));
            }
        }
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
            let summary = self.store.outbox_summary()?;
            report.pending = summary.pending;
            report.in_flight = summary.in_flight;
            report.bytes = summary.bytes;
            return Ok(report);
        }
        // The uplink push already ran inside `prepare_secondary`; this half
        // owns the downlink. The WebSocket stays resident across passes: the
        // primary admits four concurrent connections, so the held slot no
        // longer starves the status calls, and a LAN keepalive link beats a
        // fresh TCP+TLS+WS handshake per pass.
        //
        // Rollup-level downlink: one authenticated full-snapshot fetch per
        // ROLLUP_FETCH_INTERVAL replaces the row-level page walk entirely —
        // a 500k-event primary lands in one ~300 KB response, so there is
        // no backfill and nothing to resume.
        if self.secondary_identity.is_some() && self.primary_data_sharing {
            let due = self.last_rollup_fetch.map_or(true, |at| {
                now_ms().saturating_sub(at) >= ROLLUP_FETCH_INTERVAL_MS
            });
            if due {
                match self.fetch_primary_rollup(now_ms()) {
                    Ok(()) => {
                        self.primary_download_complete = true;
                        self.last_rollup_fetch = Some(now_ms());
                    }
                    Err(error) => {

                        let message = format!("primary rollup sync failed: {error}");
                        let _ = self.store.set_secondary_last_error(Some(&message), now_ms());
                        report.last_error = Some(message);
                    }
                }
            } else {
                self.primary_download_complete = true;
            }
        }
        let summary = self.store.outbox_summary()?;
        report.pending = summary.pending;
        report.in_flight = summary.in_flight;
        report.bytes = summary.bytes;
        if self.secondary_identity.is_some() && report.last_error.is_none() {
            let _ = self.store.set_secondary_last_error(None, now_ms());
        }
        Ok(report)
    }

    /// One authenticated full-snapshot fetch of the primary's own rollup
    /// rows, applied as a full replace for the primary's origin label. This
    /// is the whole downlink: no pages, no cursor, no backfill.
    fn fetch_primary_rollup(&mut self, now_ms: i64) -> SyncResult<()> {
        let identity = self.secondary_identity.clone().ok_or_else(|| SyncError::Security("primary rollup sync requires a paired secondary identity".into()))?;
        let request = AuthenticatedRollupRequest::new_at(&identity, self.store.primary_id().to_string(), now_ms)?;
        self.transport.ws_close();
        let snapshot = self.transport.rollup_snapshot(&request).map_err(|error| match error {
            TransportError::Unsupported(message) => SyncError::Unsupported(message),
            error => SyncError::Protocol(format!("primary rollup request failed: {error}")),
        })?;
        self.store.apply_remote_rollup(&snapshot.origin, &snapshot.rows, now_ms)?;
        Ok(())
    }

    /// The status-and-pairing preamble every pass runs, closed by the uplink
    /// push. Returns the rollup rows the push carried — 0 when the pass had
    /// nothing paired to do or the fingerprint matched the last delivered
    /// push. Errors are the caller's record-and-backoff input.
    fn prepare_secondary(&mut self, now_ms: &mut impl FnMut() -> i64) -> SyncResult<u64> {
        let Some(identity) = self.secondary_identity.clone() else { return Ok(0); };
        let config = self.store.secondary_config()?.ok_or_else(|| SyncError::NotPaired(identity.device_id.clone()))?;
        if !config.enabled {
            // Stop is the network scheduler switch.  A paired device keeps
            // rolling local rows up while offline so resume cannot skip
            // changes made before the last push.
            return Ok(0);
        }
        // A device that has a stream or ack history has synced before; a
        // fresh pairing has neither, and must not be forced to reconcile
        // just because it never minted a stream (new pairings never do).
        let local_has_history = if config.stream_id.is_some() {
            true
        } else {
            self.store.secondary_has_history(&config)?
        };
        let request_now = now_ms();
        let request = AuthenticatedStatusRequest::new_at_with_checkpoint(
            &identity, config.primary_id.clone(), true, config.last_ack_generation, request_now,
        )?;
        let status_result = self.transport.status(&request).map_err(map_secondary_status_error);
        // A checkpoint the primary no longer has a receipt for makes the
        // status call itself fail with 409.  The primary is reachable, and
        // that answer is precisely the recovery handshake's input.
        if matches!(&status_result, Ok(_) | Err(SyncError::ReconcileRequired)) {
            self.store.mark_secondary_connected(now_ms())?;
        }
        let status = match status_result {
            Ok(status) => Some(status),
            Err(SyncError::ReconcileRequired) => None,
            Err(error) => return Err(error),
        };
        // A local reset deliberately forgot the stream while the primary still
        // holds its highwater: adopting that stream again would replay
        // generation 1 against a stream expecting N+1 forever. The recovery
        // handshake freezes it and mints a fresh one instead.
        let mut recover = status.is_none() || self.store.secondary_needs_recovery()?;
        if let Some(status) = status.as_ref() {
            self.primary_data_sharing = status.primary_data_sharing;
            if let Some(name) = status.primary_display_name.as_deref() {
                if config.primary_display_name.as_deref() != Some(name) {
                    if let Err(error) = self.store.set_secondary_primary_display_name(name, now_ms()) {
                        eprintln!("tokenme sync: primary display-name cache failed: {error}");
                    }
                }
            }
            match status.status.as_str() {
                "paired" => {}
                "pending" => return Err(SyncError::NotPaired("pairing request is awaiting approval".into())),
                "rejected" => return Err(SyncError::NotPaired("pairing request was rejected".into())),
                "expired" => return Err(SyncError::NotPaired("pairing invite expired".into())),
                "paused" => return Err(SyncError::Conflict("primary paused this device".into())),
                "revoked" => return Err(SyncError::Revoked(identity.device_id.clone())),
                "reconcile_required" => recover = true,
                other => return Err(SyncError::Protocol(format!("primary returned unknown device status {other:?}"))),
            }
            // A cursor, stream or epoch the primary no longer recognizes is
            // the recovery handshake's job, not a terminal error: the primary
            // freezes the old stream and issues a fresh one.  Any other
            // validation failure is reported as-is.
            recover = recover || match validate_secondary_status(&config, status, local_has_history) {
                Ok(()) => false,
                Err(SyncError::ReconcileRequired) | Err(SyncError::EpochStale) => true,
                Err(error) => return Err(error),
            };
            if let Some(display_name) = self.secondary_display_name.clone() {
                if status.display_name.as_deref().is_some_and(|current| current != display_name) {
                    match AuthenticatedDisplayNameRequest::new_at(&identity, config.primary_id.clone(), display_name, now_ms()) {
                        Ok(update) => if let Err(error) = self.transport.update_display_name(&update) {
                            eprintln!("tokenme sync: display-name refresh failed: {error}");
                        },
                        Err(error) => eprintln!("tokenme sync: display-name refresh request was rejected locally: {error}"),
                    }
                }
            }
        }
        let receipt = if recover {
            self.transport.recover(&request).map_err(|error| SyncError::Protocol(format!("secondary stream recovery failed: {error}")))?
        } else {
            let status = status.as_ref().expect("a non-recovering status pass has a status");
            match (status.active_stream_id.clone(), status.expected_generation) {
                (Some(stream_id), Some(expected_generation)) => StreamOpenReceipt {
                    primary_id: status.primary_id.clone(),
                    device_id: status.device_id.clone(),
                    stream_id,
                    data_epoch: status.data_epoch,
                    expected_generation,
                },
                (None, None) => self.transport.recover(&request).map_err(|error| SyncError::Protocol(format!("secondary stream recovery failed: {error}")))?,
                _ => return Err(SyncError::Security("primary returned an incomplete stream status".into())),
            }
        };
        let stream_now = now_ms();
        if !self.renew_lease_if_due(stream_now)? {
            return Err(SyncError::Security("sync scheduler lease was lost before stream update".into()));
        }
        if recover {
            self.store.adopt_recovered_stream(&receipt, stream_now)?;
        } else {
            self.store.update_secondary_stream(&receipt, stream_now)?;
        }
        // The uplink: one authenticated rollup-snapshot push, riding the
        // settled session — a pending/paused/revoked device never gets here,
        // and a fresh pairing (no stream ever minted) pushes right after its
        // first status pass. A delivered-or-skipped push is the whole
        // catch-up: the pass that returns from here owes nothing to the
        // next one.
        self.push_local_rollup(now_ms())
    }

    fn remember_secondary_error(&mut self, error: &SyncError, now_ms: i64) {
        if self.secondary_identity.is_none() { return; }
        let message = error.to_string();
        let _ = self.store.set_secondary_last_error(Some(&message), now_ms);
        if matches!(error, SyncError::Revoked(_) | SyncError::Security(_))
            || matches!(error, SyncError::NotPaired(message) if message == "pairing request was rejected" || message == "pairing invite expired")
        {
            let _ = self.store.set_secondary_enabled(false, now_ms);
            // The primary cut this device off (revoked or blocked): the panel
            // polls on its own cadence, so wake the owner directly — the same
            // door-knock courtesy the primary's pairing hook provides.
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
    /// gate. Returns the number of rows the delivered push carried (0 when
    /// skipped as unchanged).
    fn push_local_rollup(&mut self, now_ms: i64) -> SyncResult<u64> {
        let fingerprint = self.local_rollup_fingerprint()?;
        if self.last_upload_fingerprint == Some(fingerprint) {
            return Ok(0);
        }
        let rows = self.store.local_rollup_snapshot()?;
        // Error handling is the old outbox drain's: a version-incompatible
        // primary is disabled — resetting would loop reset → re-push → 426
        // forever, so only an upgrade fixes it — a broken stream state is
        // auto-recovered by resetting the local stream (the next pass's
        // recovery handshake mints a fresh one), and anything else goes back
        // as a pass error for the caller's record-and-backoff paths.
        match self.transport.ws_rollup_sync(&rows) {
            Ok(_origin) => {
                self.last_upload_fingerprint = Some(fingerprint);
                self.last_upload_pushed_at = Some(now_ms);
                Ok(rows.len() as u64)
            }
            Err(error) => {
                let message = error.to_string();
                if is_version_incompatible_error(&message) {
                    let _ = self.store.reset_secondary_sync_state(now_ms);
                    let _ = self.store.set_secondary_enabled(false, now_ms);
                    let _ = self.store.set_secondary_last_error(Some(&message), now_ms);
                    return Err(SyncError::Protocol(message));
                }
                if is_unrecoverable_sync_error(&message) {
                    let _ = self.store.reset_secondary_sync_state(now_ms);
                    return Err(SyncError::Protocol(format!("auto-recovering: {message}")));
                }
                Err(SyncError::Protocol(format!("rollup push failed: {message}")))
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
}

fn validate_secondary_status(config: &crate::SecondarySyncConfig, status: &AuthenticatedStatus, local_has_history: bool) -> SyncResult<()> {
    if config.data_epoch.is_some_and(|epoch| epoch != status.data_epoch) {
        return Err(SyncError::EpochStale);
    }
    match (config.stream_id.as_deref(), status.active_stream_id.as_deref()) {
        (Some(local), Some(remote)) if local != remote => return Err(SyncError::ReconcileRequired),
        (Some(_), None) => return Err(SyncError::ReconcileRequired),
        (None, Some(_)) if local_has_history => return Err(SyncError::ReconcileRequired),
        _ => {}
    }
    if let Some(last_ack_generation) = config.last_ack_generation {
        if status.checkpoint_generation != Some(last_ack_generation)
            || status.checkpoint_receipt_hash != config.last_ack_receipt_hash
        {
            return Err(SyncError::ReconcileRequired);
        }
        let remote_generation = status.highwater_generation.unwrap_or(0);
        if remote_generation < last_ack_generation {
            return Err(SyncError::ReconcileRequired);
        }
        if remote_generation == last_ack_generation && status.highwater_receipt_hash != config.last_ack_receipt_hash {
            return Err(SyncError::ReconcileRequired);
        }
    } else if status.checkpoint_generation.is_some() || status.checkpoint_receipt_hash.is_some() {
        return Err(SyncError::ReconcileRequired);
    }
    Ok(())
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

/// A primary (or secondary) on an incompatible schema version answers 426
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

fn map_secondary_status_error(error: TransportError) -> SyncError {
    match error {
        TransportError::HttpStatus { status: 409, message } if message.contains("reconciliation is required") => SyncError::ReconcileRequired,
        TransportError::Unsupported(message) => SyncError::Unsupported(message),
        error => SyncError::Protocol(format!("secondary status request failed: {error}")),
    }
}

/// Export the complete local-origin projection used by the secondary runtime.
/// A full projection keeps keyless multiplicity deterministic across rebuilds;
/// remote `linux:` rows are excluded so a primary never receives a re-export.
pub fn export_index_snapshot(index: &Index, origin: &str, days: i64, now_ms: i64) -> SyncResult<LocalSnapshot> {
    let mut snapshots = export_index_snapshots(index, origin, days, now_ms)?;
    if snapshots.len() != 1 {
        return Err(SyncError::Limit(format!("snapshot contains {} segments; use export_index_snapshots", snapshots.len())));
    }
    Ok(snapshots.remove(0))
}

/// Builds deterministic, bounded package segments from one Index read view.
/// Dirty rows outside the initial rolling coverage are added to the same view,
/// so a long-offline device cannot lose a late call-only update.
pub fn export_index_snapshots(index: &Index, origin: &str, days: i64, now_ms: i64) -> SyncResult<Vec<LocalSnapshot>> {
    if days <= 0 || days > 3650 { return Err(SyncError::Invalid("snapshot days must be in 1..=3650".into())); }
    if origin.is_empty() || origin.contains(':') || origin.contains('/') || origin.contains('\\') {
        return Err(SyncError::Invalid("snapshot origin is not safe".into()));
    }
    let snapshot = index.sync_snapshot(now_ms, days).map_err(SyncError::from)?;
    export_index_snapshots_from_snapshot(index, origin, snapshot)
}

pub fn export_index_snapshots_from_snapshot(index: &Index, origin: &str, snapshot: usage_index::SyncSnapshot) -> SyncResult<Vec<LocalSnapshot>> {
    let snapshot_hi_ms = snapshot.coverage_hi_ms;
    let occurrence_ids = index.sync_occurrence_ids(&snapshot.events).map_err(SyncError::from)?;
    let mut rows_by_identity = BTreeMap::<String, NetworkRow>::new();
    for (event, occurrence_id) in snapshot.events.into_iter().zip(occurrence_ids) {
        let row = network_row(event, occurrence_id)?;
        rows_by_identity.insert(row.identity_material()?.to_string(), row);
    }
    for dirty in snapshot.dirty_events {
        let row = network_row(dirty.event, dirty.occurrence_id)?;
        rows_by_identity.insert(row.identity_material()?.to_string(), row);
    }
    let mut rows: Vec<NetworkRow> = rows_by_identity.into_values().collect();
    rows.sort_by(|left, right| left.identity_material().unwrap_or("").cmp(right.identity_material().unwrap_or("")));
    const SEGMENT_ROWS: usize = 50_000;
    let window_lo_ms = rows.iter().map(|row| row.ts_ms).min().unwrap_or(snapshot.coverage_lo_ms).min(snapshot.coverage_lo_ms);
    let window_hi_ms = rows.iter().map(|row| row.ts_ms.saturating_add(1)).max().unwrap_or(snapshot.coverage_hi_ms).max(snapshot.coverage_hi_ms);
    let mut out = Vec::new();
    for chunk in rows.chunks(SEGMENT_ROWS) {
        let generated_at_ms = chunk.iter().map(|row| row.ts_ms).max().unwrap_or(window_lo_ms);
        let package = NetworkPackage::from_rows(origin, usage_index::INDEX_SCHEMA_VERSION, window_lo_ms, window_hi_ms, chunk, generated_at_ms)?;
        out.push(LocalSnapshot { package, rows: chunk.to_vec() });
    }
    if out.is_empty() {
        let package = NetworkPackage::from_rows(origin, usage_index::INDEX_SCHEMA_VERSION, window_lo_ms, window_hi_ms, &[], snapshot_hi_ms)?;
        out.push(LocalSnapshot { package, rows: Vec::new() });
    }
    Ok(out)
}

pub(crate) fn network_row(event: UsageEvent, occurrence_id: Option<String>) -> SyncResult<NetworkRow> {
    let key = event.dedupe_key.filter(|key| !key.is_empty());
    if key.is_none() && occurrence_id.as_deref().is_none_or(str::is_empty) {
        return Err(SyncError::UnstableOccurrence);
    }
    let mut calls = event.calls.into_iter().map(|call| NetworkCall {
        kind: match call.kind { CallKind::Mcp => "mcp", CallKind::Skill => "skill" }.into(),
        name: call.name,
    }).collect::<Vec<_>>();
    calls.sort_by(|left, right| left.kind.cmp(&right.kind).then(left.name.cmp(&right.name)));
    Ok(NetworkRow {
        key, occurrence_id, tool: event.tool, ts_ms: event.ts_ms, session: event.session,
        project: event.project, model: event.model,
        meter: match event.meter { Meter::Tokens => "tokens", Meter::Credits => "credits" }.into(),
        source: if event.source.is_empty() { "local".into() } else { event.source },
        counts: NetworkCounts {
            in_tok: Some(event.counts.input), cc_tok: Some(event.counts.cache_creation),
            cr_tok: Some(event.counts.cache_read), out_tok: Some(event.counts.output),
            reason_tok: Some(event.counts.reasoning), credits: Some(event.counts.credits),
        },
        calls,
    })
}

fn idle_interval(poll_interval: Duration, preparing: bool, pass_had_error: bool, consecutive_errors: u32) -> Duration {
    let base = if preparing && !pass_had_error {
        PREPARING_POLL.min(poll_interval)
    } else {
        poll_interval.saturating_mul(1_u32 << consecutive_errors.min(6))
    };
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

    fn paired_primary(device: &DeviceIdentity) -> (SyncStore, crate::StreamOpenReceipt) {
        let mut primary = paired_primary_without_stream(device);
        let stream = primary.open_stream_for_registered(&device.device_id, 3).unwrap();
        (primary, stream)
    }

    fn server_config(cert_der: &[u8], key_der: &[u8], staging_dir: std::path::PathBuf) -> HttpsServerConfig {
        HttpsServerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            certificate_der: cert_der.to_vec(), private_key_der: key_der.to_vec(),
            allow_lan: false, allowed_peer_ips: vec![], max_body_bytes: MAX_COMPRESSED_BYTES,
            // Matches the panel's primary: the resident upload WebSocket holds
            // one slot while the same pass's status/download calls take another.
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
    fn preparing_downloads_poll_quickly_while_failures_back_off_to_the_cap() {
        let preparing = idle_interval(Duration::from_secs(5), true, false, 0);
        assert!(preparing >= Duration::from_millis(400) && preparing <= Duration::from_millis(600), "{preparing:?}");
        let capped_by_user_poll = idle_interval(Duration::from_millis(200), true, false, 0);
        assert!(capped_by_user_poll <= Duration::from_millis(240), "{capped_by_user_poll:?}");
        // A failing primary is not a preparing primary: pass_had_error wins.
        let failing_while_preparing = idle_interval(Duration::from_secs(5), true, true, 0);
        assert!(failing_while_preparing >= Duration::from_secs(4), "{failing_while_preparing:?}");
        let capped = idle_interval(Duration::from_secs(60), false, true, 10);
        assert!(capped >= Duration::from_secs(240) && capped <= Duration::from_secs(360), "{capped:?}");
        // The default 5s cadence must still reach the documented 5-minute cap.
        let default_capped = idle_interval(Duration::from_secs(5), false, true, 10);
        assert!(default_capped >= Duration::from_secs(240) && default_capped <= Duration::from_secs(360), "{default_capped:?}");
    }

    #[test]
    fn secondary_runtime_syncs_primary_rollup_over_loopback_tls() {
        let device = DeviceIdentity::create("runtime-downlink", "Secondary").unwrap();
        let (mut primary, _) = paired_primary(&device);
        let now = usage_core::report::now_ms();
        primary.save_primary_config(&PrimarySyncConfig {
            auto_approve_pairing: false,
            primary_id: "primary-1".into(), bind_addr: "127.0.0.1:47821".into(),
            advertise_host: Some("127.0.0.1".into()), allow_lan: false, enabled: true,
            share_data_with_secondaries: true,
        }, now).unwrap();
        // The primary's own rollup rows, plus a foreign-origin row that must
        // never travel: the export serves local-origin rows only.
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
        assert_eq!(report.acknowledged, 0, "the push's Ok is the ack; there is no separate bookkeeping");
        assert!(report.last_error.is_none());
        assert!(runtime.primary_download_complete, "one rollup snapshot completes the downlink");
        let (origin, in_tok): (String, f64) = runtime.store().index().conn().query_row(
            "SELECT origin, in_tok FROM remote_rollup", [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        ).unwrap();
        assert_eq!(origin, usage_index::hostname());
        assert_eq!(in_tok, 9.0);
        assert_eq!(runtime.store().index().event_count().unwrap(), 0, "raw event rows are no longer downloaded");
        let config = runtime.store().secondary_config().unwrap().unwrap();
        assert_eq!(config.last_error, None);
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
        assert_eq!(report.acknowledged, 0, "the push's Ok is the ack; there is no separate bookkeeping");
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

    #[test]
    fn remote_highwater_rollback_or_same_generation_hash_change_requires_reconcile() {
        let mut config = SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: "device-1".into(), origin: "origin-1".into(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: vec![1], certificate_pin: hash_bytes(&[1]),
            identity_path: std::path::PathBuf::from("identity.json"), stream_id: Some("stream-1".into()), data_epoch: Some(1), expected_generation: Some(3), enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: Some(2), last_ack_receipt_hash: Some([7; 32]),
        };
        let status = |generation, receipt_hash| AuthenticatedStatus {
            primary_id: "primary-1".into(), device_id: "device-1".into(), origin: "origin-1".into(), request_nonce: [0; 16], status: "paired".into(),
            data_epoch: 1, active_stream_id: Some("stream-1".into()), expected_generation: Some(3), last_generation: generation,
            highwater_generation: generation, highwater_receipt_hash: receipt_hash,
            checkpoint_generation: Some(2), checkpoint_receipt_hash: Some([7; 32]),
            display_name: None, primary_display_name: None, primary_data_sharing: false,
            coverage_lo_ms: None, coverage_hi_ms: None, observed_at_ms: 0,
        };
        assert!(matches!(validate_secondary_status(&config, &status(Some(1), Some([7; 32])), true), Err(SyncError::ReconcileRequired)));
        assert!(matches!(validate_secondary_status(&config, &status(Some(2), Some([8; 32])), true), Err(SyncError::ReconcileRequired)));
        assert!(validate_secondary_status(&config, &status(Some(2), Some([7; 32])), true).is_ok());
        assert!(validate_secondary_status(&config, &status(Some(3), Some([9; 32])), true).is_ok());
        config.stream_id = Some("other-stream".into());
        assert!(matches!(validate_secondary_status(&config, &status(Some(2), Some([7; 32])), true), Err(SyncError::ReconcileRequired)));
    }

    #[test]
    fn first_pairing_adopts_primary_stream_without_local_history() {
        let config = SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: "device-1".into(), origin: "origin-1".into(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: vec![1], certificate_pin: hash_bytes(&[1]),
            identity_path: std::path::PathBuf::from("identity.json"), stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        };
        let status = AuthenticatedStatus {
            primary_id: "primary-1".into(), device_id: "device-1".into(), origin: "origin-1".into(), request_nonce: [0; 16], status: "paired".into(),
            data_epoch: 1, active_stream_id: Some("primary-stream".into()), expected_generation: Some(1), last_generation: None,
            highwater_generation: None, highwater_receipt_hash: None, checkpoint_generation: None, checkpoint_receipt_hash: None,
            display_name: None, primary_display_name: None, primary_data_sharing: false,
            coverage_lo_ms: None, coverage_hi_ms: None, observed_at_ms: 0,
        };
        assert!(validate_secondary_status(&config, &status, false).is_ok());
        assert!(matches!(validate_secondary_status(&config, &status, true), Err(SyncError::ReconcileRequired)));
    }

    #[test]
    fn secondary_status_reconciliation_response_disables_the_secondary() {
        let error = map_secondary_status_error(TransportError::HttpStatus {
            status: 409,
            message: "reconciliation is required before retrying this package".into(),
        });
        assert!(matches!(error, SyncError::ReconcileRequired));
    }

    #[test]
    fn unrelated_secondary_status_conflicts_remain_retryable_protocol_errors() {
        let error = map_secondary_status_error(TransportError::HttpStatus {
            status: 409,
            message: "primary paused this device".into(),
        });
        assert!(matches!(error, SyncError::Protocol(message) if message.contains("HTTP status 409")));
    }
}

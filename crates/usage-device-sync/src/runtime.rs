//! The single-owner outbox runtime used by CLI, Panel and a user service.
//!
//! It deliberately owns no timer thread by itself: callers choose `run_once`
//! or `run_until`, while the SQLite lease is the process-wide singleton gate.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use usage_core::{CallKind, Meter, UsageEvent};
use usage_index::Index;

use crate::{
    AuthenticatedDisplayNameRequest, AuthenticatedDownloadRequest, AuthenticatedRollupRequest, AuthenticatedStatus,
    AuthenticatedStatusRequest, DeviceIdentity, DirtyJournalInput,
    NetworkCall, NetworkCounts, NetworkPackage, NetworkRow, StreamOpenReceipt,
    ClientTransportCancellation, SyncError, SyncResult, SyncStore, Tls13HttpsTransport,
    TransportError,
};

// Keep the scheduler lease longer than the transport's 10-minute request
// deadline. A live upload must not become reclaimable by a second process
// while its owner is still waiting for the ACK.
const DEFAULT_LEASE_TTL_MS: i64 = 15 * 60 * 1000;
const DEFAULT_POLL_MS: u64 = 5_000;
const MAX_SNAPSHOT_PAGES_PER_PASS: usize = 8;
// Snapshot ENQUEUING per pass. The outbox's own bounds (4,096 entries / 1 GB)
// are the real backpressure; the old "stop at the first pending entry" gate
// serialized the initial backfill to one 256-row generation per pass — 147
// passes / 3.5 h measured on a seeded Windows secondary. 64 pages stream in
// one WS burst; the outbox Limit still stops preparation when full.
const SNAPSHOT_ENQUEUE_PAGES_PER_PASS: usize = 64;
// A download burst used to hold the index writer almost continuously: every
// page imports in one BEGIN IMMEDIATE (rows + rollups + cursor) and the
// catch-up loop starts the next pass with no poll sleep, so the panel
// engine's publish cycle timed out its 5 s busy wait with "database is
// locked" for the whole burst. After each imported page this sleep hands
// the writer to whoever is waiting — an engine-yield, not a rate limit:
// four pages per pass pay at most 600 ms of it against per-page network
// fetches and the five-second poll cadence.
const DOWNLOAD_PAGE_ENGINE_YIELD: Duration = Duration::from_millis(50);
// A fresh backup can owe hundreds of thousands of identity rows. One 256-row
// batch per five-second poll left the first sync materializing for half an
// hour on a 500k-event index (~200 rows/s), so each pass runs many larger
// batches with an engine-yield between them — the same courtesy the download
// burst pays: ~5k rows/s while the panel's publish cycle still wins the
// writer inside every yield window.
const HISTORY_INIT_BATCHES_PER_PASS: usize = 16;
// Matches the raised materialization batch cap: fewer, denser transactions
// beat many small ones while the 100ms yield still hands the writer over.
const HISTORY_INIT_BATCH_ROWS: usize = 2048;
const HISTORY_INIT_ENGINE_YIELD: Duration = Duration::from_millis(100);
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRunReport {
    pub claimed: u64,
    pub uploaded: u64,
    pub acknowledged: u64,
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
    primary_download_progressed: bool,
    primary_download_more: bool,
    primary_download_preparing: bool,
    /// Embedding-UI wake for the moment the primary cuts this device off
    /// (revoked/blocked) and the runtime self-disables. Receives a short
    /// reason tag; the panel turns it into a toast.
    on_cut_off: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    primary_download_complete: bool,
    occurrences_initialized: bool,
    last_rollup_fetch: Option<i64>,
    occurrence_init_progressed: bool,
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
        Self { store, transport, config, secondary_identity: None, secondary_display_name: None, primary_data_sharing: false, primary_download_progressed: false, primary_download_more: false, primary_download_preparing: false, primary_download_complete: false, occurrences_initialized: false, occurrence_init_progressed: false, lease_renewed_at_ms: 0, last_rollup_fetch: None, on_cut_off: None }
    }

    pub fn new_secondary(
        store: SyncStore,
        transport: Tls13HttpsTransport,
        config: SyncRuntimeConfig,
        identity: DeviceIdentity,
    ) -> Self {
        Self { store, transport, config, secondary_identity: Some(identity), secondary_display_name: None, primary_data_sharing: false, primary_download_progressed: false, primary_download_more: false, primary_download_preparing: false, primary_download_complete: false, occurrences_initialized: false, occurrence_init_progressed: false, lease_renewed_at_ms: 0, last_rollup_fetch: None, on_cut_off: None }
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
    /// `run_once`, so ACK/retry/lease checks never reuse the pre-upload time.
    pub fn run_once_with_clock(&mut self, mut now_ms: impl FnMut() -> i64) -> SyncResult<RuntimeRunReport> {
        let first_now = now_ms();
        if !self.store.acquire_scheduler_lease(&self.config.owner, first_now, self.config.lease_ttl_ms)? {
            return Err(SyncError::Security("another sync runtime owns the scheduler lease".into()));
        }
        self.lease_renewed_at_ms = first_now;
        let result = (|| {
            self.store.recover_inflight_outbox(&self.config.owner, first_now)?;
            let _ = self.prepare_secondary(&mut now_ms)?;
            // A standalone pass must ask the primary again; only the resident
            // scheduler may skip a downlink it already finished in this burst.
            self.primary_download_complete = false;
            self.run_owned_once(None, &mut now_ms)
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
        if let Err(error) = self.store.recover_inflight_outbox(&self.config.owner, first_now) {
            let _ = self.store.release_scheduler_lease(&self.config.owner);
            return Err(error);
        }
        let mut total = RuntimeRunReport::default();
        let mut consecutive_errors = 0_u32;
        // The first pass and every timed pass ingest local sources; the
        // bounded catch-up continuations skip it. During an initial-snapshot
        // backfill no new local bytes exist to read, and on a large index the
        // scan dominated every continuation — one snapshot generation per
        // ~20 s measured on a seeded Windows secondary.
        let mut timed_pass = true;
        while !stop.load(Ordering::Acquire) {
            self.occurrence_init_progressed = false;
            self.primary_download_progressed = false;
            self.primary_download_more = false;
            self.primary_download_preparing = false;
            let lease_now = now_ms();
            if !self.renew_lease_if_due(lease_now)? {
                let _ = self.store.release_scheduler_lease(&self.config.owner);
                return Err(SyncError::Security("sync scheduler lease was lost before preparing the next pass".into()));
            }
            if timed_pass {
                if let Err(error) = ingest(&mut self.store) {
                    let _ = self.store.release_scheduler_lease(&self.config.owner);
                    return Err(error);
                }
            }
            let mut catchup_complete = true;
            let pass = match self.prepare_secondary(&mut now_ms) {
                Ok(complete) => {
                    catchup_complete = complete;
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
            catchup_complete &= !self.primary_download_more;
            total.claimed = total.claimed.saturating_add(pass.claimed);
            total.uploaded = total.uploaded.saturating_add(pass.uploaded);
            total.acknowledged = total.acknowledged.saturating_add(pass.acknowledged);
            total.retried = total.retried.saturating_add(pass.retried);
            total.pending = pass.pending;
            total.in_flight = pass.in_flight;
            total.bytes = pass.bytes;
            let pass_had_error = pass.last_error.is_some();
            let waiting_for_primary = pass.last_error.as_deref().is_some_and(|message| message.contains("awaiting approval") || message.contains("primary paused this device"));
            consecutive_errors = if pass_had_error && !waiting_for_primary { consecutive_errors.saturating_add(1) } else { 0 };
            let pass_had_no_claim_with_pending = pass.claimed == 0 && pass.pending > 0;
            let pass_had_occurrence_progress = self.occurrence_init_progressed;
            let pass_had_primary_download_progress = self.primary_download_progressed;
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
            // Continue bounded local or remote snapshot catch-up immediately;
            // idle passes stay on the normal poll cadence. A continuation
            // skips the ingest scan (see `timed_pass` above): nothing new can
            // have landed mid-backfill, and the scan was the dominant cost of
            // every continuation pass.
            if !catchup_complete
                && (pass.acknowledged > 0 || pass_had_occurrence_progress || pass_had_primary_download_progress)
                && !pass_had_error
                && !pass_had_no_claim_with_pending
            {
                timed_pass = false;
                continue;
            }
            timed_pass = true;
            let started = std::time::Instant::now();
            let interval = idle_interval(self.config.poll_interval, self.primary_download_preparing, pass_had_error, consecutive_errors);
            // The timed pass is the one that discovers new primary data; the
            // fast catch-up continuations above must not re-poll a downlink
            // that already reported itself complete in this burst.
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
        if self.secondary_identity.is_some()
            && self.store.secondary_config()?.is_some_and(|config| !config.enabled)
        {
            let summary = self.store.outbox_summary()?;
            report.pending = summary.pending;
            report.in_flight = summary.in_flight;
            report.bytes = summary.bytes;
            return Ok(report);
        }
        // The WS leg lives exactly for this drain: opened for the first push,
        // reused across the burst, and closed on every exit — a primary that
        // admits one connection at a time must see the slot again before the
        // pass's own status/download calls.
        let drain = 'drain: {
            loop {
                if stop.is_some_and(|flag| flag.load(Ordering::Acquire)) { break; }
                let claim_now = now_ms();
                let entries = self.store.claim_outbox(&self.config.owner, claim_now, 1)?;
                if entries.is_empty() { break; }
                report.claimed += entries.len() as u64;
                let entry = &entries[0];
                if stop.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                    self.store.retry_outbox(&self.config.owner, entry.id, now_ms(), "sync stopped before upload")?;
                    break;
                }
                // The resident WebSocket is the only upload path. The primary's
                // import is idempotent per envelope — a package whose
                // manifest+gzip hashes already have a receipt returns that
                // receipt without re-importing — so a push the primary
                // imported before its ack was lost is safely re-sent.
                let upload = entry.signed_upload();
                let result = self.transport.ws_upload(&upload);
                match result {
                    Ok(receipt) => {
                        report.uploaded += 1;
                        let commit_now = now_ms();
                        if !self.renew_lease_if_due(commit_now)? {
                            break 'drain Err(SyncError::Security("sync scheduler lease was lost before ACK".into()));
                        }
                        self.store.ack_outbox(&self.config.owner, entry.id, &receipt, commit_now)?;
                        report.acknowledged += 1;
                    }
                    Err(error) => {
                        let message = error.to_string();
                        // A version-incompatible primary would just 426 the
                        // re-snapshot too: resetting here used to loop
                        // reset → full snapshot → 426 forever. Surface the
                        // version copy instead and wait for an upgrade.
                        if is_version_incompatible_error(&message) && self.secondary_identity.is_some() {
                            let _ = self.store.reset_secondary_sync_state(now_ms());
                            let _ = self.store.set_secondary_enabled(false, now_ms());
                            let _ = self.store.set_secondary_last_error(Some(&message), now_ms());
                            report.last_error = Some(message.clone());
                            break;
                        }
                        let fatal = is_unrecoverable_sync_error(&message) || entry.attempts >= 4;
                        if fatal && self.secondary_identity.is_some() {
                            let _ = self.store.reset_secondary_sync_state(now_ms());
                            report.retried += 1;
                            report.last_error = Some(format!("auto-recovering: {message}"));
                            break;
                        }
                        self.store.retry_outbox(&self.config.owner, entry.id, now_ms(), &message)?;
                        report.retried += 1;
                        report.last_error = Some(message);
                        break;
                    }
                }
            }
            Ok(())
        };
        // The WebSocket stays resident across passes: the primary admits four
        // concurrent connections, so the held slot no longer starves the
        // status/download calls, and a LAN keepalive link beats a fresh
        // TCP+TLS+WS handshake per pass. `try_ws_upload` re-authenticates
        // nothing — the socket is already authenticated — and rebuilds the
        // leg itself if a send or read ever fails.
        drain?;
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
                        self.primary_download_progressed = true;
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

    fn download_primary_pages(&mut self, stop: Option<&AtomicBool>, now_ms: &mut impl FnMut() -> i64) -> SyncResult<bool> {
        let identity = self.secondary_identity.clone().ok_or_else(|| SyncError::Security("primary download requires a paired secondary identity".into()))?;
        for _ in 0..MAX_SNAPSHOT_PAGES_PER_PASS {
            if stop.is_some_and(|flag| flag.load(Ordering::Acquire)) { return Ok(false); }
            let cursor = self.store.primary_download_cursor()?;
            let request = AuthenticatedDownloadRequest::new_at(
                &identity, self.store.primary_id().to_string(), cursor.last_identity.clone(),
                cursor.last_dirty_revision, now_ms(),
            )?;
            let page = self.transport.download_primary_page(&request).map_err(|error| match error {
                TransportError::Unsupported(message) => SyncError::Unsupported(message),
                error => SyncError::Protocol(format!("primary download request failed: {error}")),
            })?;
            if !page.enabled {
                self.primary_data_sharing = false;
                return Ok(true);
            }
            self.store.apply_primary_download_page(&page, now_ms())?;
            // Hand the index writer to a waiting publish cycle between imports
            // (see DOWNLOAD_PAGE_ENGINE_YIELD): a preparing page wrote nothing
            // and a complete page ends the burst, so only a real import sleeps.
            if !page.preparing && !page.complete { std::thread::sleep(DOWNLOAD_PAGE_ENGINE_YIELD); }
            // A preparing page is an empty marker: the primary is still
            // materializing identities and the cursor did not move, so this
            // pass made no catch-up progress.
            self.primary_download_preparing = page.preparing;
            if !page.preparing { self.primary_download_progressed = true; }
            if page.complete { self.primary_download_complete = true; return Ok(true); }
        }
        Ok(false)
    }

    fn prepare_secondary(&mut self, now_ms: &mut impl FnMut() -> i64) -> SyncResult<bool> {
        let Some(identity) = self.secondary_identity.clone() else { return Ok(true); };
        let config = self.store.secondary_config()?.ok_or_else(|| SyncError::NotPaired(identity.device_id.clone()))?;
        if !config.enabled {
            // Stop is the network scheduler switch.  A paired device keeps
            // capturing dirty revisions while offline so resume cannot skip
            // changes made before the last ACK.
            return Ok(true);
        }
        // Never build more pages while older immutable bytes are waiting for
        // ACK.  This is the backpressure valve that lets the sender drain a
        // bounded outbox instead of filling it during one preparation loop.
        let summary = self.store.outbox_summary()?;
        let has_pending = summary.pending > 0 || summary.in_flight > 0;
        let had_stream = config.stream_id.is_some();
        let local_has_history = if had_stream { true } else { self.store.secondary_has_history(&config)? };
        let mut catchup_complete = true;
        if had_stream && !has_pending {
            let snapshot_now = now_ms();
            if !self.renew_lease_if_due(snapshot_now)? {
                return Err(SyncError::Security("sync scheduler lease was lost before snapshot".into()));
            }
            catchup_complete = self.enqueue_local_snapshot(&config, &identity, snapshot_now)?;
        }
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
        if recover || (!had_stream && !has_pending) {
            let config = self.store.secondary_config()?.ok_or_else(|| SyncError::NotPaired(identity.device_id.clone()))?;
            let snapshot_now = now_ms();
            if !self.renew_lease_if_due(snapshot_now)? {
                return Err(SyncError::Security("sync scheduler lease was lost before snapshot".into()));
            }
            catchup_complete = self.enqueue_local_snapshot(&config, &identity, snapshot_now)?;
        }
        Ok(catchup_complete && !has_pending)
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

    fn enqueue_local_snapshot(&mut self, config: &crate::SecondarySyncConfig, identity: &DeviceIdentity, now_ms: i64) -> SyncResult<bool> {
        let historical_ready = if self.occurrences_initialized {
            true
        } else {
            let mut complete = false;
            for _ in 0..HISTORY_INIT_BATCHES_PER_PASS {
                complete = self.store.index_mut().ensure_sync_occurrences_page(HISTORY_INIT_BATCH_ROWS).map_err(SyncError::from)?;
                if complete {
                    self.occurrences_initialized = true;
                    break;
                }
                self.occurrence_init_progressed = true;
                // Hand the writer to the engine between batches — the
                // materialization is worth minutes, the panel's publish cycle
                // must not starve for any of them (see DOWNLOAD_PAGE_ENGINE_YIELD).
                std::thread::sleep(HISTORY_INIT_ENGINE_YIELD);
            }
            complete
        };
        let mut cursor = self.store.snapshot_cursor(config)?;
        if !historical_ready { cursor.last_identity = None; }
        for _ in 0..SNAPSHOT_ENQUEUE_PAGES_PER_PASS {
            let page = if historical_ready {
                self.store.index().sync_snapshot_page(
                    cursor.last_identity.as_deref(), cursor.last_dirty_revision, now_ms,
                    self.config.snapshot_days, usage_index::SYNC_SNAPSHOT_PAGE_ROWS,
                    usage_index::SYNC_SNAPSHOT_PAGE_BYTES,
                )
            } else {
                self.store.index().sync_dirty_page(
                    cursor.last_dirty_revision, now_ms, self.config.snapshot_days,
                    usage_index::SYNC_SNAPSHOT_PAGE_ROWS, usage_index::SYNC_SNAPSHOT_PAGE_BYTES,
                )
            }.map_err(SyncError::from)?;
            let mut rows_by_identity = BTreeMap::<String, NetworkRow>::new();
            for event in page.events {
                let row = network_row(event.event, event.occurrence_id)?;
                rows_by_identity.insert(event.identity, row);
            }
            let mut dirty_by_identity = BTreeMap::<String, usage_index::SyncDirtyEvent>::new();
            for dirty in page.dirty_events {
                dirty_by_identity.insert(dirty.identity.clone(), dirty);
            }
            for (dirty_identity, dirty) in &dirty_by_identity {
                let row = network_row(dirty.event.clone(), dirty.occurrence_id.clone())?;
                rows_by_identity.insert(dirty_identity.clone(), row);
            }
            // Journal every dirty row in this page straight from the map that
            // produced its package row.  Re-deriving the identity from the
            // serialized row could miss a repaired/duplicated identity, and a
            // dirty row with no journal entry is never cleared by ACK — it
            // would pin its event against prune forever.
            let journal = dirty_by_identity.iter().map(|(identity_key, dirty)| {
                let row = rows_by_identity.get(identity_key)
                    .ok_or_else(|| SyncError::Protocol("dirty page row disappeared before journalling".into()))?;
                Ok(DirtyJournalInput {
                    identity: dirty.identity.clone(),
                    revision: dirty.revision,
                    kind: "event_snapshot".into(),
                    row: row.clone(),
                })
            }).collect::<SyncResult<Vec<_>>>()?;
            let mut rows = rows_by_identity.into_values().collect::<Vec<_>>();
            rows.sort_by(|left, right| left.identity_material().unwrap_or("").cmp(right.identity_material().unwrap_or("")));
            let complete = page.events_done && page.dirty_done;
            if rows.is_empty() && !complete {
                if !historical_ready { return Ok(false); }
                return Err(SyncError::Limit("snapshot page made no cursor progress".into()));
            }
            if rows.is_empty() && complete && cursor.complete && cursor.completion_generation.is_some() {
                // An already-ACKed cursor with no dirty revision is an idle
                // pass, not a new empty generation.  In particular, do not
                // let generated_at_ms manufacture receipts every poll.
                return Ok(true);
            }
            let window_lo_ms = page.coverage_lo_ms;
            let window_hi_ms = page.coverage_hi_ms;
            let package = NetworkPackage::from_rows_with_snapshot_complete(
                config.origin.clone(), usage_index::INDEX_SCHEMA_VERSION, window_lo_ms, window_hi_ms,
                &rows, now_ms, complete,
            )?;
            match self.store.enqueue_snapshot_page_auto(
                config, package, &journal, &identity.signing_key, page.next_identity.as_deref(),
                page.next_dirty_revision, complete, now_ms,
            ) {
                Ok(_) => {}
                Err(SyncError::Limit(_)) if self.store.outbox_summary()?.pending > 0 => return Ok(false),
                Err(error) => return Err(error),
            }
            cursor.last_identity = page.next_identity;
            cursor.last_dirty_revision = page.next_dirty_revision;
            cursor.complete = complete;
            if complete { return Ok(true); }
        }
        Ok(false)
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
        hash_bytes, DeviceIdentity, HttpsServerConfig, HttpsTransportConfig, NetworkCounts, NetworkPackage,
        PrimarySyncConfig,
        NetworkRow, PairingRequest, SecondarySyncConfig, SyncStore, Tls13HttpsTransport, MAX_COMPRESSED_BYTES,
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

    fn package(origin: &str) -> NetworkPackage {
        NetworkPackage::from_rows(
            origin,
            usage_index::INDEX_SCHEMA_VERSION,
            0,
            100,
            &[NetworkRow {
                key: Some("runtime-row".into()), occurrence_id: None, tool: "codex".into(),
                ts_ms: 10, session: "runtime-session".into(), project: None, model: None,
                meter: "tokens".into(), source: "runtime.jsonl".into(),
                counts: NetworkCounts::all_zero(), calls: vec![],
            }],
            4,
        ).unwrap()
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
    fn download_page_yield_stays_a_small_engine_yield() {
        // Wide band on purpose: the sleep must leave a waiting publish cycle
        // room to win the writer, without turning a page backfill into a
        // rate-limited crawl. SQLite's busy handler re-tries well inside
        // 50 ms, and denser pages hand the writer over 8 times per pass.
        assert!(DOWNLOAD_PAGE_ENGINE_YIELD >= Duration::from_millis(25), "{DOWNLOAD_PAGE_ENGINE_YIELD:?} is too short to let a waiting writer win the lock");
        assert!(DOWNLOAD_PAGE_ENGINE_YIELD <= Duration::from_millis(1_000), "{DOWNLOAD_PAGE_ENGINE_YIELD:?} is a rate limit, not an engine-yield");
    }

    #[test]
    fn secondary_runtime_uses_its_own_store_to_snapshot_and_ack() {
        let device = DeviceIdentity::create("runtime-secondary", "Secondary").unwrap();
        let (primary, _stream) = paired_primary(&device);
        let mut secondary = SyncStore::open_in_memory("primary-1").unwrap();
        let (cert_der, key_der) = certificate();
        let staging = tempfile::tempdir().unwrap();
        let server = crate::HttpsSyncServer::bind(server_config(&cert_der, &key_der, staging.path().join("staging")), primary).unwrap();
        let addr = server.local_addr().unwrap();
        let server_thread = thread::spawn(move || server.run_connections_concurrent(5));
        let identity_dir = tempfile::tempdir().unwrap();
        let identity_path = identity_dir.path().join("secondary-identity.json");
        secondary.save_secondary_config(&SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: format!("https://127.0.0.1:{}", addr.port()), server_name: "localhost".into(),
            certificate_pin: crate::hash_bytes(&cert_der), certificate_der: cert_der.clone(), identity_path,
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }, usage_core::report::now_ms()).unwrap();
        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert_der,
        )).unwrap();
        let mut runtime = SyncRuntime::new_secondary_with_display_name(
            secondary, transport, SyncRuntimeConfig::new("secondary-test-owner").unwrap(), device.clone(), "Current host",
        );
        let first_now = usage_core::report::now_ms();
        let report = runtime.run_once_with_clock(|| first_now).unwrap();
        assert_eq!(report.acknowledged, 1, "an empty initial snapshot still needs a signed completion marker");
        assert_eq!(runtime.store().outbox_summary().unwrap().pending, 0);
        let second = runtime.run_once_with_clock(|| first_now + 5_000).unwrap();
        assert_eq!(second.acknowledged, 0, "idle polling at a different time must remain outbox-free");
        assert_eq!(runtime.store().outbox_summary().unwrap().pending, 0);
        let config = runtime.store().secondary_config().unwrap().unwrap();
        let primary_name = usage_index::hostname();
        assert_eq!(config.primary_display_name.as_deref(), Some(primary_name.as_str()));
        assert!(config.stream_id.is_some());
        assert_eq!(config.data_epoch, Some(1));
        assert!(config.last_connected_at_ms.is_some());
        assert_eq!(config.last_ack_generation, Some(1));
        assert!(config.last_ack_receipt_hash.is_some());
        assert_eq!(runtime.store().snapshot_cursor(&config).unwrap().completion_generation, Some(1));
        let status_request = AuthenticatedStatusRequest::new_at(&device, "primary-1", false, first_now + 10_000).unwrap();
        assert_eq!(runtime.transport.status(&status_request).unwrap().display_name.as_deref(), Some("Current host"));
        assert!(server_thread.join().unwrap().is_ok());
    }

    #[test]
    fn secondary_runtime_recovers_a_purged_receipt_with_a_fresh_stream() {
        let device = DeviceIdentity::create("runtime-recovery", "Secondary").unwrap();
        // The primary is file-backed so the test can purge a receipt between
        // passes while the server holds its own connection to the same store.
        let primary_dir = tempfile::tempdir().unwrap();
        let primary_path = primary_dir.path().join("index.db");
        let mut primary_admin = SyncStore::open(&primary_path, "primary-1").unwrap();
        let invite = primary_admin.create_pairing_invite("https://localhost:47821", &"1".repeat(64), i64::MAX).unwrap();
        let request = PairingRequest::new(&invite, &device, "linux").unwrap();
        let state = primary_admin.receive_pairing_request(&request, 1).unwrap();
        primary_admin.approve_pairing(&state.request_id, 2).unwrap();
        let stream = primary_admin.open_stream_for_registered(&device.device_id, 3).unwrap();

        let (cert_der, key_der) = certificate();
        let staging = tempfile::tempdir().unwrap();
        let server = crate::HttpsSyncServer::bind(
            server_config(&cert_der, &key_der, staging.path().join("staging")),
            SyncStore::open(&primary_path, "primary-1").unwrap(),
        ).unwrap();
        let addr = server.local_addr().unwrap();
        let server_thread = thread::spawn(move || server.run_connections_concurrent(32));

        let mut secondary = SyncStore::open_in_memory("primary-1").unwrap();
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
        secondary.index_mut().conn().execute(
            "INSERT INTO event(id, tool, ts_ms, session, meter, dedupe_key, source) VALUES(1, 'codex', ?1, 'recovery-session', 'tokens', 'recovery-row', 'live.jsonl')",
            rusqlite::params![first_now],
        ).unwrap();
        secondary.index_mut().conn().execute(
            "INSERT INTO sync_event_identity(event_id, identity, occurrence_id) VALUES(1, 'key:recovery-row', NULL)",
            [],
        ).unwrap();
        secondary.index_mut().conn().execute(
            "INSERT INTO sync_dirty(identity, source_key, event_json, event_id, event_ts_ms, revision, dirty_since_ms, updated_ms)
             VALUES('key:recovery-row', 'live.jsonl', X'', 1, ?1, 1, ?1, ?1)",
            rusqlite::params![first_now],
        ).unwrap();

        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert_der,
        )).unwrap();
        let mut runtime = SyncRuntime::new_secondary(
            secondary, transport, SyncRuntimeConfig::new("secondary-recovery-owner").unwrap(), device.clone(),
        );
        let first = runtime.run_once_with_clock(|| first_now).unwrap();
        assert_eq!(first.acknowledged, 1);
        let before = runtime.store().secondary_config().unwrap().unwrap();
        assert_eq!(before.stream_id.as_deref(), Some(stream.stream_id.as_str()));
        assert_eq!(before.last_ack_generation, Some(1));

        // Losing the ACKed receipt is the primary-side reconcile trigger; the
        // next status call itself answers 409 and the client must repair it
        // without a manual step.
        primary_admin.purge_receipt(&device.device_id, &stream.stream_id, 1).unwrap();
        let second = runtime.run_once_with_clock(|| first_now + 5_000).unwrap();
        assert_eq!(second.acknowledged, 1, "recovery must re-send a fresh generation-1 snapshot");
        let after = runtime.store().secondary_config().unwrap().unwrap();
        assert_ne!(after.stream_id.as_deref(), Some(stream.stream_id.as_str()));
        assert_eq!(after.data_epoch, Some(1));
        assert_eq!(after.last_ack_generation, Some(1));
        assert_eq!(after.last_error, None);
        assert_eq!(runtime.store().outbox_summary().unwrap().pending, 0);
        assert_eq!(runtime.store().snapshot_cursor(&after).unwrap().completion_generation, Some(1));

        let status_request = AuthenticatedStatusRequest::new_at(&device, "primary-1", false, first_now + 6_000).unwrap();
        let status = runtime.transport.status(&status_request).unwrap();
        assert_eq!(status.status, "paired");
        assert_eq!(status.active_stream_id.as_deref(), after.stream_id.as_deref());
        assert_eq!(status.expected_generation, Some(2), "the recovered stream must have accepted generation 1");
        let old_active: i64 = primary_admin.index().conn().query_row(
            "SELECT active FROM device_stream WHERE device_id = ?1 AND stream_id = ?2",
            rusqlite::params![device.device_id, stream.stream_id], |row| row.get(0),
        ).unwrap();
        assert_eq!(old_active, 0, "the recovered-from stream stays frozen");
        let receipts: i64 = primary_admin.index().conn().query_row(
            "SELECT count(*) FROM device_receipt WHERE device_id = ?1 AND stream_id = ?2",
            rusqlite::params![device.device_id, after.stream_id.clone().unwrap()], |row| row.get(0),
        ).unwrap();
        assert_eq!(receipts, 1);
        drain_server(addr, server_thread);
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
        assert_eq!(report.acknowledged, 1, "the initial empty upload still establishes the paired stream");
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
    fn secondary_runtime_uploads_dirty_rows_before_historical_identity_catchup() {
        let device = DeviceIdentity::create("runtime-incremental", "Secondary").unwrap();
        let primary = paired_primary_without_stream(&device);
        let mut secondary = SyncStore::open_in_memory("primary-1").unwrap();
        let (cert_der, key_der) = certificate();
        let staging = tempfile::tempdir().unwrap();
        let server = crate::HttpsSyncServer::bind(server_config(&cert_der, &key_der, staging.path().join("staging")), primary).unwrap();
        let addr = server.local_addr().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let identity_path = identity_dir.path().join("secondary-identity.json");
        let first_now = usage_core::report::now_ms();
        secondary.save_secondary_config(&SecondarySyncConfig {
            primary_os: None,
            primary_id: "primary-1".into(), primary_display_name: None, device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: format!("https://127.0.0.1:{}", addr.port()), server_name: "localhost".into(),
            certificate_pin: crate::hash_bytes(&cert_der), certificate_der: cert_der.clone(), identity_path,
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }, first_now).unwrap();
        // One more history row than the upload side materializes per pass
        // (four identity batches).
        let missing = usage_index::SYNC_SNAPSHOT_PAGE_ROWS * 4 + 1;
        for id in 1..=missing as i64 {
            secondary.index_mut().conn().execute(
                "INSERT INTO event(id, tool, ts_ms, session, meter, source) VALUES(?1, 'codex', ?2, ?3, 'tokens', 'history.jsonl')",
                rusqlite::params![id, first_now, format!("history-{id}")],
            ).unwrap();
        }
        let dirty_id = missing as i64 + 1;
        secondary.index_mut().conn().execute(
            "INSERT INTO event(id, tool, ts_ms, session, meter, dedupe_key, source) VALUES(?1, 'codex', ?2, 'live', 'tokens', 'live-row', 'live.jsonl')",
            rusqlite::params![dirty_id, first_now],
        ).unwrap();
        secondary.index_mut().conn().execute(
            "INSERT INTO sync_event_identity(event_id, identity, occurrence_id) VALUES(?1, 'key:live-row', NULL)",
            rusqlite::params![dirty_id],
        ).unwrap();
        secondary.index_mut().conn().execute(
            "INSERT INTO sync_dirty(identity, source_key, event_json, event_id, event_ts_ms, revision, dirty_since_ms, updated_ms)
             VALUES('key:live-row', 'live.jsonl', X'', ?1, ?2, 1, ?2, ?2)",
            rusqlite::params![dirty_id, first_now],
        ).unwrap();

        let server_thread = thread::spawn(move || server.run_connections_concurrent(3));
        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert_der,
        )).unwrap();
        let mut runtime = SyncRuntime::new_secondary(secondary, transport, SyncRuntimeConfig::new("secondary-incremental-owner").unwrap(), device);
        let report = runtime.run_once_with_clock(|| first_now).unwrap();
        assert_eq!(report.acknowledged, 1, "a new dirty row must upload before history mapping finishes");

        // 16 batches per pass catch the small fixture up in one pass: the
        // cursor must sit at the last local event. The live dirty row's
        // import created the final identity before this read.
        // One pass with 16 batches maps the whole fixture: no local-origin
        // event may remain without an identity row (the live dirty row came
        // pre-materialized, so it never enters the walk).
        let missing: bool = runtime.store().index().conn()
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM event e LEFT JOIN sync_event_identity i ON i.event_id = e.id WHERE ({}) = '' AND i.event_id IS NULL)", usage_index::ORIGIN_SQL),
                [], |row| row.get(0),
            ).unwrap();
        assert!(!missing, "one pass maps the whole fixture");
        let config = runtime.store().secondary_config().unwrap().unwrap();
        assert!(!runtime.store().snapshot_cursor(&config).unwrap().complete);
        assert!(server_thread.join().unwrap().is_ok());
    }

    #[test]
    fn runtime_claims_uploads_acks_and_clears_journaled_outbox() {
        let device = DeviceIdentity::create("runtime-device", "Runtime").unwrap();
        let (primary, stream) = paired_primary(&device);
        let package = package(&device.origin);
        let mut sender = SyncStore::open_in_memory("primary-1").unwrap();
        sender.enqueue_outbox_auto(
            &device.device_id, &device.origin, &stream.stream_id, stream.data_epoch,
            package.manifest_bytes, package.gzip_bytes, &device.signing_key, &[], 10,
        ).unwrap();
        let (cert_der, key_der) = certificate();
        let staging = tempfile::tempdir().unwrap();
        let server = crate::HttpsSyncServer::bind(server_config(&cert_der, &key_der, staging.path().join("staging")), primary).unwrap();
        let addr = server.local_addr().unwrap();
        let server_thread = thread::spawn(move || server.run_connections_concurrent(1));
        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert_der,
        )).unwrap();
        let config = SyncRuntimeConfig::new("runtime-owner").unwrap();
        let mut runtime = SyncRuntime::new(sender, transport, config);
        let report = runtime.run_once_with_clock(|| 10).unwrap();
        assert_eq!(report.acknowledged, 1);
        assert_eq!(runtime.store().outbox_summary().unwrap().pending, 0);
        assert_eq!(runtime.store().outbox_summary().unwrap().in_flight, 0);
        assert!(server_thread.join().unwrap().is_ok());
    }

    #[test]
    fn runtime_keeps_outbox_pending_when_transport_fails() {
        let device = DeviceIdentity::create("runtime-failure", "Runtime").unwrap();
        let (_primary, stream) = paired_primary(&device);
        let package = package(&device.origin);
        let mut sender = SyncStore::open_in_memory("primary-1").unwrap();
        sender.enqueue_outbox_auto(
            &device.device_id, &device.origin, &stream.stream_id, stream.data_epoch,
            package.manifest_bytes, package.gzip_bytes, &device.signing_key, &[], 10,
        ).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (cert_der, _key_der) = certificate();
        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{port}"), "localhost", cert_der,
        )).unwrap();
        let mut runtime = SyncRuntime::new(sender, transport, SyncRuntimeConfig::new("runtime-owner").unwrap());
        let report = runtime.run_once_with_clock(|| 10).unwrap();
        assert_eq!(report.retried, 1);
        assert_eq!(runtime.store().outbox_summary().unwrap().pending, 1);
        assert_eq!(runtime.store().outbox_summary().unwrap().in_flight, 0);
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

//! The single-owner outbox runtime used by CLI, Panel and a user service.
//!
//! It deliberately owns no timer thread by itself: callers choose `run_once`
//! or `run_until`, while the SQLite lease is the process-wide singleton gate.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use usage_core::{CallKind, Meter, UsageEvent};
use usage_index::Index;

use crate::{
    AuthenticatedStatus, AuthenticatedStatusRequest, DeviceIdentity, DirtyJournalInput,
    NetworkCall, NetworkCounts, NetworkPackage, NetworkRow, StreamOpenReceipt,
    ClientTransportCancellation, SyncError, SyncResult, SyncStore, Tls13HttpsTransport,
    TransportError,
};

// Keep the scheduler lease longer than the transport's 10-minute request
// deadline. A live upload must not become reclaimable by a second process
// while its owner is still waiting for the ACK.
const DEFAULT_LEASE_TTL_MS: i64 = 15 * 60 * 1000;
const DEFAULT_POLL_MS: u64 = 5_000;
const MAX_SNAPSHOT_PAGES_PER_PASS: usize = 4;

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
    occurrences_initialized: bool,
}

#[derive(Debug, Clone)]
pub struct LocalSnapshot {
    pub package: NetworkPackage,
    pub rows: Vec<NetworkRow>,
}

impl SyncRuntime {
    pub fn new(store: SyncStore, transport: Tls13HttpsTransport, config: SyncRuntimeConfig) -> Self {
        Self { store, transport, config, secondary_identity: None, occurrences_initialized: false }
    }

    pub fn new_secondary(
        store: SyncStore,
        transport: Tls13HttpsTransport,
        config: SyncRuntimeConfig,
        identity: DeviceIdentity,
    ) -> Self {
        Self { store, transport, config, secondary_identity: Some(identity), occurrences_initialized: false }
    }

    pub fn store(&self) -> &SyncStore { &self.store }
    pub fn store_mut(&mut self) -> &mut SyncStore { &mut self.store }
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
        let result = (|| {
            self.store.recover_inflight_outbox(&self.config.owner, first_now)?;
            let _ = self.prepare_secondary(&mut now_ms)?;
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
        let first_now = now_ms();
        if !self.store.acquire_scheduler_lease(&self.config.owner, first_now, self.config.lease_ttl_ms)? {
            return Err(SyncError::Security("another sync runtime owns the scheduler lease".into()));
        }
        if let Err(error) = self.store.recover_inflight_outbox(&self.config.owner, first_now) {
            let _ = self.store.release_scheduler_lease(&self.config.owner);
            return Err(error);
        }
        let mut total = RuntimeRunReport::default();
        let mut consecutive_errors = 0_u32;
        while !stop.load(Ordering::Acquire) {
            let lease_now = now_ms();
            if !self.store.renew_scheduler_lease(&self.config.owner, lease_now, self.config.lease_ttl_ms)? {
                let _ = self.store.release_scheduler_lease(&self.config.owner);
                return Err(SyncError::Security("sync scheduler lease was lost before preparing the next pass".into()));
            }
            if let Err(error) = ingest(&mut self.store) {
                let _ = self.store.release_scheduler_lease(&self.config.owner);
                return Err(error);
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
            total.last_error = pass.last_error;
            let renew_now = now_ms();
            if !self.store.renew_scheduler_lease(&self.config.owner, renew_now, self.config.lease_ttl_ms)? {
                let _ = self.store.release_scheduler_lease(&self.config.owner);
                return Err(SyncError::Security("sync scheduler lease was lost".into()));
            }
            if stop.load(Ordering::Acquire) { break; }
            // A long offline catch-up must continue immediately after a page
            // was ACKed.  Once the page is complete, or when a retry is in
            // backoff with no claimable row, use the normal poll cadence.
            if !catchup_complete && !pass_had_error && !pass_had_no_claim_with_pending {
                continue;
            }
            let started = std::time::Instant::now();
            let interval = self.config.poll_interval.saturating_mul(1_u32 << consecutive_errors.min(4)).min(Duration::from_secs(60));
            while started.elapsed() < interval && !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(50).min(interval));
            }
        }
        let _ = self.store.release_scheduler_lease(&self.config.owner);
        Ok(total)
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
            match self.transport.upload(&entry.signed_upload()) {
                Ok(receipt) => {
                    report.uploaded += 1;
                    let commit_now = now_ms();
                    if !self.store.renew_scheduler_lease(&self.config.owner, commit_now, self.config.lease_ttl_ms)? {
                        return Err(SyncError::Security("sync scheduler lease was lost before ACK".into()));
                    }
                    self.store.ack_outbox(&self.config.owner, entry.id, &receipt, commit_now)?;
                    report.acknowledged += 1;
                }
                Err(error) => {
                    let message = error.to_string();
                    self.store.retry_outbox(&self.config.owner, entry.id, now_ms(), &message)?;
                    report.retried += 1;
                    report.last_error = Some(message);
                    break;
                }
            }
        }
        let summary = self.store.outbox_summary()?;
        report.pending = summary.pending;
        report.in_flight = summary.in_flight;
        report.bytes = summary.bytes;
        Ok(report)
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
            if !self.store.renew_scheduler_lease(&self.config.owner, snapshot_now, self.config.lease_ttl_ms)? {
                return Err(SyncError::Security("sync scheduler lease was lost before snapshot".into()));
            }
            catchup_complete = self.enqueue_local_snapshot(&config, &identity, snapshot_now)?;
        }
        let request_now = now_ms();
        let request = AuthenticatedStatusRequest::new_at_with_checkpoint(
            &identity, config.primary_id.clone(), true, config.last_ack_generation, request_now,
        )?;
        let status = self.transport.status(&request).map_err(map_secondary_status_error)?;
        self.store.mark_secondary_connected(now_ms())?;
        match status.status.as_str() {
            "paired" => {}
            "pending" => return Err(SyncError::NotPaired("pairing request is awaiting approval".into())),
            "rejected" => return Err(SyncError::NotPaired("pairing request was rejected".into())),
            "expired" => return Err(SyncError::NotPaired("pairing invite expired".into())),
            "paused" => return Err(SyncError::Conflict("primary paused this device".into())),
            "revoked" => return Err(SyncError::Revoked(identity.device_id.clone())),
            "reconcile_required" => return Err(SyncError::ReconcileRequired),
            other => return Err(SyncError::Protocol(format!("primary returned unknown device status {other:?}"))),
        }
        validate_secondary_status(&config, &status, local_has_history)?;
        let receipt = match (status.active_stream_id, status.expected_generation) {
            (Some(stream_id), Some(expected_generation)) => StreamOpenReceipt {
                primary_id: status.primary_id,
                device_id: status.device_id,
                stream_id,
                data_epoch: status.data_epoch,
                expected_generation,
            },
            (None, None) => self.transport.recover(&request).map_err(|error| SyncError::Protocol(format!("secondary stream recovery failed: {error}")))?,
            _ => return Err(SyncError::Security("primary returned an incomplete stream status".into())),
        };
        let stream_now = now_ms();
        if !self.store.renew_scheduler_lease(&self.config.owner, stream_now, self.config.lease_ttl_ms)? {
            return Err(SyncError::Security("sync scheduler lease was lost before stream update".into()));
        }
        self.store.update_secondary_stream(&receipt, stream_now)?;
        if !had_stream && !has_pending {
            let config = self.store.secondary_config()?.ok_or_else(|| SyncError::NotPaired(identity.device_id.clone()))?;
            let snapshot_now = now_ms();
            if !self.store.renew_scheduler_lease(&self.config.owner, snapshot_now, self.config.lease_ttl_ms)? {
                return Err(SyncError::Security("sync scheduler lease was lost before snapshot".into()));
            }
            catchup_complete = self.enqueue_local_snapshot(&config, &identity, snapshot_now)?;
        }
        self.store.set_secondary_last_error(None, now_ms())?;
        Ok(catchup_complete && !has_pending)
    }

    fn remember_secondary_error(&mut self, error: &SyncError, now_ms: i64) {
        if self.secondary_identity.is_none() { return; }
        let message = error.to_string();
        let _ = self.store.set_secondary_last_error(Some(&message), now_ms);
        if matches!(error, SyncError::Revoked(_) | SyncError::ReconcileRequired | SyncError::EpochStale | SyncError::Security(_))
            || matches!(error, SyncError::NotPaired(message) if message == "pairing request was rejected" || message == "pairing invite expired")
        {
            let _ = self.store.set_secondary_enabled(false, now_ms);
        }
    }

    fn enqueue_local_snapshot(&mut self, config: &crate::SecondarySyncConfig, identity: &DeviceIdentity, now_ms: i64) -> SyncResult<bool> {
        if !self.occurrences_initialized {
            let complete = self.store.index_mut().ensure_sync_occurrences_page(256).map_err(SyncError::from)?;
            if !complete { return Ok(false); }
            self.occurrences_initialized = true;
        }
        let mut cursor = self.store.snapshot_cursor(config)?;
        for _ in 0..MAX_SNAPSHOT_PAGES_PER_PASS {
            let summary = self.store.outbox_summary()?;
            if summary.pending > 0 || summary.in_flight > 0 {
                return Ok(false);
            }
            let page = self.store.index().sync_snapshot_page(
                cursor.last_identity.as_deref(), cursor.last_dirty_revision, now_ms,
                self.config.snapshot_days, usage_index::SYNC_SNAPSHOT_PAGE_ROWS,
                usage_index::SYNC_SNAPSHOT_PAGE_BYTES,
            ).map_err(SyncError::from)?;
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
            let mut rows = rows_by_identity.into_values().collect::<Vec<_>>();
            rows.sort_by(|left, right| left.identity_material().unwrap_or("").cmp(right.identity_material().unwrap_or("")));
            let complete = page.events_done && page.dirty_done;
            if rows.is_empty() && !complete {
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
            let journal = dirty_by_identity.into_iter().filter_map(|(identity_key, dirty)| {
                rows.iter().find(|row| sync_dirty_identity(row) == identity_key).map(|row| DirtyJournalInput {
                    identity: dirty.identity,
                    revision: dirty.revision,
                    kind: "event_snapshot".into(),
                    row: row.clone(),
                })
            }).collect::<Vec<_>>();
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

fn sync_dirty_identity(row: &NetworkRow) -> String {
    match (&row.key, &row.occurrence_id) {
        (Some(key), _) => format!("key:{key}"),
        (None, Some(occurrence)) => format!("occ:{occurrence}"),
        _ => String::new(),
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
        SyncError::Protocol(_) | SyncError::Auth(_) | SyncError::NotPaired(_) | SyncError::Conflict(_) => true,
        _ => false,
    }
}

fn map_secondary_status_error(error: TransportError) -> SyncError {
    match error {
        TransportError::HttpStatus { status: 409, message } if message.contains("reconciliation is required") => SyncError::ReconcileRequired,
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

fn network_row(event: UsageEvent, occurrence_id: Option<String>) -> SyncResult<NetworkRow> {
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

    fn paired_primary(device: &DeviceIdentity) -> (SyncStore, crate::StreamOpenReceipt) {
        let mut primary = SyncStore::open_in_memory("primary-1").unwrap();
        let invite = primary.create_pairing_invite(
            "https://localhost:47821",
            &"1".repeat(64),
            i64::MAX,
        ).unwrap();
        let request = PairingRequest::new(&invite, device, "linux").unwrap();
        let state = primary.receive_pairing_request(&request, 1).unwrap();
        primary.approve_pairing(&state.request_id, 2).unwrap();
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
            max_concurrent: 1, staging_dir, idle_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(600), staging_quota_bytes: 512 * 1024 * 1024,
        }
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
        let server_thread = thread::spawn(move || server.run_connections(3));
        let identity_dir = tempfile::tempdir().unwrap();
        let identity_path = identity_dir.path().join("secondary-identity.json");
        secondary.save_secondary_config(&SecondarySyncConfig {
            primary_id: "primary-1".into(), device_id: device.device_id.clone(), origin: device.origin.clone(),
            endpoint: format!("https://127.0.0.1:{}", addr.port()), server_name: "localhost".into(),
            certificate_pin: crate::hash_bytes(&cert_der), certificate_der: cert_der.clone(), identity_path,
            stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        }, usage_core::report::now_ms()).unwrap();
        let transport = Tls13HttpsTransport::new(HttpsTransportConfig::pinned(
            format!("https://127.0.0.1:{}", addr.port()), "localhost", cert_der,
        )).unwrap();
        let mut runtime = SyncRuntime::new_secondary(secondary, transport, SyncRuntimeConfig::new("secondary-test-owner").unwrap(), device);
        let first_now = usage_core::report::now_ms();
        let report = runtime.run_once_with_clock(|| first_now).unwrap();
        assert_eq!(report.acknowledged, 1, "an empty initial snapshot still needs a signed completion marker");
        assert_eq!(runtime.store().outbox_summary().unwrap().pending, 0);
        let second = runtime.run_once_with_clock(|| first_now + 5_000).unwrap();
        assert_eq!(second.acknowledged, 0, "idle polling at a different time must remain outbox-free");
        assert_eq!(runtime.store().outbox_summary().unwrap().pending, 0);
        let config = runtime.store().secondary_config().unwrap().unwrap();
        assert!(config.stream_id.is_some());
        assert_eq!(config.data_epoch, Some(1));
        assert!(config.last_connected_at_ms.is_some());
        assert_eq!(config.last_ack_generation, Some(1));
        assert!(config.last_ack_receipt_hash.is_some());
        assert_eq!(runtime.store().snapshot_cursor(&config).unwrap().completion_generation, Some(1));
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
        let server_thread = thread::spawn(move || server.run_connections(1));
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
            primary_id: "primary-1".into(), device_id: "device-1".into(), origin: "origin-1".into(),
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
            primary_id: "primary-1".into(), device_id: "device-1".into(), origin: "origin-1".into(),
            endpoint: "https://localhost:1".into(), server_name: "localhost".into(), certificate_der: vec![1], certificate_pin: hash_bytes(&[1]),
            identity_path: std::path::PathBuf::from("identity.json"), stream_id: None, data_epoch: None, expected_generation: None, enabled: true, capture_enabled: true,
            last_error: None, last_manifest_hash: None, last_gzip_hash: None, last_connected_at_ms: None,
            last_ack_generation: None, last_ack_receipt_hash: None,
        };
        let status = AuthenticatedStatus {
            primary_id: "primary-1".into(), device_id: "device-1".into(), origin: "origin-1".into(), request_nonce: [0; 16], status: "paired".into(),
            data_epoch: 1, active_stream_id: Some("primary-stream".into()), expected_generation: Some(1), last_generation: None,
            highwater_generation: None, highwater_receipt_hash: None, checkpoint_generation: None, checkpoint_receipt_hash: None,
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

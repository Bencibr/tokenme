import { useCallback, useEffect, useRef, useState } from "react";
import type { DiscoveredPrimaryView, SyncDevicesSnapshot, SyncManagementRequest } from "../types";
import { bridge } from "../lib/bridge";
import { t } from "../lib/i18n";
import { IconBack, IconClose, IconDevicePlatform, IconMonitorDown, IconRadioTower, IconWarn } from "./Icons";

async function copyText(value: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(value);
    return true;
  } catch {
    const node = document.createElement("textarea");
    node.value = value;
    node.style.position = "fixed";
    node.style.opacity = "0";
    document.body.appendChild(node);
    node.select();
    const ok = document.execCommand("copy");
    node.remove();
    return ok;
  }
}

type Screen = "setup" | "discovery" | "confirm" | "waiting" | "overview";

type PendingConfirmation = {
  message: string;
  request: SyncManagementRequest;
};

function shortPin(pin: string): string {
  const clean = pin.trim().toLowerCase().replace(/^sha256:/, "");
  if (clean.length <= 8) return clean;
  return `${clean.slice(0, 4)}…${clean.slice(-4)}`;
}

function relTime(ms: number): string {
  const minutes = Math.max(0, Math.floor((Date.now() - ms) / 60000));
  if (minutes < 1) return t("sync.rel.now");
  if (minutes < 60) return t("sync.rel.min", { n: minutes });
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return t("sync.rel.hour", { n: hours });
  return t("sync.rel.day", { n: Math.floor(hours / 24) });
}

function hasRecentSecondaryContact(snapshot: SyncDevicesSnapshot): boolean {
  if (snapshot.role !== "secondary" || snapshot.last_connected_at_ms == null) return false;
  const age = Date.now() - snapshot.last_connected_at_ms;
  return age >= -60_000 && age <= 60_000;
}

function syncStatus(snapshot: SyncDevicesSnapshot): { kind: "running" | "recovering" | "degraded" | "stopped" | "error"; label: string } {
  if (snapshot.role === "secondary" && snapshot.service_running && snapshot.last_error) {
    return hasRecentSecondaryContact(snapshot)
      ? { kind: "degraded", label: t("sync.state.degraded") }
      : { kind: "recovering", label: t("sync.state.recovering") };
  }
  if (snapshot.last_error || snapshot.operation_state === "error") return { kind: "error", label: t("sync.state.error") };
  // A fast-accepted start reports operation_state "running" while no service
  // exists yet — without this the retry line claims a connection that was
  // never lost, on a machine that is not even a secondary.
  if (snapshot.primary_starting && !snapshot.service_running) {
    return { kind: "recovering", label: t("sync.state.starting") };
  }
  if (snapshot.service_running) return { kind: "running", label: t("sync.state.running") };
  if (snapshot.operation_state === "running") return { kind: "recovering", label: t("sync.state.recovering") };
  return { kind: "stopped", label: t("sync.state.stopped") };
}

function syncErrorMessage(error: string | null): string | null {
  if (!error) return null;
  const normalized = error.toLowerCase();
  if (normalized.includes("connection refused") || normalized.includes("connection reset") || normalized.includes("transport i/o failed") || normalized.includes("cannot notify primary")) return t("sync.error.unreachable");
  if (normalized.includes("certificate") || normalized.includes("pin mismatch")) return t("sync.error.security");
  if (normalized.includes("reconcile")) return t("sync.error.reconcile");
  if (normalized.includes("primary download")) return t("sync.error.download");
  if (normalized.includes("database") || normalized.includes("sqlite") || normalized.includes("corrupt")) return t("sync.error.database");
  if (normalized.includes("pairing invite") && normalized.includes("expired")) return t("sync.error.invite_expired");
  if (normalized.includes("invalid pairing invite")) return t("sync.error.invite_invalid");
  if (normalized.includes("pairing request was rejected")) return t("sync.error.rejected");
  if (normalized.includes("already in use")) return t("sync.error.listener");
  if (normalized.includes("sync protocol") || normalized.includes("update tokenme on the other machine")) return t("sync.error.version");
  if (normalized.includes("busy") || normalized.includes("already running")) return t("sync.error.busy");
  if (normalized.includes("network") || normalized.includes("timeout") || normalized.includes("timed out")) return t("sync.error.network");
  return error;
}

function syncScreenFor(snapshot: SyncDevicesSnapshot): Screen {
  if (snapshot.role === "unconfigured" && !snapshot.primary_starting) return "setup";
  if (!snapshot.service_running && !snapshot.primary_starting) return "setup";
  return "overview";
}

export function SyncSheet({ snapshot, onSnapshot, onClose }: { snapshot: SyncDevicesSnapshot; onSnapshot: (next: SyncDevicesSnapshot) => void; onClose: () => void }) {
  const [mutationBusy, setMutationBusy] = useState(false);
  const [pollingBusy, setPollingBusy] = useState(false);
  const [initializing, setInitializing] = useState(true);
  const [discovering, setDiscovering] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [dismissedError, setDismissedError] = useState<string | null>(null);
  const [screen, setScreen] = useState<Screen>(() => syncScreenFor(snapshot));
  const [pickedRole, setPickedRole] = useState<"primary" | "secondary" | null>(null);
  const [confirmTarget, setConfirmTarget] = useState<DiscoveredPrimaryView | null>(null);
  const [manualInvite, setManualInvite] = useState("");
  const [manualInviteOpen, setManualInviteOpen] = useState(false);
  const [inviteCopied, setInviteCopied] = useState(false);
  const [inviteOpen, setInviteOpen] = useState(false);
  const [pollGeneration, setPollGeneration] = useState(0);
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirmation | null>(null);
  const [secondaryConnectingElapsed, setSecondaryConnectingElapsed] = useState(false);
  // Optimistic override for the primary's share switch: the panel-side write
  // can sit seconds behind the ingest engine's lock on the usage index, so the
  // toggle flips locally at click speed and reconciles when the write lands.
  const [shareOverride, setShareOverride] = useState<boolean | null>(null);
  const shareBusy = useRef(false);
  // Same optimistic-override pattern as the share switch for the primary's
  // auto-approve setting: the write is state-only and fast, but the click
  // must still read back instantly.
  const [autoApproveOverride, setAutoApproveOverride] = useState<boolean | null>(null);
  const autoApproveBusy = useRef(false);
  // Set when the primary rejects a pairing this sheet is waiting on, so the
  // role-teardown lifecycle below lands the user back on the scan page
  // instead of the role selection.
  const rejectedScanReturn = useRef(false);
  const discoveryInFlight = useRef(false);
  const manualDiscoveryRequested = useRef(false);
  const operationGeneration = useRef(0);
  const mutationInFlight = useRef(false);
  const pollingInFlight = useRef(false);
  const mounted = useRef(false);

  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  const run = async (request: SyncManagementRequest, onSuccess?: (next: SyncDevicesSnapshot) => void) => {
    if (!mounted.current) return;
    setPendingConfirm(null);
    const generation = ++operationGeneration.current;
    mutationInFlight.current = true;
    setMutationBusy(true);
    setError(null);
    setDismissedError(null);
    setPollGeneration((value) => value + 1);
    try {
      const next = await bridge.syncDevices(request);
      if (!mounted.current) return;
      setError(null);
      setDismissedError(null);
      onSnapshot(next);
      if (request.action === "stop") setScreen("setup");
      else onSuccess?.(next);
      // Mutations return a snapshot, but take one explicit status read after
      // the write so stale pairing/device rows cannot remain on screen.
      const latest = await bridge.syncDevices({ action: "status" });
      if (mounted.current && generation === operationGeneration.current) {
        setError(null);
        setDismissedError(null);
        onSnapshot(latest);
      }
    } catch (e) {
      if (mounted.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      mutationInFlight.current = false;
      if (mounted.current) setMutationBusy(false);
    }
  };

  // The share switch must never wait on the sync worker: flip locally now,
  // persist in the background, and roll back only if the write actually fails.
  const toggleSharing = (on: boolean) => {
    if (!mounted.current || shareBusy.current) return;
    shareBusy.current = true;
    setShareOverride(on);
    setError(null);
    void bridge.syncDevices({ action: "set_primary_data_sharing", enabled: on })
      .then((next) => {
        if (!mounted.current) return;
        setError(null);
        onSnapshot(next);
      })
      .catch((e) => {
        if (!mounted.current) return;
        setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (mounted.current) setShareOverride(null);
        shareBusy.current = false;
      });
  };

  const toggleAutoApprove = (on: boolean) => {
    if (!mounted.current || autoApproveBusy.current) return;
    autoApproveBusy.current = true;
    setAutoApproveOverride(on);
    setError(null);
    void bridge.syncDevices({ action: "set_primary_auto_approve", enabled: on })
      .then((next) => {
        if (!mounted.current) return;
        setError(null);
        onSnapshot(next);
      })
      .catch((e) => {
        if (!mounted.current) return;
        setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (mounted.current) setAutoApproveOverride(null);
        autoApproveBusy.current = false;
      });
  };

  const discover = useCallback(async () => {
    if (!mounted.current || mutationInFlight.current || discoveryInFlight.current) return;
    manualDiscoveryRequested.current = true;
    discoveryInFlight.current = true;
    const generation = ++operationGeneration.current;
    setDiscovering(true);
    setError(null);
    try {
      const next = await bridge.syncDevices({ action: "discover" });
      if (mounted.current && generation === operationGeneration.current) {
        setError(null);
        onSnapshot(next);
      }
    } catch (e) {
      if (mounted.current && generation === operationGeneration.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      discoveryInFlight.current = false;
      if (mounted.current) setDiscovering(false);
    }
  }, [onSnapshot]);

  useEffect(() => {
    let alive = true;
    // Defer even the local status read by one task so the newly mounted sheet
    // gets a paint before any synchronous Tauri command work starts.
    const statusTimer = window.setTimeout(() => {
      if (!alive || mutationInFlight.current) return;
      const generation = ++operationGeneration.current;
      void bridge.syncDevices().then((next) => {
        if (!alive || generation !== operationGeneration.current) return;
        setError(null);
        onSnapshot(next);
      }).catch((e) => {
        if (alive && generation === operationGeneration.current) setError(e instanceof Error ? e.message : String(e));
      }).finally(() => {
        if (alive && generation === operationGeneration.current) setInitializing(false);
      });
    }, 0);
    return () => { alive = false; window.clearTimeout(statusTimer); };
  }, [onSnapshot]);

  // A successful backend snapshot is authoritative. Clear a previous
  // transport error even when the sheet stayed mounted through recovery.
  useEffect(() => {
    if (!snapshot.last_error) {
      setError(null);
      setDismissedError(null);
    }
  }, [snapshot.last_error]);

  // Entering the discovery screen starts one bounded mDNS browse; the small
  // retry loop only waits out an in-flight initial status read.
  useEffect(() => {
    if (screen !== "discovery") return;
    let alive = true;
    let timer: number | undefined;
    const attempt = () => {
      if (!alive || !mounted.current) return;
      if (mutationInFlight.current || discoveryInFlight.current) {
        timer = window.setTimeout(attempt, 50);
        return;
      }
      timer = undefined;
      void discover();
    };
    timer = window.setTimeout(attempt, 0);
    return () => { alive = false; if (timer !== undefined) window.clearTimeout(timer); };
  }, [discover, screen]);

  // The sheet follows the sync lifecycle instead of keeping a stale local
  // overview alive after the service has left the running state.
  const previousRole = useRef(snapshot.role);
  const previousServiceRunning = useRef(snapshot.service_running);
  useEffect(() => {
    const wasRole = previousRole.current;
    const wasRunning = previousServiceRunning.current;
    previousRole.current = snapshot.role;
    previousServiceRunning.current = snapshot.service_running;
    const roleSelection = (snapshot.role === "unconfigured" || !snapshot.service_running) && !snapshot.primary_starting;
    const enteredRoleSelection = roleSelection && (wasRole !== snapshot.role || wasRunning !== snapshot.service_running);
    const enteredRunningState = !roleSelection && (wasRole !== snapshot.role || wasRunning !== snapshot.service_running);
    if (enteredRoleSelection) {
      if (rejectedScanReturn.current) {
        // A rejected pairing returns to the scan page, not the role
        // selection: the error banner above tells why, and the back button
        // still reaches the role cards.
        rejectedScanReturn.current = false;
        setPickedRole("secondary");
        setConfirmTarget(null);
        setManualInviteOpen(false);
        setInviteOpen(false);
        setInviteCopied(false);
        setScreen("discovery");
        return;
      }
      manualDiscoveryRequested.current = false;
      setPickedRole(null);
      setConfirmTarget(null);
      setManualInviteOpen(false);
      setInviteOpen(false);
      setInviteCopied(false);
      setScreen("setup");
    } else if (enteredRunningState && pickedRole === null) {
      setScreen((current) => (current === "setup" ? "overview" : current));
    }
  }, [pickedRole, snapshot.role, snapshot.service_running]);

  useEffect(() => {
    if (snapshot.role !== "secondary" || !snapshot.service_running || snapshot.stream_id) {
      setSecondaryConnectingElapsed(false);
      return;
    }
    const timer = window.setTimeout(() => {
      setSecondaryConnectingElapsed(true);
    }, 2000);
    return () => window.clearTimeout(timer);
  }, [snapshot.role, snapshot.service_running, Boolean(snapshot.stream_id)]);

  const hasPendingRequest = snapshot.pairing_requests.some((request) => request.status === "pending");
  // A primary start runs in a background worker; the sheet stays on the
  // overview and polls until the listener either serves or reports an error.
  const primaryStarting = snapshot.primary_starting && !snapshot.service_running;
  // "Waiting for the primary" is honest only while the request is genuinely
  // pending approval. Once the runtime records any other error the pairing is
  // approved but unreachable, and the error box plus the recovering state tell
  // that story instead of a stale waiting banner.
  const awaitingApprovalError = snapshot.last_error?.toLowerCase().includes("awaiting approval") ?? false;
  const awaitingSecondaryApproval = snapshot.role === "secondary" && snapshot.service_running && !snapshot.stream_id && (awaitingApprovalError || (!snapshot.last_error && secondaryConnectingElapsed));
  const secondaryServiceMayChange = snapshot.role === "secondary" && snapshot.service_running;
  const primaryDeviceStateMayChange = snapshot.role === "primary" && snapshot.devices.some((device) => device.role === "secondary" && device.status === "paired");
  const shouldPoll = primaryStarting || snapshot.pairing_pending || secondaryServiceMayChange || primaryDeviceStateMayChange || (snapshot.service_running && (hasPendingRequest || awaitingSecondaryApproval));
  const commandBusy = mutationBusy || initializing || discovering;
  const cancelBusy = mutationBusy;

  // Keep the overview live while pairing decisions are outstanding or a
  // secondary is retrying a temporarily unavailable primary.
  useEffect(() => {
    if (!shouldPoll) return;
    let alive = true;
    let attempts = 0;
    let timer: number | undefined;
    const poll = async () => {
      if (!alive) return;
      const maxAttempts = primaryStarting || snapshot.pairing_pending || secondaryServiceMayChange || primaryDeviceStateMayChange ? Number.POSITIVE_INFINITY : 12;
      if (attempts >= maxAttempts) return;
      if (mutationInFlight.current || pollingInFlight.current || discoveryInFlight.current) {
        timer = window.setTimeout(() => void poll(), 500);
        return;
      }
      attempts += 1;
      const baseDelay = primaryStarting ? 1000 : 5000;
      let retryDelay = baseDelay;
      const generation = ++operationGeneration.current;
      pollingInFlight.current = true;
      setPollingBusy(true);
      try {
        const next = await bridge.syncDevices({ action: "status" });
        attempts = 0;
        if (alive && mounted.current && generation === operationGeneration.current) {
          onSnapshot(next);
          setError(null);
        }
      } catch (e) {
        retryDelay = Math.min(30000, baseDelay * 2 ** Math.max(0, attempts - 1));
        if (alive && mounted.current && generation === operationGeneration.current) setError(e instanceof Error ? e.message : String(e));
      } finally {
        pollingInFlight.current = false;
        if (alive && mounted.current) setPollingBusy(false);
      }
      if (alive && generation === operationGeneration.current && attempts < maxAttempts) timer = window.setTimeout(() => void poll(), retryDelay);
    };
    const pollInterval = primaryStarting ? 1000 : 5000;
    timer = window.setTimeout(() => void poll(), pollInterval);
    return () => { alive = false; if (timer !== undefined) window.clearTimeout(timer); };
  }, [onSnapshot, pollGeneration, primaryDeviceStateMayChange, primaryStarting, secondaryServiceMayChange, shouldPoll, snapshot.pairing_pending]);

  // The fast-accepted start leaves role empty until the worker finishes, but
  // the card it is bringing up is the primary's — without `primaryStarting`
  // here the badge reads 从端 with the secondary's paired-count line for the
  // whole starting window.
  const isPrimary = snapshot.role === "primary" || primaryStarting;
  const isSecondary = snapshot.role === "secondary";
  const secondaryHasError = isSecondary && snapshot.service_running && Boolean(snapshot.last_error);
  const secondaryDegraded = secondaryHasError && hasRecentSecondaryContact(snapshot);
  const secondaryRecovering = secondaryHasError && !secondaryDegraded;
  const status = syncStatus(snapshot);
  const pendingRequests = snapshot.pairing_requests.filter((request) => request.status === "pending");
  const connectedDevices = snapshot.devices.filter((device) => device.status !== "revoked");
  const primaryDevices = connectedDevices.filter((device) => device.role === "primary");
  const secondaryDevices = connectedDevices.filter((device) => device.role === "secondary");
  const pairedPrimaryCount = primaryDevices.length || (isSecondary && snapshot.primary_id ? 1 : 0);
  const errorDetail = (error ?? snapshot.last_error)?.trim() || null;
  const visibleError = syncErrorMessage(errorDetail);
  // The two halves of a secondary's sync, each with its own finish line: the
  // initial upload snapshot (pages still ACKing → queued items) and the
  // primary-data downlink (absent until the primary shares its first page).
  const uploadQueued = snapshot.pending + snapshot.in_flight;
  const uploadState = snapshot.upload_complete === true
    ? t("sync.prog.done")
    : snapshot.upload_complete === false
      ? uploadQueued > 0 ? t("sync.prog.running_items", { n: uploadQueued }) : t("sync.prog.running")
      : t("sync.prog.wait");
  const downloadState = snapshot.download_complete === true
    ? t("sync.prog.done")
    : snapshot.download_complete === false
      ? t("sync.prog.running")
      : t("sync.prog.wait_share");
  const rejectedByPrimary = errorDetail?.toLowerCase().includes("pairing request was rejected") ?? false;

  // The waiting screen ends exactly two ways: a live stream means the primary
  // approved and sync is up (enter the device list); a rejection returns to
  // the scan page with the mapped error banner telling the story. The
  // rejectedScanReturn flag keeps the role teardown from bouncing the sheet
  // to the role-selection cards while that banner is the whole point.
  useEffect(() => {
    if (screen !== "waiting") return;
    if (snapshot.stream_id) { setScreen("overview"); return; }
    if (rejectedByPrimary) {
      rejectedScanReturn.current = true;
      setScreen("discovery");
    }
  }, [screen, snapshot.stream_id, rejectedByPrimary]);

  const nextFromSetup = () => {
    if (pickedRole === "primary") {
      // 0.0.0.0 with an auto-bound port; no network questions — the wizard
      // promises the defaults, the backend owns the fallbacks.
      void run({ action: "start_primary", bind: "0.0.0.0:47821", advertise_host: null, allow_lan: true }, () => setScreen("overview"));
    } else if (pickedRole === "secondary") {
      setManualInviteOpen(false);
      setScreen("discovery");
    }
  };

  const pairConfirmed = () => {
    if (!confirmTarget) return;
    void run({ action: "pair_discovered", primary_id: confirmTarget.primary_id, endpoint: confirmTarget.endpoint, certificate_pin: confirmTarget.certificate_pin, origin: null, display_name: null, os: null }, () => setScreen("waiting"));
  };

  const pairWithInvite = () => {
    const invite = manualInvite.trim();
    if (!invite) return;
    void run({ action: "pair", invite, origin: null, display_name: null, os: null }, () => {
      setManualInvite("");
      setManualInviteOpen(false);
      setScreen("waiting");
    });
  };

  const createInvite = () => {
    setInviteCopied(false);
    void run({ action: "create_invite" }, () => setInviteOpen(true));
  };

  const hideInvite = () => {
    setInviteOpen(false);
    setInviteCopied(false);
  };

  const copyInvite = async () => {
    if (!snapshot.invite_url) return;
    if (await copyText(snapshot.invite_url)) {
      setInviteCopied(true);
      window.setTimeout(() => setInviteCopied(false), 1600);
    }
  };

  const deviceRow = (device: (typeof snapshot.devices)[number]) => {
    const hostName = device.display_name.trim() || device.origin.trim() || device.device_id;
    const deviceMeta = [
      !device.snapshot_complete ? t("sync.device.snapshot_incomplete") : "",
      device.last_generation != null ? t("sync.device.gen", { n: device.last_generation }) : "",
      device.last_merged_at_ms != null ? t("sync.device.merged", { time: relTime(device.last_merged_at_ms) }) : "",
    ].filter(Boolean).join(" · ");
    return (
      <div className="sync-dev" key={device.device_id}>
        <span className={`sync-dev-dot${device.status === "paused" ? " is-paused" : ""}`} aria-hidden="true" />
        <span className="sync-dev-platform" role="img" aria-label={device.os || "unknown system"} title={device.os || "unknown system"}><IconDevicePlatform os={device.os} size={14} /></span>
        <span className="sync-dev-name" title={hostName}>{hostName}</span>
        {device.status === "pending" ? <code>{t("sync.device.pending")}</code> : device.status === "paused" ? <code>{t("sync.device.paused")}</code> : device.status === "reconcile_required" ? <code>{t("sync.device.reconcile")}</code> : null}
        {device.role === "secondary" && device.status === "paired" && deviceMeta ? <span className="sync-dev-when">{deviceMeta}</span> : null}
        <button type="button" className="sync-disc" disabled={commandBusy} onClick={() => setPendingConfirm({
          request: { action: "revoke", device_id: device.device_id },
          message: isSecondary && uploadQueued > 0
            ? t("sync.revoke.pending_confirm", { name: hostName, count: uploadQueued })
            : t("sync.revoke.confirm", { name: hostName }),
        })}>{t("sync.disconnect")}</button>
      </div>
    );
  };

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet sheet-srv" role="dialog" aria-modal="true" aria-busy={pollingBusy} aria-label={t("sync.title")} onClick={(event) => event.stopPropagation()}>
        <div className="sheet-hd">
          <div className="sheet-hd-main"><h2 className="sec-label">{t("sync.title")}</h2></div>
          <div className="sheet-actions">
            <button type="button" className="sheet-close" aria-label={t("sync.close")} onClick={onClose}><IconClose size={12} /></button>
          </div>
        </div>
        <div className="sheet-body sync-body">
          {visibleError && dismissedError !== errorDetail ? (
            <div className="sheet-notice is-error" role="alert">
              <IconWarn size={12} />
              <div className="sync-error-head">
                <div className="sync-error-copy">
                  {secondaryDegraded ? <strong>{t("sync.degraded.hint")}</strong> : secondaryRecovering ? <strong>{t("sync.recovery.hint")}</strong> : null}
                  <span>{visibleError}</span>
                  {errorDetail ? <details className="sync-error-details"><summary>{t("sync.error.details")}</summary><code>{errorDetail.slice(0, 600)}</code></details> : null}
                </div>
                <div className="sync-error-actions">
                  <button type="button" className="sync-error-btn" disabled={commandBusy} onClick={() => void run({ action: "retry_connect" })}>{t("sync.error.retry")}</button>
                  <button type="button" className="sync-error-close" title={t("sync.error.dismiss")} onClick={() => setDismissedError(errorDetail)}><IconClose size={11} /></button>
                </div>
              </div>
            </div>
          ) : null}
          {snapshot.discovery_error ? <div className="sheet-notice" role="status"><IconWarn size={12} />{t("sync.discovery.permission_error", { message: snapshot.discovery_error })}</div> : null}
          {pendingConfirm ? <div className="sync-confirm" role="alertdialog" aria-live="assertive" aria-label={t("sync.confirm.title")}>
            <div className="sync-confirm-copy"><strong>{t("sync.confirm.title")}</strong><span>{pendingConfirm.message}</span></div>
            <div className="sync-actions"><button type="button" className="btn-accent" disabled={commandBusy} onClick={() => { const { request } = pendingConfirm; setPendingConfirm(null); void run(request); }}>{t("sync.confirm.accept")}</button><button type="button" className="btn-quiet" disabled={cancelBusy} onClick={() => setPendingConfirm(null)}>{t("sync.confirm.cancel")}</button></div>
          </div> : null}

          {screen === "setup" ? (
            <>
              {snapshot.pairing_pending ? (
                <>
                  <div className="sync-waiting"><strong>{t("sync.pairing.recovering")}</strong><span>{t("sync.pairing.recovering_hint")}</span></div>
                  <div className="sync-btnrow">
                    <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => setPendingConfirm({ request: { action: "unpair" }, message: t("sync.pairing.cancel_confirm") })}>{t("sync.disconnect")}</button>
                  </div>
                </>
              ) : (
                <>
                  <div className="sync-ask">{t("sync.role.ask")}</div>
                  <div className="sync-choice-grid">
                    <button type="button" className="sync-choice-card" aria-pressed={pickedRole === "primary"} disabled={commandBusy} onClick={() => setPickedRole("primary")}>
                      <span className="sync-choice-ico"><IconRadioTower size={22} /></span>
                      <strong>{t("sync.role.primary")}</strong>
                      <span>{t("sync.primary.hint")}</span>
                    </button>
                    <button type="button" className="sync-choice-card" aria-pressed={pickedRole === "secondary"} disabled={commandBusy} onClick={() => setPickedRole("secondary")}>
                      <span className="sync-choice-ico"><IconMonitorDown size={22} /></span>
                      <strong>{t("sync.role.secondary")}</strong>
                      <span>{t("sync.pair.hint")}</span>
                    </button>
                  </div>
                  <div className="sync-btnrow">
                    <button type="button" className="btn-accent" disabled={!pickedRole || commandBusy} onClick={nextFromSetup}>{t("sync.setup.next")}</button>
                  </div>
                </>
              )}
            </>
          ) : null}

          {screen === "discovery" ? (
            <>
              <div className="sync-crumb">
                <button type="button" className="sync-back" aria-label={t("sync.discovery.back")} onClick={() => setScreen("setup")}><IconBack size={12} /></button>
                <b>{t("sync.discovery.title")}</b>
              </div>
              {discovering ? (
                <div className="sync-scan">
                  <div className="sync-dualring" aria-hidden="true"><i /><i /></div>
                  <div>{t("sync.discovery.scanning")}</div>
                </div>
              ) : snapshot.discovered_primaries.length ? (
                snapshot.discovered_primaries.map((primary) => (
                  <button type="button" className="sync-found" key={primary.primary_id} disabled={commandBusy || primary.identity_conflict} onClick={() => { setConfirmTarget(primary); setScreen("confirm"); }}>
                    <span className="sync-found-name">{primary.display_name}{primary.identity_conflict ? <code> · {t("sync.discovery.conflict")}</code> : null}</span>
                    <code>{primary.endpoint}</code>
                    <span className="sync-found-go">{primary.identity_conflict ? null : t("sync.discovery.connect")}</span>
                  </button>
                ))
              ) : (
                <div className="sync-scan"><div>{t("sync.discovery.empty")}</div></div>
              )}
              {!discovering && !snapshot.discovered_primaries.length ? (
                <>
                  <div className="sync-crumb">
                    <span>{t("sync.discovery.none")}</span>
                    <div className="sync-discovery-actions">
                      <button type="button" className="sync-back" disabled={commandBusy} onClick={() => void discover()}>{t("sync.discovery.rescan")}</button>
                      <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => setManualInviteOpen((open) => !open)}>{manualInviteOpen ? t("sync.discovery.manual.hide") : t("sync.discovery.manual.title")}</button>
                    </div>
                  </div>
                  {manualInviteOpen ? (
                    <div className="sync-manual-panel sync-credbox">
                      <div className="sync-diagnostics-label"><span>{t("sync.discovery.manual.title")}</span></div>
                      <p className="explain">{t("sync.discovery.manual.explain")}</p>
                      <textarea className="sync-invite" value={manualInvite} onChange={(event) => setManualInvite(event.target.value)} placeholder={t("sync.discovery.manual.placeholder")} aria-label={t("sync.discovery.manual.title")} />
                      <div className="sync-btnrow">
                        <button type="button" className="btn-accent" disabled={commandBusy || !manualInvite.trim()} onClick={pairWithInvite}>{t("sync.discovery.manual.go")}</button>
                      </div>
                    </div>
                  ) : null}
                </>
              ) : null}
            </>
          ) : null}

          {screen === "confirm" && confirmTarget ? (
            <>
              <div className="sync-crumb"><b>{t("sync.pair.confirm.title")}</b></div>
              <div className="sync-confirmbox">
                <strong>{t("sync.pair.confirm.ask", { name: confirmTarget.display_name })}</strong>
                <span>{confirmTarget.endpoint} · {t("sync.pair.fingerprint", { pin: shortPin(confirmTarget.certificate_pin) })}</span>
              </div>
              <div className="sync-btnrow">
                <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => setScreen("discovery")}>{t("sync.confirm.cancel")}</button>
                <button type="button" className="btn-accent" disabled={commandBusy || confirmTarget.identity_conflict} onClick={pairConfirmed}>{t("sync.pair.go")}</button>
              </div>
            </>
          ) : null}

          {screen === "waiting" ? (
            <>
              <div className="sync-scan" role="status">
                <div className="sync-dualring" aria-hidden="true"><i /><i /></div>
                <div>{t("sync.pairing.waiting_primary")}</div>
                <div>{t("sync.pairing.waiting_hint")}</div>
              </div>
              <div className="sync-btnrow">
                <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => setPendingConfirm({ request: { action: "unpair" }, message: t("sync.pairing.cancel_confirm") })}>{t("sync.pairing.cancel_wait")}</button>
              </div>
            </>
          ) : null}

          {screen === "overview" && (snapshot.role !== "unconfigured" || snapshot.primary_starting) && (snapshot.service_running || snapshot.primary_starting) ? (
            <>
              {primaryStarting ? <div className="sync-waiting" role="status">{t("sync.primary.starting")}</div> : null}
              <div className="rolecard">
                <div className="role-row">
                  <div>
                    <span className="role-badge">{isPrimary ? t("sync.role.primary") : t("sync.role.secondary")}</span>
                    <span className="role-name">{t("sync.local_device")}</span>
                    <div className="meta">
                      {isPrimary
                        ? <>{snapshot.endpoint ? <span className="num">{snapshot.endpoint}</span> : null}{snapshot.endpoint ? " · " : null}{t("sync.ov.awaiting")}</>
                        : <>{t("sync.ov.meta_sec", { n: pairedPrimaryCount })}{snapshot.last_connected_at_ms ? <> · {t("sync.last_contact", { time: relTime(snapshot.last_connected_at_ms) })}</> : null}</>}
                    </div>
                  </div>
                  <div className="role-right">
                    <span className={`role-state role-state-${status.kind}`} aria-live="polite">{status.label}</span>
                    <button type="button" className="stopbtn" disabled={commandBusy || !snapshot.service_running} onClick={() => {
                      if (isSecondary && uploadQueued > 0) {
                        setPendingConfirm({ request: { action: "stop" }, message: t("sync.stop.pending_confirm", { count: uploadQueued }) });
                        return;
                      }
                      void run({ action: "stop" });
                    }} aria-label={t("sync.stop_disconnect")}>
                      <span className="stopicon" aria-hidden="true" />{t("sync.stop_disconnect")}
                    </button>
                  </div>
                </div>
                {isPrimary ? (
                  <>
                    <label className="sync-share-setting">
                      <input
                        type="checkbox"
                        checked={shareOverride ?? snapshot.primary_data_sharing_enabled}
                        disabled={commandBusy}
                        onChange={(event) => toggleSharing(event.currentTarget.checked)}
                      />
                      <span className="sync-share-copy">
                        <strong>{t("sync.share.title")}</strong>
                        <small>{t("sync.share.hint")}</small>
                      </span>
                    </label>
                    <label className="sync-share-setting">
                      <input
                        type="checkbox"
                        checked={autoApproveOverride ?? snapshot.primary_auto_approve_enabled}
                        disabled={commandBusy}
                        onChange={(event) => toggleAutoApprove(event.currentTarget.checked)}
                      />
                      <span className="sync-share-copy">
                        <strong>{t("sync.autoapprove.title")}</strong>
                        <small>{t("sync.autoapprove.hint")}</small>
                      </span>
                    </label>
                  </>
                ) : null}
              </div>
              {isSecondary && awaitingSecondaryApproval ? <div className="sync-waiting">{t("sync.pairing.waiting_primary")}</div> : null}
              {isSecondary ? (
                <>
                  <div className="sync-progress" aria-live="polite">
                    <div className="sync-progress-row"><span>{t("sync.prog.upload")}</span><code>{uploadState}</code></div>
                    <div className="sync-progress-row"><span>{t("sync.prog.download")}</span><code>{downloadState}</code></div>
                  </div>
                  {!awaitingSecondaryApproval ? (
                    <div className="sync-btnrow">
                      <button type="button" className="btn-accent" disabled={commandBusy} onClick={() => void run({ action: "retry_connect" })}>{t("sync.action.reconnect")}</button>
                      <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => setPendingConfirm({ request: { action: "reset_secondary" }, message: t("sync.reset.confirm") })}>{t("sync.reset.button")}</button>
                    </div>
                  ) : null}
                </>
              ) : null}
              {isPrimary && pendingRequests.length ? (
                <>
                  <div className="sync-sect2">{t("sync.ov.pending_sect")}</div>
                  {pendingRequests.map((request) => (
                    <div className="sync-pending" key={request.request_id}>
                      <span className="sync-pdot" aria-hidden="true" />
                      <span className="sync-pname">{request.display_name}<code>{request.os}</code></span>
                      <button type="button" className="sync-pok" disabled={commandBusy} onClick={() => void run({ action: "approve", request_id: request.request_id })}>{t("sync.approve")}</button>
                      <button type="button" className="sync-pno" disabled={commandBusy} onClick={() => void run({ action: "reject", request_id: request.request_id })}>{t("sync.reject")}</button>
                    </div>
                  ))}
                </>
              ) : null}
              {isPrimary && snapshot.service_running ? (
                <div className="sync-manual-panel sync-credbox sync-secondary-panel">
                  <div className="sync-secondary-head">
                    <span>{t("sync.ov.secondary_sect")}</span>
                    <button type="button" className="btn-quiet" disabled={commandBusy} onClick={createInvite}>{t("sync.invite.create")}</button>
                  </div>
                  {secondaryDevices.length ? secondaryDevices.map((device) => deviceRow(device)) : <div className="sync-secondary-empty">{t("sync.ov.secondary_empty")}</div>}
                  {inviteOpen && snapshot.invite_url ? (
                    <div className="sync-invite-reveal">
                      <div className="sync-invite-reveal-head">
                        <span>{t("sync.invite.title")}</span>
                        <button type="button" className="btn-quiet" disabled={commandBusy} onClick={hideInvite}>{t("sync.invite.hide")}</button>
                      </div>
                      <textarea className="sync-invite" readOnly value={snapshot.invite_url} aria-label={t("sync.invite.title")} onFocus={(event) => event.currentTarget.select()} />
                      <div className="sync-btnrow">
                        <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void copyInvite()}>{inviteCopied ? t("sync.invite.copied") : t("sync.invite.copy")}</button>
                      </div>
                    </div>
                  ) : null}
                </div>
              ) : null}
              {isSecondary && snapshot.primary_id ? (
                <>
                  <div className="sync-sect2">{t("sync.ov.paired_sect_sec")}</div>
                  {primaryDevices.length ? primaryDevices.map((device) => deviceRow(device)) : (
                    <div className="sync-dev" key={snapshot.primary_id}>
                      <span className={`sync-dev-dot${secondaryRecovering ? " is-paused" : secondaryDegraded ? " is-error" : ""}`} aria-hidden="true" />
                      <span className="sync-dev-platform" role="img" aria-label={snapshot.primary_os || "unknown system"} title={snapshot.primary_os || "unknown system"}><IconDevicePlatform os={snapshot.primary_os ?? ""} size={14} /></span>
                      <span className="sync-dev-name">{snapshot.primary_display_name?.trim() || snapshot.discovered_primaries.find((primary) => primary.primary_id === snapshot.primary_id)?.display_name.trim() || t("sync.role.primary")}<code>{snapshot.endpoint ?? snapshot.primary_id}</code></span>
                      <code>{secondaryRecovering ? t("sync.state.recovering") : secondaryDegraded ? t("sync.state.degraded") : t("sync.state.running")}</code>
                    </div>
                  )}
                </>
              ) : null}
            </>
          ) : null}

        </div>
      </div>
    </div>
  );
}

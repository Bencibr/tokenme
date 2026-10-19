import { useCallback, useEffect, useRef, useState } from "react";
import type { DiscoveredPrimaryView, SyncDevicesSnapshot, SyncManagementRequest } from "../types";
import { bridge } from "../lib/bridge";
import { t } from "../lib/i18n";
import { IconBack, IconCheck, IconClose, IconDevicePlatform, IconMonitorDown, IconRadioTower, IconWarn } from "./Icons";

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

type Screen = "setup" | "discovery" | "confirm" | "done" | "overview";

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

function syncStatus(snapshot: SyncDevicesSnapshot): { kind: "running" | "recovering" | "stopped" | "disconnected" | "error"; label: string } {
  if (snapshot.role === "secondary" && snapshot.service_running && snapshot.last_error) return { kind: "recovering", label: t("sync.state.recovering") };
  if (snapshot.last_error || snapshot.operation_state === "error") return { kind: "error", label: t("sync.state.error") };
  if (snapshot.service_running) return { kind: "running", label: t("sync.state.running") };
  if (snapshot.operation_state === "running") return { kind: "recovering", label: t("sync.state.recovering") };
  if (snapshot.operation_state === "offline") return { kind: "disconnected", label: t("sync.state.disconnected") };
  return { kind: "stopped", label: t("sync.state.stopped") };
}

function syncErrorMessage(error: string | null): string | null {
  if (!error) return null;
  const normalized = error.toLowerCase();
  if (normalized.includes("connection refused") || normalized.includes("connection reset") || normalized.includes("transport i/o failed") || normalized.includes("cannot notify primary")) return t("sync.error.unreachable");
  if (normalized.includes("certificate") || normalized.includes("pin mismatch")) return t("sync.error.security");
  if (normalized.includes("pairing invite") && normalized.includes("expired")) return t("sync.error.invite_expired");
  if (normalized.includes("invalid pairing invite")) return t("sync.error.invite_invalid");
  if (normalized.includes("address already in use")) return t("sync.error.listener");
  return t("sync.error.generic");
}

function syncScreenFor(snapshot: SyncDevicesSnapshot): Screen {
  if (snapshot.role === "unconfigured") return "setup";
  if (!snapshot.service_running) return "setup";
  return "overview";
}

export function SyncSheet({ snapshot, onSnapshot, onClose }: { snapshot: SyncDevicesSnapshot; onSnapshot: (next: SyncDevicesSnapshot) => void; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [initializing, setInitializing] = useState(true);
  const [discovering, setDiscovering] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [screen, setScreen] = useState<Screen>(() => syncScreenFor(snapshot));
  const [pickedRole, setPickedRole] = useState<"primary" | "secondary" | null>(null);
  const [confirmTarget, setConfirmTarget] = useState<DiscoveredPrimaryView | null>(null);
  const [manualInvite, setManualInvite] = useState("");
  const [manualInviteOpen, setManualInviteOpen] = useState(false);
  const [inviteCopied, setInviteCopied] = useState(false);
  const [inviteOpen, setInviteOpen] = useState(false);
  const [pollGeneration, setPollGeneration] = useState(0);
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirmation | null>(null);
  const discoveryInFlight = useRef(false);
  const manualDiscoveryRequested = useRef(false);
  const operationGeneration = useRef(0);
  const operationInFlight = useRef<"initial" | "mutation" | "discover" | "poll" | null>(null);
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
    if (!mounted.current || operationInFlight.current) return;
    setPendingConfirm(null);
    const generation = ++operationGeneration.current;
    operationInFlight.current = "mutation";
    setBusy(true);
    setError(null);
    setPollGeneration((value) => value + 1);
    try {
      const next = await bridge.syncDevices(request);
      if (!mounted.current || generation !== operationGeneration.current) return;
      setError(null);
      onSnapshot(next);
      if (request.action === "stop") setScreen("setup");
      else onSuccess?.(next);
      // Mutations return a snapshot, but take one explicit status read after
      // the write so stale pairing/device rows cannot remain on screen.
      const latest = await bridge.syncDevices({ action: "status" });
      if (mounted.current && generation === operationGeneration.current) {
        setError(null);
        onSnapshot(latest);
      }
    } catch (e) {
      if (mounted.current && generation === operationGeneration.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (operationInFlight.current === "mutation" && generation === operationGeneration.current) operationInFlight.current = null;
      if (mounted.current && generation === operationGeneration.current) setBusy(false);
    }
  };

  const discover = useCallback(async () => {
    if (!mounted.current || operationInFlight.current) return;
    manualDiscoveryRequested.current = true;
    discoveryInFlight.current = true;
    const generation = ++operationGeneration.current;
    operationInFlight.current = "discover";
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
      if (operationInFlight.current === "discover" && generation === operationGeneration.current) operationInFlight.current = null;
      if (mounted.current) setDiscovering(false);
    }
  }, [onSnapshot]);

  useEffect(() => {
    let alive = true;
    // Defer even the local status read by one task so the newly mounted sheet
    // gets a paint before any synchronous Tauri command work starts.
    const statusTimer = window.setTimeout(() => {
      if (!alive || operationInFlight.current) return;
      const generation = ++operationGeneration.current;
      operationInFlight.current = "initial";
      void bridge.syncDevices().then((next) => {
        if (!alive || generation !== operationGeneration.current) return;
        setError(null);
        onSnapshot(next);
      }).catch((e) => {
        if (alive && generation === operationGeneration.current) setError(e instanceof Error ? e.message : String(e));
      }).finally(() => {
        if (operationInFlight.current === "initial" && generation === operationGeneration.current) operationInFlight.current = null;
        if (alive && generation === operationGeneration.current) setInitializing(false);
      });
    }, 0);
    return () => { alive = false; window.clearTimeout(statusTimer); };
  }, [onSnapshot]);

  // A successful backend snapshot is authoritative. Clear a previous
  // transport error even when the sheet stayed mounted through recovery.
  useEffect(() => {
    if (!snapshot.last_error) setError(null);
  }, [snapshot.last_error]);

  // Entering the discovery screen starts one bounded mDNS browse; the small
  // retry loop only waits out an in-flight initial status read.
  useEffect(() => {
    if (screen !== "discovery") return;
    let alive = true;
    let timer: number | undefined;
    const attempt = () => {
      if (!alive || !mounted.current) return;
      if (operationInFlight.current) {
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
    const roleSelection = snapshot.role === "unconfigured" || !snapshot.service_running;
    const enteredRoleSelection = roleSelection && (wasRole !== snapshot.role || wasRunning !== snapshot.service_running);
    const enteredRunningState = !roleSelection && (wasRole !== snapshot.role || wasRunning !== snapshot.service_running);
    if (enteredRoleSelection) {
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

  const hasPendingRequest = snapshot.pairing_requests.some((request) => request.status === "pending");
  const awaitingSecondaryApproval = snapshot.role === "secondary" && snapshot.service_running && !snapshot.stream_id;
  const secondaryServiceMayChange = snapshot.role === "secondary" && snapshot.service_running;
  const primaryDeviceStateMayChange = snapshot.role === "primary" && snapshot.devices.some((device) => device.role === "secondary" && device.status === "paired");
  const shouldPoll = snapshot.pairing_pending || secondaryServiceMayChange || primaryDeviceStateMayChange || (snapshot.service_running && (hasPendingRequest || awaitingSecondaryApproval));
  const commandBusy = busy || initializing || discovering;

  // Keep the overview live while pairing decisions are outstanding or a
  // secondary is retrying a temporarily unavailable primary.
  useEffect(() => {
    if (!shouldPoll) return;
    let alive = true;
    let attempts = 0;
    let timer: number | undefined;
    const poll = async () => {
      if (!alive) return;
      const maxAttempts = snapshot.pairing_pending || secondaryServiceMayChange || primaryDeviceStateMayChange ? Number.POSITIVE_INFINITY : 12;
      if (attempts >= maxAttempts) return;
      if (operationInFlight.current) {
        timer = window.setTimeout(() => void poll(), 500);
        return;
      }
      attempts += 1;
      let retryDelay = 5000;
      const generation = ++operationGeneration.current;
      operationInFlight.current = "poll";
      setBusy(true);
      try {
        const next = await bridge.syncDevices({ action: "status" });
        if (alive && mounted.current && generation === operationGeneration.current) { onSnapshot(next); setError(null); }
      } catch (e) {
        retryDelay = Math.min(30000, 5000 * 2 ** Math.max(0, attempts - 1));
        if (alive && mounted.current && generation === operationGeneration.current) setError(e instanceof Error ? e.message : String(e));
      } finally {
        if (operationInFlight.current === "poll" && generation === operationGeneration.current) operationInFlight.current = null;
        if (alive && mounted.current && generation === operationGeneration.current) setBusy(false);
      }
      if (alive && generation === operationGeneration.current && attempts < maxAttempts) timer = window.setTimeout(() => void poll(), retryDelay);
    };
    timer = window.setTimeout(() => void poll(), 5000);
    return () => { alive = false; if (timer !== undefined) window.clearTimeout(timer); };
  }, [onSnapshot, pollGeneration, primaryDeviceStateMayChange, secondaryServiceMayChange, shouldPoll, snapshot.pairing_pending]);

  const isPrimary = snapshot.role === "primary";
  const isSecondary = snapshot.role === "secondary";
  const secondaryRecovering = isSecondary && snapshot.service_running && Boolean(snapshot.last_error);
  const status = syncStatus(snapshot);
  const pendingRequests = snapshot.pairing_requests.filter((request) => request.status === "pending");
  const connectedDevices = snapshot.devices.filter((device) => device.status !== "revoked");
  const primaryDevices = connectedDevices.filter((device) => device.role === "primary");
  const secondaryDevices = connectedDevices.filter((device) => device.role === "secondary");
  const pairedPrimaryCount = primaryDevices.length || (isSecondary && snapshot.primary_id ? 1 : 0);
  const visibleError = syncErrorMessage(error ?? snapshot.last_error);

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
    void run({ action: "pair_discovered", primary_id: confirmTarget.primary_id, endpoint: confirmTarget.endpoint, certificate_pin: confirmTarget.certificate_pin, origin: null, display_name: null, os: null }, () => setScreen("done"));
  };

  const pairWithInvite = () => {
    const invite = manualInvite.trim();
    if (!invite) return;
    void run({ action: "pair", invite, origin: null, display_name: null, os: null }, () => {
      setManualInvite("");
      setManualInviteOpen(false);
      setScreen("done");
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
    const hostName = device.origin.trim() || device.display_name.trim() || device.device_id;
    return (
      <div className="sync-dev" key={device.device_id}>
        <span className={`sync-dev-dot${device.status === "paused" ? " is-paused" : ""}`} aria-hidden="true" />
        <span className="sync-dev-platform" role="img" aria-label={device.os || "unknown system"} title={device.os || "unknown system"}><IconDevicePlatform os={device.os} size={14} /></span>
        <span className="sync-dev-name" title={hostName}>{hostName}</span>
        {device.status === "pending" ? <code>{t("sync.device.pending")}</code> : device.status === "paused" ? <code>{t("sync.device.paused")}</code> : device.status === "reconcile_required" ? <code>{t("sync.device.reconcile")}</code> : null}
        <button type="button" className="sync-disc" disabled={commandBusy} onClick={() => setPendingConfirm({ request: { action: "revoke", device_id: device.device_id }, message: t("sync.revoke.confirm", { name: hostName }) })}>{t("sync.disconnect")}</button>
      </div>
    );
  };

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet sheet-srv" role="dialog" aria-modal="true" aria-label={t("sync.title")} onClick={(event) => event.stopPropagation()}>
        <div className="sheet-hd">
          <div className="sheet-hd-main"><h2 className="sec-label">{t("sync.title")}</h2></div>
          <div className="sheet-actions">
            <button type="button" className="sheet-close" aria-label={t("sync.close")} onClick={onClose}><IconClose size={12} /></button>
          </div>
        </div>
        <div className="sheet-body sync-body">
          {visibleError && !secondaryRecovering ? <div className="sheet-notice is-error" role="alert"><IconWarn size={12} /><span>{visibleError}</span></div> : null}
          {secondaryRecovering ? <div className="sheet-notice" role="status"><IconWarn size={12} />{t("sync.recovery.hint")}</div> : null}
          {snapshot.discovery_error ? <div className="sheet-notice" role="status"><IconWarn size={12} />{t("sync.discovery.permission_error", { message: snapshot.discovery_error })}</div> : null}
          {pendingConfirm ? <div className="sync-confirm" role="alertdialog" aria-live="assertive" aria-label={t("sync.confirm.title")}>
            <div className="sync-confirm-copy"><strong>{t("sync.confirm.title")}</strong><span>{pendingConfirm.message}</span></div>
            <div className="sync-actions"><button type="button" className="btn-accent" disabled={commandBusy} onClick={() => { const { request } = pendingConfirm; setPendingConfirm(null); void run(request); }}>{t("sync.confirm.accept")}</button><button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => setPendingConfirm(null)}>{t("sync.confirm.cancel")}</button></div>
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

          {screen === "done" ? (
            <>
              <div className="sync-done">
                <div className="sync-done-ok"><IconCheck size={16} /></div>
                <div className="sync-done-label">{t("sync.done.title")}</div>
              </div>
              <div className="sync-btnrow">
                <button type="button" className="btn-accent" onClick={() => setScreen("overview")}>{t("sync.done.next")}</button>
              </div>
            </>
          ) : null}

          {screen === "overview" && snapshot.role !== "unconfigured" && snapshot.service_running ? (
            <>
              <div className="rolecard">
                <div className="role-row">
                  <div>
                    <span className="role-badge">{isPrimary ? t("sync.role.primary") : t("sync.role.secondary")}</span>
                    <span className="role-name">{t("sync.local_device")}</span>
                    <div className="meta">
                      {isPrimary
                        ? <>{snapshot.endpoint ? <span className="num">{snapshot.endpoint}</span> : null}{snapshot.endpoint ? " · " : null}{t("sync.ov.awaiting")}</>
                        : <>{t("sync.ov.meta_sec", { n: pairedPrimaryCount })}{snapshot.last_connected_at_ms ? <> · {t("sync.last_sync", { time: relTime(snapshot.last_connected_at_ms) })}</> : null}</>}
                    </div>
                  </div>
                  <div className="role-right">
                    <span className={`role-state role-state-${status.kind}`} aria-live="polite">{status.label}</span>
                    <button type="button" className="stopbtn" disabled={commandBusy || !snapshot.service_running} onClick={() => void run({ action: "stop" })} aria-label={t("sync.stop_disconnect")}>
                      <span className="stopicon" aria-hidden="true" />{t("sync.stop_disconnect")}
                    </button>
                  </div>
                </div>
              </div>
              {isPrimary ? (
                <div className="sync-share-row">
                  <div><strong>{t("sync.primary.share.title")}</strong><span>{t("sync.primary.share.hint")}</span></div>
                  <button type="button" className="switch" role="switch" aria-checked={snapshot.allow_secondary_read} aria-label={t("sync.primary.share.title")} disabled={commandBusy} onClick={() => void run({ action: "set_primary_data_sharing", enabled: !snapshot.allow_secondary_read })} />
                </div>
              ) : null}
              {isSecondary && awaitingSecondaryApproval ? <div className="sync-waiting">{t("sync.pairing.waiting_primary")}</div> : null}
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
                      <span className={`sync-dev-dot${secondaryRecovering ? " is-paused" : ""}`} aria-hidden="true" />
                      <span className="sync-dev-name">{t("sync.role.primary")}<code>{snapshot.endpoint ?? snapshot.primary_id}</code></span>
                      <code>{secondaryRecovering ? t("sync.state.recovering") : t("sync.state.running")}</code>
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

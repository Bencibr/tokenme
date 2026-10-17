import { useCallback, useEffect, useRef, useState } from "react";
import type { DiagnosticsConfig, DiagnosticsManagementRequest, DiagnosticsResponse, DiagnosticsScope, DiagnosticsSessionView, DiscoveredPrimaryView, McpClientConfig, SyncDevicesSnapshot, SyncManagementRequest } from "../types";
import { bridge } from "../lib/bridge";
import { t } from "../lib/i18n";
import { IconBack, IconCheck, IconClose, IconMonitorDown, IconRadioTower, IconWarn } from "./Icons";

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

type Screen = "setup" | "discovery" | "confirm" | "done" | "overview" | "diagnostics";

type PendingConfirmation = {
  message: string;
  request: SyncManagementRequest;
};

type DiagnosticsTtl = "5m" | "15m" | "1h";

const DIAGNOSTICS_SCOPES: DiagnosticsScope[] = ["diagnostics.read", "sync.control"];
const DIAGNOSTICS_TTLS: DiagnosticsTtl[] = ["5m", "15m", "1h"];
const DIAGNOSTICS_MAX_POLLS = 12;
const DIAGNOSTICS_POLL_INITIAL_DELAY_MS = 250;
const DIAGNOSTICS_POLL_MAX_DELAY_MS = 4000;

export function diagnosticsPollDelayMs(attempt: number): number {
  return Math.min(DIAGNOSTICS_POLL_MAX_DELAY_MS, DIAGNOSTICS_POLL_INITIAL_DELAY_MS * 2 ** Math.max(0, attempt));
}

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

function timeLeft(ms: number): string {
  const minutes = Math.max(0, Math.ceil((ms - Date.now()) / 60000));
  if (minutes < 60) return t("sync.rel.left_min", { n: minutes });
  return t("sync.rel.left_hour", { n: Math.floor(minutes / 60) });
}

function isSecondaryAuthorizationError(value: string | null | undefined): boolean {
  return value?.includes("HTTP status 401: device is not authorized") === true;
}

function presentSyncError(value: string | null | undefined): string | null {
  if (!value) return null;
  if (value.includes("pairing request is awaiting approval")) return t("sync.pairing.waiting_primary");
  if (value.includes("pairing request was rejected")) return t("sync.error.pairing_rejected");
  if (value.includes("pairing invite expired")) return t("sync.error.pairing_expired");
  if (value.includes("primary paused this device")) return t("sync.error.primary_paused");
  if (value.includes("device is revoked")) return t("sync.error.device_revoked");
  return isSecondaryAuthorizationError(value) ? t("sync.error.secondary_unauthorized") : value;
}

export function SyncSheet({ snapshot, onSnapshot, onClose }: { snapshot: SyncDevicesSnapshot; onSnapshot: (next: SyncDevicesSnapshot) => void; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [initializing, setInitializing] = useState(true);
  const [discovering, setDiscovering] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [screen, setScreen] = useState<Screen>(() => (snapshot.role === "unconfigured" ? "setup" : "overview"));
  const [pickedRole, setPickedRole] = useState<"primary" | "secondary" | null>(null);
  const [confirmTarget, setConfirmTarget] = useState<DiscoveredPrimaryView | null>(null);
  const [manualInvite, setManualInvite] = useState("");
  const [inviteCopied, setInviteCopied] = useState(false);
  const [pollGeneration, setPollGeneration] = useState(0);
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirmation | null>(null);
  const [diagnosticsEnabled, setDiagnosticsEnabled] = useState(false);
  const [diagnosticsScopes, setDiagnosticsScopes] = useState<DiagnosticsScope[]>(DIAGNOSTICS_SCOPES);
  const [diagnosticsTtl, setDiagnosticsTtl] = useState<DiagnosticsTtl>("15m");
  const [diagnosticsTarget, setDiagnosticsTarget] = useState("");
  const [diagnosticsError, setDiagnosticsError] = useState<string | null>(null);
  const [diagnosticsSessions, setDiagnosticsSessions] = useState<DiagnosticsSessionView[]>([]);
  const [diagnosticsConfig, setDiagnosticsConfig] = useState<DiagnosticsConfig | null>(null);
  const [diagnosticsCopied, setDiagnosticsCopied] = useState(false);
  const [lanControl, setLanControl] = useState(false);
  const [localCred, setLocalCred] = useState<McpClientConfig | null>(null);
  const [localCopied, setLocalCopied] = useState(false);
  const diagnosticsConfigRef = useRef<DiagnosticsConfig | null>(null);
  // The create flow chains configure → create_grant; a state read here would
  // still be the stale closure from before configure answered, so guard on a ref.
  const diagnosticsEnabledRef = useRef(false);
  const discoveryInFlight = useRef(false);
  const manualDiscoveryRequested = useRef(false);
  const diagnosticsPollTimer = useRef<number | undefined>();
  const diagnosticsPollGeneration = useRef(0);
  const operationGeneration = useRef(0);
  const operationInFlight = useRef<"initial" | "mutation" | "discover" | "poll" | null>(null);
  const mounted = useRef(false);

  const clearDiagnosticsPoll = useCallback(() => {
    diagnosticsPollGeneration.current += 1;
    if (diagnosticsPollTimer.current !== undefined) {
      window.clearTimeout(diagnosticsPollTimer.current);
      diagnosticsPollTimer.current = undefined;
    }
  }, []);

  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; clearDiagnosticsPoll(); };
  }, [clearDiagnosticsPoll]);

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
    const interruptsRead = request.action === "stop" || request.action === "unpair";
    if (!mounted.current || (operationInFlight.current && operationInFlight.current !== "poll"
      && (!interruptsRead || operationInFlight.current === "mutation"))) return;
    setPendingConfirm(null);
    clearDiagnosticsPoll();
    const generation = ++operationGeneration.current;
    operationInFlight.current = "mutation";
    setBusy(true);
    if (interruptsRead) { setInitializing(false); setDiscovering(false); }
    setError(null);
    setPollGeneration((value) => value + 1);
    try {
      const next = await bridge.syncDevices(request);
      if (!mounted.current || generation !== operationGeneration.current) return;
      onSnapshot(next);
      onSuccess?.(next);
      // The command already returns its post-write snapshot. A second status
      // call can re-enter recovery and overwrite an intentional stop.
      if (interruptsRead) { setPickedRole(null); setConfirmTarget(null); setScreen("setup"); }
    } catch (e) {
      if (mounted.current && generation === operationGeneration.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (operationInFlight.current === "mutation" && generation === operationGeneration.current) operationInFlight.current = null;
      if (mounted.current && generation === operationGeneration.current) setBusy(false);
    }
  };

  const discover = useCallback(async () => {
    if (!mounted.current || operationInFlight.current) return;
    clearDiagnosticsPoll();
    manualDiscoveryRequested.current = true;
    discoveryInFlight.current = true;
    const generation = ++operationGeneration.current;
    operationInFlight.current = "discover";
    setDiscovering(true);
    setError(null);
    try {
      const next = await bridge.syncDevices({ action: "discover" });
      if (mounted.current && generation === operationGeneration.current) onSnapshot(next);
    } catch (e) {
      if (mounted.current && generation === operationGeneration.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      discoveryInFlight.current = false;
      if (operationInFlight.current === "discover" && generation === operationGeneration.current) operationInFlight.current = null;
      if (mounted.current) setDiscovering(false);
    }
  }, [clearDiagnosticsPoll, onSnapshot]);

  useEffect(() => {
    let alive = true;
    // Defer even the local status read by one task so the newly mounted sheet
    // gets a paint before any synchronous Tauri command work starts.
    const statusTimer = window.setTimeout(() => {
      if (!alive || operationInFlight.current) return;
      const generation = ++operationGeneration.current;
      operationInFlight.current = "initial";
      const read = async (attempt: number): Promise<void> => {
        try {
          const next = await bridge.syncDevices();
          if (!alive || generation !== operationGeneration.current) return;
          onSnapshot(next);
          // The engine may still be creating/migrating the index when the
          // WebView asks for its first status. Give persisted sync state a
          // short bounded window to appear before showing role setup.
          if (next.role === "unconfigured" && !next.pairing_pending && attempt < 8) {
            await new Promise<void>((resolve) => { window.setTimeout(resolve, 250); });
            if (alive && generation === operationGeneration.current) return await read(attempt + 1);
          }
        } catch (e) {
          if (alive && generation === operationGeneration.current) setError(e instanceof Error ? e.message : String(e));
        } finally {
          if (operationInFlight.current === "initial" && generation === operationGeneration.current) operationInFlight.current = null;
          if (alive && generation === operationGeneration.current) setInitializing(false);
        }
      };
      void read(0);
    }, 0);
    return () => { alive = false; window.clearTimeout(statusTimer); };
  }, [onSnapshot]);

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

  // A stop (or any external unconfigure) lands back on role selection; a role
  // that appears without our own wizard action means the sheet was opened on a
  // stale snapshot and should show the live overview.
  const previousRole = useRef(snapshot.role);
  useEffect(() => {
    const was = previousRole.current;
    previousRole.current = snapshot.role;
    if (snapshot.role === "unconfigured" && was !== "unconfigured") {
      manualDiscoveryRequested.current = false;
      setPickedRole(null);
      setConfirmTarget(null);
      setError(null);
      setScreen((current) => (current === "diagnostics" ? current : "setup"));
    } else if (snapshot.role !== "unconfigured" && was === "unconfigured" && pickedRole === null) {
      setScreen((current) => (current === "setup" ? "overview" : current));
    }
  }, [pickedRole, snapshot.role]);

  const hasPendingRequest = snapshot.pairing_requests.some((request) => request.status === "pending");
  const awaitingSecondaryApproval = snapshot.role === "secondary" && !snapshot.stream_id
    && (snapshot.service_running || isSecondaryAuthorizationError(snapshot.last_error));
  const primaryDeviceStateMayChange = snapshot.role === "primary" && snapshot.devices.some((device) => device.role === "secondary" && device.status === "paired");
  const shouldPoll = snapshot.pairing_pending || primaryDeviceStateMayChange || awaitingSecondaryApproval
    || (snapshot.service_running && (hasPendingRequest || snapshot.role === "secondary"));
  const commandBusy = busy || initializing || discovering;
  const stopBusy = operationInFlight.current === "mutation";

  // Silent status polling keeps the overview live while a pairing decision or
  // approval is outstanding; nothing renders for it directly.
  useEffect(() => {
    if (!shouldPoll) return;
    let alive = true;
    let attempts = 0;
    let timer: number | undefined;
    const poll = async () => {
      if (!alive) return;
      const maxAttempts = snapshot.pairing_pending || primaryDeviceStateMayChange || (snapshot.role === "secondary" && snapshot.service_running) ? Number.POSITIVE_INFINITY : 12;
      if (attempts >= maxAttempts) return;
      if (operationInFlight.current) {
        timer = window.setTimeout(() => void poll(), 500);
        return;
      }
      attempts += 1;
      clearDiagnosticsPoll();
      let retryDelay = 5000;
      const generation = ++operationGeneration.current;
      operationInFlight.current = "poll";
      try {
        const next = await bridge.syncDevices({ action: "status" });
        if (alive && mounted.current && generation === operationGeneration.current) { onSnapshot(next); setError(null); }
      } catch (e) {
        retryDelay = Math.min(30000, 5000 * 2 ** Math.max(0, attempts - 1));
        if (alive && mounted.current && generation === operationGeneration.current) setError(e instanceof Error ? e.message : String(e));
      } finally {
        if (operationInFlight.current === "poll" && generation === operationGeneration.current) operationInFlight.current = null;
      }
      if (alive && generation === operationGeneration.current && attempts < maxAttempts) timer = window.setTimeout(() => void poll(), retryDelay);
    };
    timer = window.setTimeout(() => void poll(), 5000);
    return () => { alive = false; if (timer !== undefined) window.clearTimeout(timer); };
  }, [clearDiagnosticsPoll, onSnapshot, pollGeneration, primaryDeviceStateMayChange, shouldPoll, snapshot.pairing_pending, snapshot.role, snapshot.service_running]);

  const applyDiagnosticsResponse = useCallback((response: DiagnosticsResponse, generation?: number) => {
    if (generation !== undefined && generation !== operationGeneration.current) return;
    diagnosticsEnabledRef.current = response.enabled;
    setDiagnosticsEnabled(response.enabled);
    setLanControl(response.lan_control_enabled);
    setDiagnosticsSessions(response.sessions);
    setDiagnosticsError(response.error?.message ?? null);
    if (response.config) {
      diagnosticsConfigRef.current = response.config;
      setDiagnosticsConfig(response.config);
      setDiagnosticsCopied(false);
    } else if (!response.enabled || (diagnosticsConfigRef.current && !response.sessions.some((session) => session.session_id === diagnosticsConfigRef.current?.grant.grant_id))) {
      diagnosticsConfigRef.current = null;
      setDiagnosticsConfig(null);
      setDiagnosticsCopied(false);
    }
  }, []);

  const pollDiagnosticsStatus = useCallback((operationId: string, generation: number) => {
    clearDiagnosticsPoll();
    const pollGeneration = diagnosticsPollGeneration.current;
    let attempts = 0;
    const poll = () => {
      if (!mounted.current || generation !== operationGeneration.current || pollGeneration !== diagnosticsPollGeneration.current || attempts >= DIAGNOSTICS_MAX_POLLS) return;
      const delay = diagnosticsPollDelayMs(attempts);
      attempts += 1;
      diagnosticsPollTimer.current = window.setTimeout(() => {
        diagnosticsPollTimer.current = undefined;
        if (!mounted.current || generation !== operationGeneration.current || pollGeneration !== diagnosticsPollGeneration.current) return;
        void bridge.diagnosticsSession({ action: "status" }).then((response) => {
          if (!mounted.current || generation !== operationGeneration.current || pollGeneration !== diagnosticsPollGeneration.current) return;
          applyDiagnosticsResponse(response, generation);
          const operation = response.last_operation;
          if (operation?.state !== "running") return;
          if (operation.operation_id !== operationId) {
            pollDiagnosticsStatus(operation.operation_id, generation);
            return;
          }
          poll();
        }).catch((diagError) => {
          if (!mounted.current || generation !== operationGeneration.current || pollGeneration !== diagnosticsPollGeneration.current) return;
          setDiagnosticsError(diagError instanceof Error ? diagError.message : String(diagError));
          poll();
        });
      }, delay);
    };
    poll();
  }, [applyDiagnosticsResponse, clearDiagnosticsPoll]);

  useEffect(() => {
    let alive = true;
    const timer = window.setTimeout(() => {
      const generation = operationGeneration.current;
      void bridge.diagnosticsSession({ action: "status" }).then((response) => {
        if (alive && mounted.current && generation === operationGeneration.current) {
          applyDiagnosticsResponse(response, generation);
          if (response.last_operation?.state === "running") pollDiagnosticsStatus(response.last_operation.operation_id, generation);
        }
      }).catch((diagError) => {
        if (alive && mounted.current) setDiagnosticsError(diagError instanceof Error ? diagError.message : String(diagError));
      });
    }, 0);
    return () => { alive = false; window.clearTimeout(timer); };
  }, [applyDiagnosticsResponse, pollDiagnosticsStatus]);

  const runDiagnostics = useCallback(async (request: DiagnosticsManagementRequest) => {
    if (!mounted.current || operationInFlight.current) return;
    if (request.action !== "configure" && !diagnosticsEnabledRef.current) return;
    clearDiagnosticsPoll();
    const generation = ++operationGeneration.current;
    operationInFlight.current = "mutation";
    setBusy(true);
    setDiagnosticsError(null);
    try {
      const response = await bridge.diagnosticsSession(request);
      if (!mounted.current || generation !== operationGeneration.current) return;
      applyDiagnosticsResponse(response, generation);
      if (response.last_operation?.state === "running") pollDiagnosticsStatus(response.last_operation.operation_id, generation);
    } catch (diagError) {
      if (mounted.current && generation === operationGeneration.current) setDiagnosticsError(diagError instanceof Error ? diagError.message : String(diagError));
    } finally {
      if (operationInFlight.current === "mutation" && generation === operationGeneration.current) operationInFlight.current = null;
      if (mounted.current && generation === operationGeneration.current) setBusy(false);
    }
  }, [applyDiagnosticsResponse, clearDiagnosticsPoll, pollDiagnosticsStatus]);

  const grantTargets = snapshot.devices.filter((device) => device.role === "secondary" && device.status === "paired");
  const grantCode = diagnosticsConfig ? `tm_${diagnosticsConfig.grant.grant_id}` : "";

  const createGrantFlow = async () => {
    const target = grantTargets.find((device) => device.device_id === diagnosticsTarget);
    if (!target || !diagnosticsScopes.length) {
      setDiagnosticsError(t("sync.diagnostics.invalid_config"));
      return;
    }
    // The local diagnostics server must be on before a grant can be signed;
    // the protocol caps every grant at one hour.
    if (!diagnosticsEnabled) await runDiagnostics({ action: "configure", enabled: true });
    if (!mounted.current) return;
    const ttl_ms = diagnosticsTtl === "5m" ? 5 * 60 * 1000 : diagnosticsTtl === "15m" ? 15 * 60 * 1000 : 60 * 60 * 1000;
    await runDiagnostics({ action: "create_grant", debugger_device_id: target.device_id, scopes: diagnosticsScopes, ttl_ms });
  };

  const copyDiagnosticsConfig = async () => {
    if (!diagnosticsConfig) return;
    const ok = await copyText(JSON.stringify(diagnosticsConfig, null, 2));
    if (ok) { setDiagnosticsCopied(true); window.setTimeout(() => setDiagnosticsCopied(false), 1600); }
  };

  // Local debug clients consume the MCP connection config, not a signed grant:
  // the main MCP switch owns its lifetime and the scope choice only changes
  // LAN-control powers.
  const createLocalFlow = async () => {
    if (!diagnosticsScopes.length) { setDiagnosticsError(t("sync.diagnostics.invalid_config")); return; }
    await runDiagnostics({ action: "configure", enabled: true });
    if (!mounted.current) return;
    const control = diagnosticsScopes.includes("sync.control");
    if (control !== lanControl) {
      await runDiagnostics({ action: "configure_lan_control", enabled: control });
      if (!mounted.current) return;
    }
    try {
      const cfg = await bridge.mcpClientConfig();
      if (!mounted.current) return;
      if (!cfg.ready) { setDiagnosticsError(cfg.reason ?? t("sync.diagnostics.invalid_config")); return; }
      setLocalCred(cfg);
      setDiagnosticsError(null);
    } catch (e) {
      if (mounted.current) setDiagnosticsError(e instanceof Error ? e.message : String(e));
    }
  };

  const revokeLocalFlow = () => {
    setLocalCred(null);
    setLocalCopied(false);
    void runDiagnostics({ action: "configure", enabled: false });
  };

  const copyLocalFlow = async () => {
    if (!localCred) return;
    const ok = await copyText(localCred.config_json);
    if (ok) { setLocalCopied(true); window.setTimeout(() => setLocalCopied(false), 1600); }
  };

  const toggleDiagnosticsScope = (scope: DiagnosticsScope) => {
    if (diagnosticsScopes.includes(scope)) {
      setDiagnosticsScopes((current) => current.filter((value) => value !== scope));
    } else {
      setDiagnosticsScopes((current) => [...current, scope]);
    }
    setDiagnosticsError(null);
  };

  const isPrimary = snapshot.role === "primary";
  const isSecondary = snapshot.role === "secondary";
  const pendingRequests = snapshot.pairing_requests.filter((request) => request.status === "pending");
  const connectedDevices = snapshot.devices.filter((device) => device.status !== "revoked");
  const primaryDevices = connectedDevices.filter((device) => device.role === "primary");
  const secondaryDevices = connectedDevices.filter((device) => device.role === "secondary");

  // A persisted role stays visible while the transport is recovering; only an
  // explicit stop returns to role selection.
  const effectiveScreen: Screen = screen === "overview" && (snapshot.role === "unconfigured" || snapshot.operation_state === "offline") ? "setup" : screen;
  const rawSyncError = error ?? snapshot.last_error;
  const shownError = presentSyncError(rawSyncError);
  // A stale secondary 401 explains why the previous overview could not sync;
  // once the user is back at role setup it must not sit above a new pairing
  // choice or make the old connection look like a fresh failure.
  const showSyncError = shownError && !(effectiveScreen === "setup" && isSecondaryAuthorizationError(rawSyncError))
    ? shownError
    : null;

  const nextFromSetup = () => {
    if (pickedRole === "primary") {
      // 0.0.0.0 with an auto-bound port; no network questions — the wizard
      // promises the defaults, the backend owns the fallbacks.
      void run({ action: "start_primary", bind: "0.0.0.0:47821", advertise_host: null, allow_lan: true }, () => setScreen("overview"));
    } else if (pickedRole === "secondary") {
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
      setScreen("done");
    });
  };

  const createInvite = () => {
    setInviteCopied(false);
    void run({ action: "create_invite" });
  };

  const copyInvite = async () => {
    if (!snapshot.invite_url) return;
    if (await copyText(snapshot.invite_url)) {
      setInviteCopied(true);
      window.setTimeout(() => setInviteCopied(false), 1600);
    }
  };

  const deviceRow = (device: (typeof snapshot.devices)[number]) => (
    <div className="sync-dev" key={device.device_id}>
      <span className={`sync-dev-dot${device.status === "paused" ? " is-paused" : ""}`} aria-hidden="true" />
      <span className="sync-dev-name">{device.display_name}</span>
      {device.status === "pending" ? <code>{t("sync.device.pending")}</code> : device.status === "paused" ? <code>{t("sync.device.paused")}</code> : device.status === "reconcile_required" ? <code>{t("sync.device.reconcile")}</code> : null}
      {isPrimary && (device.status === "paired" || device.status === "paused") ? <button type="button" className="sync-disc" disabled={commandBusy} onClick={() => void run({ action: device.status === "paused" ? "resume" : "pause", device_id: device.device_id })}>{device.status === "paused" ? t("sync.resume") : t("sync.pause")}</button> : null}
      <button type="button" className="sync-disc" disabled={commandBusy} onClick={() => setPendingConfirm({ request: isSecondary ? { action: "unpair" } : { action: "revoke", device_id: device.device_id }, message: t("sync.revoke.confirm", { name: device.display_name }) })}>{t("sync.disconnect")}</button>
    </div>
  );

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
          {showSyncError ? <div className="sheet-notice" role="alert"><IconWarn size={12} />{showSyncError}</div> : null}
          {snapshot.discovery_error ? <div className="sheet-notice" role="status"><IconWarn size={12} />{t("sync.discovery.permission_error", { message: snapshot.discovery_error })}</div> : null}
          {diagnosticsError ? <div className="sheet-notice" role="alert"><IconWarn size={12} />{diagnosticsError}</div> : null}
          {pendingConfirm ? <div className="sync-confirm" role="alertdialog" aria-live="assertive" aria-label={t("sync.confirm.title")}>
            <div className="sync-confirm-copy"><strong>{t("sync.confirm.title")}</strong><span>{pendingConfirm.message}</span></div>
            <div className="sync-actions"><button type="button" className="btn-accent" disabled={commandBusy} onClick={() => { const { request } = pendingConfirm; setPendingConfirm(null); void run(request); }}>{t("sync.confirm.accept")}</button><button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => setPendingConfirm(null)}>{t("sync.confirm.cancel")}</button></div>
          </div> : null}

          {initializing && snapshot.role === "unconfigured" ? (
            <div className="sync-scan" role="status">
              <div className="sync-dualring" aria-hidden="true"><i /><i /></div>
              <div>{t("sync.restore.loading")}</div>
            </div>
          ) : null}

          {!initializing && effectiveScreen === "setup" ? (
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

          {effectiveScreen === "discovery" ? (
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
                    <button type="button" className="sync-back" disabled={commandBusy} onClick={() => void discover()}>{t("sync.discovery.rescan")}</button>
                    <span>{t("sync.discovery.or")}</span>
                    <button type="button" className="sync-back" onClick={() => setScreen("diagnostics")}>{t("sync.discovery.diag_link")}</button>
                    <span>{t("sync.discovery.diag_tail")}</span>
                  </div>
                  <div className="sync-manual-panel sync-credbox">
                    <div className="sync-diagnostics-label"><span>{t("sync.discovery.manual.title")}</span></div>
                    <p className="explain">{t("sync.discovery.manual.explain")}</p>
                    <textarea className="sync-invite" value={manualInvite} onChange={(event) => setManualInvite(event.target.value)} placeholder={t("sync.discovery.manual.placeholder")} aria-label={t("sync.discovery.manual.title")} />
                    <div className="sync-btnrow">
                      <button type="button" className="btn-accent" disabled={commandBusy || !manualInvite.trim()} onClick={pairWithInvite}>{t("sync.discovery.manual.go")}</button>
                    </div>
                  </div>
                </>
              ) : null}
            </>
          ) : null}

          {effectiveScreen === "confirm" && confirmTarget ? (
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

          {effectiveScreen === "done" ? (
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

          {effectiveScreen === "overview" && snapshot.role !== "unconfigured" ? (
            <>
              <div className="rolecard">
                <div className="role-row">
                  <div>
                    <span className="role-badge">{isPrimary ? t("sync.role.primary") : t("sync.role.secondary")}</span>
                    <span className="role-name">{t("sync.local_device")}</span>
                    <div className="meta">
                      {isPrimary
                        ? <>{snapshot.endpoint ? <span className="num">{snapshot.endpoint}</span> : null}{snapshot.endpoint ? " · " : null}{t("sync.ov.awaiting")}</>
                        : <>{t("sync.ov.meta_sec", { n: primaryDevices.length })}{snapshot.last_connected_at_ms ? <> · {t("sync.last_sync", { time: relTime(snapshot.last_connected_at_ms) })}</> : null}</>}
                    </div>
                  </div>
                  <div className="role-right">
                    <span className="role-state">{snapshot.service_running ? t("sync.state.running") : t("sync.state.recovering")}</span>
                    <button type="button" className="stopbtn" disabled={stopBusy} onClick={() => void run({ action: "stop" })} aria-label={t("sync.stop_disconnect")}>
                      <span className="stopicon" aria-hidden="true" />{t("sync.stop_disconnect")}
                    </button>
                  </div>
                </div>
              </div>
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
              {isPrimary && secondaryDevices.length ? (
                <>
                  <div className="sync-sect2">{t("sync.ov.paired_sect")}</div>
                  {secondaryDevices.map((device) => deviceRow(device))}
                </>
              ) : null}
              {isSecondary && primaryDevices.length ? (
                <>
                  <div className="sync-sect2">{t("sync.ov.paired_sect_sec")}</div>
                  {primaryDevices.map((device) => deviceRow(device))}
                </>
              ) : null}
              {isPrimary ? (
                <div className="sync-manual-panel sync-credbox">
                  <div className="sync-diagnostics-label"><span>{t("sync.invite.title")}</span></div>
                  <p className="explain">{t("sync.invite.explain")}</p>
                  {snapshot.invite_url ? <textarea className="sync-invite" readOnly value={snapshot.invite_url} aria-label={t("sync.invite.title")} onFocus={(event) => event.currentTarget.select()} /> : null}
                  <div className="sync-btnrow">
                    <button type="button" className="btn-accent" disabled={commandBusy} onClick={createInvite}>{t("sync.invite.create")}</button>
                    {snapshot.invite_url ? <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void copyInvite()}>{inviteCopied ? t("sync.invite.copied") : t("sync.invite.copy")}</button> : null}
                  </div>
                </div>
              ) : null}
            </>
          ) : null}

          {effectiveScreen === "diagnostics" ? (
            <>
              <div className="sync-crumb">
                <button type="button" className="sync-back" aria-label={t("sync.discovery.back")} onClick={() => setScreen("discovery")}><IconBack size={12} /></button>
                <b>{t("sync.diagnostics.title")}</b>
              </div>
              <p className="explain">{t("sync.diagnostics.explain")}</p>
              {grantTargets.length ? (
                <>
                  <div className="sync-diagnostics-label"><span>{t("sync.diagnostics.ttl")}</span></div>
                  <div className="seg seg-wide" role="radiogroup" aria-label={t("sync.diagnostics.ttl")}>
                    {DIAGNOSTICS_TTLS.map((ttl) => <button type="button" className="seg-btn" key={ttl} role="radio" aria-checked={diagnosticsTtl === ttl} onClick={() => setDiagnosticsTtl(ttl)}>{ttl === "5m" ? t("sync.diagnostics.ttl.5m") : ttl === "15m" ? t("sync.diagnostics.ttl.15m") : t("sync.diagnostics.ttl.60m")}</button>)}
                  </div>
                </>
              ) : null}
              <div className="sync-diagnostics-label"><span>{t("sync.diagnostics.scope")}</span></div>
              <div className="scopes" role="group" aria-label={t("sync.diagnostics.scope")}>
                {DIAGNOSTICS_SCOPES.map((scope) => <button type="button" key={scope} aria-pressed={diagnosticsScopes.includes(scope)} onClick={() => toggleDiagnosticsScope(scope)}>{scope === "diagnostics.read" ? t("sync.diagnostics.scope.read") : t("sync.diagnostics.scope.control")}</button>)}
              </div>
              {grantTargets.length ? (
                <>
                  <label className="sync-field"><span>{t("sync.diagnostics.target")}</span><select className="sync-input" value={diagnosticsTarget} onChange={(event) => setDiagnosticsTarget(event.target.value)} aria-label={t("sync.diagnostics.target")}>
                    <option value="">{t("sync.diagnostics.target.choose")}</option>
                    {grantTargets.map((device) => <option key={device.device_id} value={device.device_id}>{device.display_name}</option>)}
                  </select></label>
                  <div className="sync-btnrow">
                    <button type="button" className="btn-accent" disabled={commandBusy || !diagnosticsScopes.length || !diagnosticsTarget || !snapshot.service_running} onClick={() => void createGrantFlow()}>{t("sync.diagnostics.create_short")}</button>
                  </div>
                  {diagnosticsConfig ? (
                    <div className="sync-credbox">
                      <div className="sync-diagnostics-label"><span>{t("sync.diagnostics.generated")}</span></div>
                      <input className="sync-input" readOnly value={grantCode} aria-label={t("sync.diagnostics.generated")} onFocus={(event) => event.currentTarget.select()} />
                      <div className="sync-btnrow">
                        <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void copyDiagnosticsConfig()}>{diagnosticsCopied ? t("sync.diagnostics.copied") : t("sync.diagnostics.copy_code")}</button>
                      </div>
                    </div>
                  ) : null}
                  <div className="sync-sect2">{t("sync.diagnostics.sessions")}</div>
                  {diagnosticsSessions.map((session) => (
                    <div className="sync-dev" key={session.session_id}>
                      <span className="sync-dev-dot" aria-hidden="true" />
                      <span className="sync-dev-name">{session.origin}<code>{session.target_device}</code></span>
                      <span className="sync-dev-when">{timeLeft(session.expires_at_ms)}</span>
                      <button type="button" className="sync-disc" disabled={commandBusy} onClick={() => void runDiagnostics({ action: "revoke_grant", grant_id: session.session_id })}>{t("sync.diagnostics.revoke_session")}</button>
                    </div>
                  ))}
                </>
              ) : (
                <>
                  <div className="sync-btnrow">
                    <button type="button" className="btn-accent" disabled={commandBusy} onClick={() => void createLocalFlow()}>{t("sync.diagnostics.create_local")}</button>
                  </div>
                  {localCred ? (
                    <div className="sync-credbox">
                      <div className="sync-diagnostics-label"><span>{t("sync.diagnostics.generated")}</span></div>
                      <input className="sync-input" readOnly value={localCred.endpoint ?? ""} aria-label={t("sync.diagnostics.generated")} onFocus={(event) => event.currentTarget.select()} />
                      <div className="sheet-hint">
                        {localCred.expires_at_ms ? timeLeft(localCred.expires_at_ms) : t("sync.diagnostics.persistent")}
                      </div>
                      <div className="sync-btnrow">
                        <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void copyLocalFlow()}>{localCopied ? t("sync.diagnostics.copied") : t("sync.diagnostics.copy_code")}</button>
                        <button type="button" className="btn-quiet" disabled={commandBusy} onClick={revokeLocalFlow}>{t("sync.diagnostics.revoke_local")}</button>
                      </div>
                    </div>
                  ) : null}
                </>
              )}
            </>
          ) : null}
        </div>
      </div>
    </div>
  );
}

import { useCallback, useEffect, useRef, useState } from "react";
import jsQR from "jsqr";
import QRCode from "qrcode";
import type { DiagnosticsConfig, DiagnosticsManagementRequest, DiagnosticsOperationView, DiagnosticsResponse, DiagnosticsScope, DiagnosticsSessionView, SyncDevicesSnapshot, SyncManagementRequest } from "../types";
import { bridge } from "../lib/bridge";
import { t } from "../lib/i18n";
import { IconClose, IconRefresh, IconWarn } from "./Icons";
import { Loading } from "./Loading";

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

async function decodeInviteFile(file: File): Promise<string> {
  if (!new Set(["image/png", "image/jpeg", "image/webp"]).has(file.type)) throw new Error(t("sync.qr.invalid"));
  if (typeof createImageBitmap !== "function") throw new Error(t("sync.qr.invalid"));
  const bitmap = await createImageBitmap(file);
  try {
    const scale = Math.min(1, 2400 / Math.max(bitmap.width, bitmap.height));
    const width = Math.max(1, Math.round(bitmap.width * scale));
    const height = Math.max(1, Math.round(bitmap.height * scale));
    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    const context = canvas.getContext("2d", { willReadFrequently: true });
    if (!context) throw new Error(t("sync.qr.invalid"));
    context.drawImage(bitmap, 0, 0, width, height);
    const pixels = context.getImageData(0, 0, width, height);
    const result = jsQR(pixels.data, width, height, { inversionAttempts: "attemptBoth" });
    if (!result?.data.startsWith("tokenme://device-pair?")) throw new Error(t("sync.qr.invalid"));
    return result.data;
  } finally {
    bitmap.close();
  }
}

type PendingConfirmation = {
  message: string;
  request: SyncManagementRequest;
  onSuccess?: (next: SyncDevicesSnapshot) => void;
};

type DiagnosticsTtl = "15m" | "1h" | "session";
type DiagnosticsAction = "configure" | "create_grant" | "revoke_all" | "revoke_grant";

const DIAGNOSTICS_SCOPES: DiagnosticsScope[] = ["diagnostics.read", "sync.control"];
const DIAGNOSTICS_TTLS: DiagnosticsTtl[] = ["15m", "1h", "session"];

function diagnosticsActionLabel(action: DiagnosticsAction): string {
  switch (action) {
    case "configure": return t("sync.diagnostics.operation.configure");
    case "create_grant": return t("sync.diagnostics.operation.create_session");
    case "revoke_all": return t("sync.diagnostics.operation.revoke_all");
    case "revoke_grant": return t("sync.diagnostics.operation.revoke_session");
  }
}

function diagnosticsOperationStateLabel(state: DiagnosticsOperationView["state"]): string {
  switch (state) {
    case "running": return t("sync.diagnostics.operation.state.running");
    case "completed": return t("sync.diagnostics.operation.state.completed");
    case "failed": return t("sync.diagnostics.operation.state.failed");
  }
}

function bindForLan(value: string): string {
  const match = value.trim().match(/^(?:127\.0\.0\.1|\[?::1\]?):(\d+)$/);
  return match ? `0.0.0.0:${match[1]}` : value;
}

export function SyncSheet({ snapshot, onSnapshot, onClose }: { snapshot: SyncDevicesSnapshot; onSnapshot: (next: SyncDevicesSnapshot) => void; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [initializing, setInitializing] = useState(true);
  const [discovering, setDiscovering] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [phase, setPhase] = useState<SyncDevicesSnapshot["operation_state"]>(snapshot.operation_state);
  const [pollStopped, setPollStopped] = useState(false);
  const [pollGeneration, setPollGeneration] = useState(0);
  const [bind, setBind] = useState("0.0.0.0:47821");
  const [advertiseHost, setAdvertiseHost] = useState("");
  const [allowLan, setAllowLan] = useState(true);
  const [invite, setInvite] = useState("");
  const [origin, setOrigin] = useState("secondary");
  const [displayName, setDisplayName] = useState("");
  const [setupMode, setSetupMode] = useState<"discovery" | "manual" | "primary">("discovery");
  const [copied, setCopied] = useState(false);
  const [qrDataUrl, setQrDataUrl] = useState<string | null>(null);
  const [qrBusy, setQrBusy] = useState(false);
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirmation | null>(null);
  const [diagnosticsEnabled, setDiagnosticsEnabled] = useState(false);
  const [diagnosticsScopes, setDiagnosticsScopes] = useState<DiagnosticsScope[]>(DIAGNOSTICS_SCOPES);
  const [diagnosticsTtl, setDiagnosticsTtl] = useState<DiagnosticsTtl>("15m");
  const [diagnosticsTarget, setDiagnosticsTarget] = useState("");
  const [diagnosticsError, setDiagnosticsError] = useState<string | null>(null);
  const [diagnosticsOperation, setDiagnosticsOperation] = useState<DiagnosticsOperationView | null>(null);
  const [diagnosticsSessions, setDiagnosticsSessions] = useState<DiagnosticsSessionView[]>([]);
  const [diagnosticsConfig, setDiagnosticsConfig] = useState<DiagnosticsConfig | null>(null);
  const [diagnosticsQrDataUrl, setDiagnosticsQrDataUrl] = useState<string | null>(null);
  const [diagnosticsConfigUrl, setDiagnosticsConfigUrl] = useState<string | null>(null);
  const [diagnosticsCopied, setDiagnosticsCopied] = useState(false);
  const [diagnosticsRevokeConfirm, setDiagnosticsRevokeConfirm] = useState(false);
  const diagnosticsConfigRef = useRef<DiagnosticsConfig | null>(null);
  const qrFile = useRef<HTMLInputElement>(null);
  const discoveryInFlight = useRef(false);
  const automaticDiscoveryStarted = useRef(false);
  const manualDiscoveryRequested = useRef(false);
  const automaticDiscoveryTimer = useRef<number | undefined>();
  const operationGeneration = useRef(0);
  const operationInFlight = useRef<"initial" | "mutation" | "refresh" | "discover" | "poll" | null>(null);
  const mounted = useRef(false);

  const clearAutomaticDiscovery = useCallback(() => {
    if (automaticDiscoveryTimer.current !== undefined) {
      window.clearTimeout(automaticDiscoveryTimer.current);
      automaticDiscoveryTimer.current = undefined;
    }
  }, []);

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
    clearAutomaticDiscovery();
    manualDiscoveryRequested.current = true;
    const generation = ++operationGeneration.current;
    operationInFlight.current = "mutation";
    setBusy(true);
    setPhase("running");
    setError(null);
    setPollGeneration((value) => value + 1);
    try {
      const next = await bridge.syncDevices(request);
      if (!mounted.current || generation !== operationGeneration.current) return;
      onSnapshot(next);
      setPhase(next.operation_state);
      onSuccess?.(next);
      // Mutations return a snapshot, but take one explicit status read after
      // the write so stale pairing/device rows cannot remain on screen.
      const latest = await bridge.syncDevices({ action: "status" });
      if (mounted.current && generation === operationGeneration.current) {
        onSnapshot(latest);
        setPhase(latest.operation_state);
      }
    } catch (e) {
      if (mounted.current && generation === operationGeneration.current) {
        setError(e instanceof Error ? e.message : String(e));
        setPhase("error");
      }
    } finally {
      if (operationInFlight.current === "mutation" && generation === operationGeneration.current) operationInFlight.current = null;
      if (mounted.current && generation === operationGeneration.current) setBusy(false);
    }
  };

  const refresh = async () => {
    if (!mounted.current || operationInFlight.current) return;
    clearAutomaticDiscovery();
    const generation = ++operationGeneration.current;
    operationInFlight.current = "refresh";
    setBusy(true);
    setPhase("running");
    setError(null);
    setPollGeneration((value) => value + 1);
    try {
      const next = await bridge.syncDevices({ action: "status" });
      if (mounted.current && generation === operationGeneration.current) {
        onSnapshot(next);
        setPhase(next.operation_state);
      }
    } catch (e) {
      if (mounted.current && generation === operationGeneration.current) {
        setError(e instanceof Error ? e.message : String(e));
        setPhase("offline");
      }
    } finally {
      if (operationInFlight.current === "refresh" && generation === operationGeneration.current) operationInFlight.current = null;
      if (mounted.current && generation === operationGeneration.current) setBusy(false);
    }
  };

  const discover = useCallback(async (automatic = false) => {
    if (!mounted.current) return;
    if (operationInFlight.current) return;
    if (automatic && (automaticDiscoveryStarted.current || manualDiscoveryRequested.current)) return;
    clearAutomaticDiscovery();
    if (automatic) automaticDiscoveryStarted.current = true;
    else manualDiscoveryRequested.current = true;
    discoveryInFlight.current = true;
    const generation = ++operationGeneration.current;
    operationInFlight.current = "discover";
    setDiscovering(true);
    setPhase("running");
    setError(null);
    try {
      const next = await bridge.syncDevices({ action: "discover" });
      if (mounted.current && generation === operationGeneration.current) {
        onSnapshot(next);
        setPhase(next.operation_state);
      }
    } catch (e) {
      if (mounted.current && generation === operationGeneration.current) {
        setError(e instanceof Error ? e.message : String(e));
        setPhase("offline");
      }
    } finally {
      discoveryInFlight.current = false;
      if (operationInFlight.current === "discover" && generation === operationGeneration.current) operationInFlight.current = null;
      if (mounted.current) setDiscovering(false);
    }
  }, [clearAutomaticDiscovery, onSnapshot]);

  const scheduleAutomaticDiscovery = useCallback(() => {
    clearAutomaticDiscovery();
    const attempt = () => {
      if (!mounted.current) return;
      if (operationInFlight.current) {
        automaticDiscoveryTimer.current = window.setTimeout(attempt, 50);
        return;
      }
      automaticDiscoveryTimer.current = undefined;
      void discover(true);
    };
    automaticDiscoveryTimer.current = window.setTimeout(attempt, 0);
  }, [clearAutomaticDiscovery, discover]);

  useEffect(() => {
    let alive = true;
    // Defer even the local status read by one task so the newly mounted sheet
    // gets a paint before any synchronous Tauri command work starts.
    const statusTimer = window.setTimeout(() => {
      if (!alive || operationInFlight.current) return;
      const generation = ++operationGeneration.current;
      operationInFlight.current = "initial";
      void bridge.syncDevices().then(async (next) => {
        if (!alive || generation !== operationGeneration.current) return;
        onSnapshot(next);
        if (!discoveryInFlight.current) setPhase(next.operation_state);
        if (next.role === "unconfigured" || (next.role === "primary" && !next.service_running)) {
          // Let the sheet paint and leave its manual paths usable before the
          // bounded mDNS browse begins. The ref guards the initial status race
          // and React StrictMode's development-only effect replay.
          scheduleAutomaticDiscovery();
        }
      }).catch((e) => {
        if (alive && generation === operationGeneration.current) { setError(e instanceof Error ? e.message : String(e)); setPhase("offline"); }
      }).finally(() => {
        if (operationInFlight.current === "initial" && generation === operationGeneration.current) operationInFlight.current = null;
        if (alive && generation === operationGeneration.current) setInitializing(false);
      });
    }, 0);
    return () => { alive = false; window.clearTimeout(statusTimer); clearAutomaticDiscovery(); };
  }, [clearAutomaticDiscovery, onSnapshot, scheduleAutomaticDiscovery]);

  const isSecondary = snapshot.role === "secondary";
  const isPrimary = snapshot.role === "primary";
  const isUnconfigured = snapshot.role === "unconfigured";
  const pairedDiagnosticsDevices = snapshot.devices.filter((device) => device.role === "secondary" && device.status === "paired");
  const diagnosticsReady = diagnosticsEnabled;
  const remoteDiagnosticsReady = isPrimary && snapshot.service_running && diagnosticsEnabled;
  const hasPendingRequest = snapshot.pairing_requests.some((request) => request.status === "pending");
  const awaitingSecondaryApproval = isSecondary && snapshot.service_running && !snapshot.stream_id;
  const shouldPoll = snapshot.service_running && (hasPendingRequest || awaitingSecondaryApproval);
  const commandBusy = busy || initializing || discovering;

  const applyDiagnosticsResponse = useCallback((response: DiagnosticsResponse, generation?: number) => {
    if (generation !== undefined && generation !== operationGeneration.current) return;
    setDiagnosticsEnabled(response.enabled);
    setDiagnosticsSessions(response.sessions);
    setDiagnosticsOperation(response.last_operation);
    setDiagnosticsError(response.error?.message ?? null);
    if (response.config) {
      diagnosticsConfigRef.current = response.config;
      setDiagnosticsConfig(response.config);
      const serialized = JSON.stringify(response.config, null, 2);
      setDiagnosticsConfigUrl(`data:application/json;charset=utf-8,${encodeURIComponent(serialized)}`);
      setDiagnosticsCopied(false);
      void QRCode.toDataURL(serialized, { errorCorrectionLevel: "M", margin: 2, width: 240 })
        .then((value) => { if (mounted.current && (generation === undefined || generation === operationGeneration.current)) setDiagnosticsQrDataUrl(value); })
        .catch((error) => { if (mounted.current && (generation === undefined || generation === operationGeneration.current)) setDiagnosticsError(error instanceof Error ? error.message : String(error)); });
    } else if (!response.enabled || (diagnosticsConfigRef.current && !response.sessions.some((session) => session.session_id === diagnosticsConfigRef.current?.grant.grant_id))) {
      diagnosticsConfigRef.current = null;
      setDiagnosticsConfig(null);
      setDiagnosticsConfigUrl(null);
      setDiagnosticsQrDataUrl(null);
      setDiagnosticsCopied(false);
    }
  }, []);

  useEffect(() => {
    let alive = true;
    const timer = window.setTimeout(() => {
      const generation = operationGeneration.current;
      void bridge.diagnosticsSession({ action: "status" }).then((response) => {
        if (alive && mounted.current && generation === operationGeneration.current && !operationInFlight.current) applyDiagnosticsResponse(response, generation);
      }).catch((error) => {
        if (alive && mounted.current) setDiagnosticsError(error instanceof Error ? error.message : String(error));
      });
    }, 0);
    return () => { alive = false; window.clearTimeout(timer); };
  }, [applyDiagnosticsResponse]);

  const runDiagnostics = useCallback(async (request: DiagnosticsManagementRequest) => {
    if (!mounted.current || operationInFlight.current) return;
    if (request.action !== "configure" && !diagnosticsEnabled) return;
    const generation = ++operationGeneration.current;
    operationInFlight.current = "mutation";
    setBusy(true);
    setDiagnosticsError(null);
    setDiagnosticsRevokeConfirm(false);
    try {
      const response = await bridge.diagnosticsSession(request);
      if (!mounted.current || generation !== operationGeneration.current) return;
      applyDiagnosticsResponse(response);
    } catch (error) {
      if (mounted.current && generation === operationGeneration.current) setDiagnosticsError(error instanceof Error ? error.message : String(error));
    } finally {
      if (operationInFlight.current === "mutation" && generation === operationGeneration.current) operationInFlight.current = null;
      if (mounted.current && generation === operationGeneration.current) setBusy(false);
    }
  }, [applyDiagnosticsResponse, diagnosticsEnabled]);

  const toggleDiagnostics = () => {
    if (commandBusy) return;
    void runDiagnostics({ action: "configure", enabled: !diagnosticsEnabled });
  };

  const createDiagnosticsSession = () => {
    const targetDevice = snapshot.devices.find((device) => device.device_id === diagnosticsTarget && device.role === "secondary" && device.status === "paired");
    if (!targetDevice || !diagnosticsScopes.length) {
      setDiagnosticsError(t("sync.diagnostics.invalid_config"));
      return;
    }
    // The protocol caps every grant at one hour; a "session" grant is still
    // process-scoped and is invalidated on primary restart, so use that cap.
    const ttl_ms = diagnosticsTtl === "15m" ? 15 * 60 * 1000 : 60 * 60 * 1000;
    void runDiagnostics({ action: "create_grant", debugger_device_id: targetDevice.device_id, scopes: diagnosticsScopes, ttl_ms });
  };

  const copyDiagnosticsConfig = async () => {
    if (!diagnosticsConfig) return;
    const ok = await copyText(JSON.stringify(diagnosticsConfig, null, 2));
    if (ok) { setDiagnosticsCopied(true); window.setTimeout(() => setDiagnosticsCopied(false), 1600); }
  };

  const toggleDiagnosticsScope = (scope: DiagnosticsScope) => {
    if (diagnosticsScopes.includes(scope)) {
      setDiagnosticsScopes((current) => current.filter((value) => value !== scope));
    } else {
      setDiagnosticsScopes((current) => [...current, scope]);
    }
    setDiagnosticsError(null);
  };

  const clearDiagnosticsErrors = () => {
    setDiagnosticsError(null);
  };

  const previousRole = useRef(snapshot.role);
  useEffect(() => {
    const was = previousRole.current;
    previousRole.current = snapshot.role;
    if (snapshot.role === "unconfigured" && was !== "unconfigured" && setupMode === "discovery") {
      manualDiscoveryRequested.current = false;
      automaticDiscoveryStarted.current = false;
      scheduleAutomaticDiscovery();
    }
  }, [scheduleAutomaticDiscovery, setupMode, snapshot.role]);
  useEffect(() => {
    if (!shouldPoll) { setPollStopped(false); return; }
    let alive = true;
    let attempts = 0;
    let timer: number | undefined;
    const poll = async () => {
      if (!alive) return;
      if (attempts >= 12) { setPollStopped(true); return; }
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
        if (alive && mounted.current && generation === operationGeneration.current) { onSnapshot(next); setError(null); setPhase(next.operation_state); }
      } catch (e) {
        retryDelay = Math.min(30000, 5000 * 2 ** Math.max(0, attempts - 1));
        if (alive && mounted.current && generation === operationGeneration.current) { setError(e instanceof Error ? e.message : String(e)); setPhase("offline"); }
      } finally {
        if (operationInFlight.current === "poll" && generation === operationGeneration.current) operationInFlight.current = null;
        if (alive && mounted.current && generation === operationGeneration.current) setBusy(false);
      }
      if (alive && generation === operationGeneration.current && attempts < 12) timer = window.setTimeout(() => void poll(), retryDelay);
      else if (alive && generation === operationGeneration.current) setPollStopped(true);
    };
    setPollStopped(false);
    timer = window.setTimeout(() => void poll(), 5000);
    return () => { alive = false; if (timer !== undefined) window.clearTimeout(timer); };
  }, [shouldPoll, pollGeneration]);

  useEffect(() => {
    let alive = true;
    setQrDataUrl(null);
    if (!snapshot.invite_url) return () => { alive = false; };
    void QRCode.toDataURL(snapshot.invite_url, { errorCorrectionLevel: "M", margin: 2, width: 240 })
      .then((value) => { if (alive) setQrDataUrl(value); })
      .catch((e) => { if (alive) setError(e instanceof Error ? e.message : String(e)); });
    return () => { alive = false; };
  }, [snapshot.invite_url]);

  const selectQrFile = async (file: File | undefined) => {
    if (!file) return;
    setQrBusy(true);
    setError(null);
    try {
      setInvite(await decodeInviteFile(file));
    } catch (e) {
      setInvite("");
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setQrBusy(false);
    }
  };

  const statusText = isSecondary
    ? snapshot.service_running ? t("sync.auto_running") : t("sync.auto_offline")
    : snapshot.service_running ? t("sync.running") : t("sync.stopped");
  const roleLabel = snapshot.role === "primary"
    ? t("sync.role.primary")
    : snapshot.role === "secondary"
      ? t("sync.role.secondary")
      : t("sync.role.unconfigured");
  const phaseLabel = phase === "idle"
    ? t("sync.phase.idle")
    : phase === "running"
      ? t("sync.phase.running")
      : phase === "success"
        ? t("sync.phase.success")
        : phase === "error"
          ? t("sync.phase.error")
          : phase === "cancelled"
            ? t("sync.phase.cancelled")
            : t("sync.phase.offline");
  const askConfirmation = (request: SyncManagementRequest, message: string, onSuccess?: (next: SyncDevicesSnapshot) => void) => {
    if (commandBusy) return;
    setError(null);
    setPendingConfirm({ request, message, onSuccess });
  };
  const confirmPending = () => {
    if (!pendingConfirm) return;
    const { request, onSuccess } = pendingConfirm;
    setPendingConfirm(null);
    void run(request, onSuccess);
  };
  const chooseSetupMode = (mode: "discovery" | "manual" | "primary") => {
    setError(null);
    clearAutomaticDiscovery();
    automaticDiscoveryStarted.current = false;
    if (mode === "discovery") {
      manualDiscoveryRequested.current = false;
      scheduleAutomaticDiscovery();
    } else {
      manualDiscoveryRequested.current = true;
    }
    setSetupMode(mode);
  };
  const toggleAllowLan = () => {
    setAllowLan((value) => {
      const next = !value;
      if (next) setBind((current) => bindForLan(current));
      return next;
    });
  };
  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet sheet-srv" role="dialog" aria-modal="true" aria-label={t("sync.title")} onClick={(event) => event.stopPropagation()}>
        <div className="sheet-hd">
          <div className="sheet-hd-main"><h2 className="sec-label">{t("sync.title")}</h2></div>
          <div className="sheet-actions">
            <button type="button" className="sheet-refresh" aria-label={t("sync.refresh")} title={t("sync.refresh")} disabled={commandBusy} onClick={() => void refresh()}><IconRefresh size={12} />{busy ? t("sync.refreshing") : t("sync.refresh")}</button>
            <button type="button" className="sheet-close" aria-label={t("sync.close")} onClick={onClose}><IconClose size={12} /></button>
          </div>
        </div>
        <div className="sheet-body sync-body">
          <div className="sheet-summary sync-summary">
            <span>{t("sync.role")}: {roleLabel}</span>
            <span className="dot-sep" aria-hidden="true" />
            <span>{statusText}</span>
            {snapshot.endpoint ? <><span className="dot-sep" aria-hidden="true" /><span className="num">{snapshot.endpoint}</span></> : null}
            {snapshot.stream_id ? <><span className="dot-sep" aria-hidden="true" /><span className="num">{snapshot.stream_id} · e{snapshot.data_epoch ?? "-"}</span></> : null}
            <span className="dot-sep" aria-hidden="true" /><span>{t("sync.last_connected")}: {snapshot.last_connected_at_ms ? new Date(snapshot.last_connected_at_ms).toLocaleString() : "-"}</span>
          </div>
          {error || snapshot.last_error ? <div className="sheet-notice" role="alert"><IconWarn size={12} />{error ?? snapshot.last_error}</div> : null}
          {snapshot.discovery_error ? <div className="sheet-notice" role="status"><IconWarn size={12} />{t("sync.discovery.permission_error", { message: snapshot.discovery_error })}</div> : null}
          <div className={`sync-operation sync-operation-${phase}`} aria-live="polite">
            {initializing || discovering || phase === "running" ? <Loading size={16} label={discovering ? t("sync.discovery.scanning") : t("sync.phase.running")} /> : <span className="sync-operation-dot" aria-hidden="true" />}
            <span>{discovering ? t("sync.discovery.scanning") : phaseLabel}</span>
            {snapshot.last_checked_at_ms ? <span className="sheet-hint"> · {t("sync.last_checked")} {new Date(snapshot.last_checked_at_ms).toLocaleTimeString()}</span> : null}
          </div>
          {pendingConfirm ? <div className="sync-confirm" role="alertdialog" aria-live="assertive" aria-label={t("sync.confirm.title")}>
            <div className="sync-confirm-copy"><strong>{t("sync.confirm.title")}</strong><span>{pendingConfirm.message}</span></div>
            <div className="sync-actions"><button type="button" className="btn-accent" disabled={commandBusy} onClick={confirmPending}>{t("sync.confirm.accept")}</button><button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => setPendingConfirm(null)}>{t("sync.confirm.cancel")}</button></div>
          </div> : null}
          {isUnconfigured && setupMode === "discovery" ? <div className="sync-section sync-role-picker">
            <div className="sync-section-title">{t("sync.role.choose")}</div>
            <div className="sync-choice-grid">
              <button type="button" className="sync-choice-card" disabled={commandBusy} onClick={() => chooseSetupMode("manual")}><strong>{t("sync.role.secondary")}</strong><span>{t("sync.pair.hint")}</span></button>
              <button type="button" className="sync-choice-card" disabled={commandBusy} onClick={() => chooseSetupMode("primary")}><strong>{t("sync.role.primary")}</strong><span>{t("sync.primary.hint")}</span></button>
            </div>
          </div> : null}
          {isSecondary ? <>
            <div className="sheet-row">
              <div><div className="sheet-label">{t("sync.secondary")}</div><div className="sheet-hint">{t("sync.secondary.hint")}</div></div>
              <div className="sheet-actions">
                <button type="button" className="btn-accent" disabled={commandBusy || snapshot.service_running} onClick={() => void run({ action: "start_secondary" })}>{t("sync.start")}</button>
                <button type="button" className="btn-quiet" disabled={commandBusy || !snapshot.service_running} onClick={() => void run({ action: "stop" })}>{t("sync.stop")}</button>
                <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => askConfirmation({ action: "unpair" }, t("sync.unpair.confirm"))}>{t("sync.unpair")}</button>
              </div>
            </div>
            {!snapshot.service_running ? <div className="sheet-block">
              <div className="sheet-label">{t("sync.switch_primary")}</div>
              <div className="sheet-hint">{t("sync.switch_primary.hint")}</div>
              <div className="sheet-row">
                <label className="sheet-label" htmlFor="sync-bind-switch">{t("sync.bind")}</label>
                <input id="sync-bind-switch" className="sync-input" value={bind} onChange={(event) => setBind(event.target.value)} disabled={commandBusy} />
              </div>
              <div className="sheet-row">
                <label className="sheet-label" htmlFor="sync-advertise-switch">{t("sync.advertise")}</label>
                <input id="sync-advertise-switch" className="sync-input" value={advertiseHost} onChange={(event) => setAdvertiseHost(event.target.value)} placeholder={t("sync.advertise.placeholder")} disabled={commandBusy} />
              </div>
              <div className="sheet-row">
                <div><div className="sheet-label">{t("sync.allow_lan")}</div><div className="sheet-hint">{t("sync.allow_lan.hint")}</div></div>
                <button type="button" role="switch" className="switch" aria-label={t("sync.allow_lan")} aria-checked={allowLan} onClick={toggleAllowLan} disabled={commandBusy} />
              </div>
              <button type="button" className="btn-accent" disabled={commandBusy} onClick={() => askConfirmation({ action: "start_primary", bind, advertise_host: advertiseHost.trim() || null, allow_lan: allowLan }, t("sync.switch_primary.confirm"))}>{t("sync.switch_primary")}</button>
            </div> : null}
          </> : (isPrimary || setupMode === "primary") ? <>
            <div className="sheet-row">
              <div><div className="sheet-label">{t("sync.primary")}</div><div className="sheet-hint">{t("sync.primary.hint")}</div></div>
              <div className="sheet-actions">
                <button type="button" className="btn-accent" disabled={commandBusy || snapshot.service_running} onClick={() => void run({ action: "start_primary", bind, advertise_host: advertiseHost.trim() || null, allow_lan: allowLan })}>{t("sync.start")}</button>
                <button type="button" className="btn-quiet" disabled={commandBusy || !snapshot.service_running} onClick={() => void run({ action: "stop" })}>{t("sync.stop")}</button>
                {isPrimary && !snapshot.service_running ? <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => askConfirmation({ action: "switch_to_secondary" }, t("sync.switch_secondary.confirm"), () => chooseSetupMode("discovery"))}>{t("sync.switch_secondary")}</button> : null}
                {isUnconfigured && setupMode === "primary" ? <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => chooseSetupMode("discovery")}>{t("sync.discovery.back")}</button> : null}
              </div>
            </div>
            <div className="sheet-row">
              <label className="sheet-label" htmlFor="sync-bind">{t("sync.bind")}</label>
              <input id="sync-bind" className="sync-input" value={bind} onChange={(event) => setBind(event.target.value)} disabled={commandBusy || snapshot.service_running} />
            </div>
            <div className="sheet-hint">{t("sync.bind.hint")}</div>
            <div className="sheet-row">
              <label className="sheet-label" htmlFor="sync-advertise">{t("sync.advertise")}</label>
              <input id="sync-advertise" className="sync-input" value={advertiseHost} onChange={(event) => setAdvertiseHost(event.target.value)} placeholder={t("sync.advertise.placeholder")} disabled={commandBusy || snapshot.service_running} />
            </div>
            <div className="sheet-row">
              <div><div className="sheet-label">{t("sync.allow_lan")}</div><div className="sheet-hint">{t("sync.allow_lan.hint")}</div></div>
              <button type="button" role="switch" className="switch" aria-label={t("sync.allow_lan")} aria-checked={allowLan} onClick={toggleAllowLan} disabled={commandBusy || snapshot.service_running} />
            </div>
            <div className="sheet-row">
              <div><div className="sheet-label">{t("sync.invite")}</div><div className="sheet-hint">{t("sync.invite.hint")}</div></div>
              <div className="sheet-actions">
                <button type="button" className="btn-quiet" disabled={commandBusy || !snapshot.service_running} onClick={() => void run({ action: "create_invite", expires_at_ms: null })}>{t("sync.create_invite")}</button>
                {snapshot.invite_url ? <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void copyText(snapshot.invite_url!).then(setCopied)}>{copied ? t("sync.copied") : t("sync.copy_invite")}</button> : null}
              </div>
            </div>
            {snapshot.invite_url ? <textarea className="sync-invite" readOnly value={snapshot.invite_url} aria-label={t("sync.invite")} /> : null}
            {snapshot.invite_url && qrDataUrl ? <div className="sync-qr-block">
              <img className="sync-qr" src={qrDataUrl} alt={t("sync.qr.alt")} />
              <div className="sheet-hint">{t("sync.qr.hint")}</div>
              <div className="sheet-actions">
                <a className="btn-quiet" href={qrDataUrl} download="tokenme-device-invite.png" aria-disabled={commandBusy} onClick={(event) => { if (commandBusy) event.preventDefault(); }}>{t("sync.download_qr")}</a>
                {snapshot.invite_expires_at_ms ? <span className="sheet-hint">{t("sync.invite.expires", { time: new Date(snapshot.invite_expires_at_ms).toLocaleString() })}</span> : null}
              </div>
            </div> : null}
          </> : null}
          {isPrimary && snapshot.service_running ? <div className="sync-notice is-muted">{t("sync.pair.primary_guard")}</div> : null}
          {isSecondary ? <div className="sync-notice is-muted">{t("sync.pair.secondary_guard")}</div> : null}
          {isUnconfigured ? setupMode === "discovery" ? <div className="sync-section sync-discovery-panel">
            <div className="sync-section-title">{t("sync.discovery.scan")}</div>
            <div className={`sync-scan-card${discovering ? " is-scanning" : ""}`}>
              {discovering ? <Loading size={18} label={t("sync.discovery.scanning")} /> : <span className="sync-scan-dot" aria-hidden="true" />}
              <div><strong>{snapshot.discovered_primaries.length ? `${snapshot.discovered_primaries.length} ${t("sync.discovery.scan")}` : t("sync.discovery.scan")}</strong><span>{t("sync.discovery.hint")}</span></div>
              <button type="button" className="btn-quiet" disabled={commandBusy || discovering} onClick={() => void discover()}>{discovering ? t("sync.refreshing") : t("sync.refresh")}</button>
            </div>
            {snapshot.discovered_primaries.length ? <div className="sync-device-list">{snapshot.discovered_primaries.map((primary) => <article className="sync-device-card" key={primary.primary_id}><div className="sync-device-head"><strong>{primary.display_name}</strong><span className={primary.identity_conflict ? "sync-device-status is-error" : "sync-device-status"}>{primary.identity_conflict ? t("sync.discovery.conflict") : t("sync.discovery.trust")}</span></div><div className="sync-device-meta"><code>{primary.endpoint}</code><span>sha256:{primary.certificate_pin}</span></div><div className="sync-actions"><button type="button" className="btn-accent" disabled={commandBusy || primary.identity_conflict} onClick={() => void run({ action: "pair_discovered", primary_id: primary.primary_id, endpoint: primary.endpoint, certificate_pin: primary.certificate_pin, origin, display_name: displayName.trim() || null, os: null })}>{t("sync.discovery.pair")}</button><button type="button" className="btn-quiet" disabled={commandBusy || primary.identity_conflict} onClick={() => void copyText(primary.endpoint).then(setCopied)}>{copied ? t("sync.copied") : t("sync.discovery.copy_endpoint")}</button></div></article>)}</div> : <div className="sync-empty-card"><strong>{t("sync.discovery.empty")}</strong><span>{t("sync.discovery.empty_hint")}</span></div>}
          </div> : setupMode === "manual" ? <div className="sync-section sync-manual-panel">
            <div className="sync-section-title">{t("sync.pair")}</div><div className="sync-card-copy">{t("sync.pair.hint")}</div>
            <div className="sync-field-grid"><label className="sync-field"><span>{t("sync.origin")}</span><input className="sync-input" value={origin} onChange={(event) => setOrigin(event.target.value)} placeholder={t("sync.origin")} disabled={commandBusy} /></label><label className="sync-field"><span>{t("sync.display_name")}</span><input className="sync-input" value={displayName} onChange={(event) => setDisplayName(event.target.value)} placeholder={t("sync.display_name.placeholder")} disabled={commandBusy} /></label></div>
            <textarea className="sync-invite" value={invite} onChange={(event) => setInvite(event.target.value)} placeholder={t("sync.paste_invite")} aria-label={t("sync.paste_invite")} />
            <input ref={qrFile} type="file" accept="image/png,image/jpeg,image/webp" className="sync-file" onChange={(event) => { void selectQrFile(event.target.files?.[0]); event.currentTarget.value = ""; }} />
            <div className="sync-actions"><button type="button" className="btn-quiet" disabled={commandBusy || qrBusy} onClick={() => void qrFile.current?.click()}>{qrBusy ? t("sync.scanning_qr") : t("sync.scan_qr")}</button><button type="button" className="btn-accent" disabled={commandBusy || qrBusy || !invite.trim()} onClick={() => void run({ action: "pair", invite: invite.trim(), origin, display_name: displayName.trim() || null, os: null })}>{t("sync.pair")}</button><button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => chooseSetupMode("discovery")}>{t("sync.discovery.back")}</button></div>
          </div> : null : null}
          {snapshot.pairing_requests.length ? <div className="sheet-block"><div className="sheet-label">{t("sync.requests")}</div>{snapshot.pairing_requests.map((request) => {
            const requestStatus = request.status === "pending" ? t("sync.request.pending") : request.status === "expired" ? t("sync.request.expired") : request.status === "rejected" ? t("sync.request.rejected") : request.status;
            const canClear = request.status === "pending" || request.status === "expired";
            return <div className="sheet-row" key={request.request_id}><div><div className="sheet-label">{request.display_name} · {request.origin}</div><div className="sheet-hint">{request.os} · {request.device_id} · {requestStatus}</div></div>{canClear ? <div className="sheet-actions">{request.status === "pending" ? <button type="button" className="btn-accent" disabled={commandBusy} onClick={() => void run({ action: "approve", request_id: request.request_id })}>{t("sync.approve")}</button> : null}<button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void run({ action: "reject", request_id: request.request_id })}>{request.status === "expired" ? t("sync.clear_expired") : t("sync.reject")}</button></div> : null}</div>;
          })}{hasPendingRequest ? <div className="sync-poll-status">{pollStopped ? t("sync.pairing.polling_stopped") : <Loading size={14} label={t("sync.pairing.polling")} />}<button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void refresh()}>{t("sync.pairing.retry_status")}</button></div> : null}</div> : null}
          {awaitingSecondaryApproval ? <div className="sheet-block"><div className="sheet-hint">{t("sync.pairing.waiting_primary")}</div><div className="sync-poll-status">{pollStopped ? t("sync.pairing.polling_stopped") : <Loading size={14} label={t("sync.pairing.polling")} />}<button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void refresh()}>{t("sync.pairing.retry_status")}</button></div></div> : null}
          {snapshot.devices.length ? <div className="sheet-block"><div className="sheet-label">{t("sync.devices")}</div>{snapshot.devices.map((device) => <div className="sheet-row" key={device.device_id}><div><div className="sheet-label">{device.display_name} · {device.origin}</div><div className="sheet-hint">{device.os} · {t(`scope.device.${device.status === "reconcile_required" ? "reconcile" : device.status}` as "scope.device.paired")}</div></div><div className="sheet-actions">{device.status === "paused" ? <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void run({ action: "resume", device_id: device.device_id })}>{t("sync.resume")}</button> : device.status === "paired" ? <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void run({ action: "pause", device_id: device.device_id })}>{t("sync.pause")}</button> : null}<button type="button" className="btn-quiet" disabled={commandBusy || device.status === "revoked"} onClick={() => askConfirmation({ action: "revoke", device_id: device.device_id }, t("sync.revoke.confirm", { name: device.display_name }))}>{t("sync.revoke")}</button></div></div>)}</div> : null}
          <section className="sync-section sync-diagnostics" aria-labelledby="sync-diagnostics-title">
            <div className="sync-diagnostics-head">
              <div>
                <div id="sync-diagnostics-title" className="sync-section-title">{t("sync.diagnostics.title")}</div>
                <div className="sync-card-copy">{t("sync.diagnostics.hint")}</div>
              </div>
              <button type="button" role="switch" className="switch" aria-label={t("sync.diagnostics.toggle")} aria-checked={diagnosticsEnabled} onClick={toggleDiagnostics} disabled={commandBusy} />
            </div>
            <div className={`sync-notice ${diagnosticsReady ? "" : "is-muted"}`}>{!diagnosticsEnabled ? t("sync.diagnostics.disabled") : remoteDiagnosticsReady ? t("sync.diagnostics.ready") : t("sync.diagnostics.local_ready")}</div>
            {diagnosticsError ? <div className="sync-diagnostics-error" role="alert"><IconWarn size={12} /><span>{diagnosticsError}</span></div> : null}
            <fieldset className="sync-diagnostics-settings" disabled={!remoteDiagnosticsReady || commandBusy}>
              <legend className="sync-diagnostics-label"><span>{t("sync.diagnostics.scope")}</span><button type="button" className="btn-quiet" disabled={diagnosticsScopes.length === DIAGNOSTICS_SCOPES.length} onClick={() => { setDiagnosticsScopes(DIAGNOSTICS_SCOPES); setDiagnosticsError(null); }}>{t("sync.diagnostics.scope.all")}</button></legend>
              <div className="sync-diagnostics-scope-grid" role="group" aria-label={t("sync.diagnostics.scope.a11y")}>
                {DIAGNOSTICS_SCOPES.map((scope) => <button type="button" className="sync-diagnostics-scope" key={scope} role="checkbox" aria-checked={diagnosticsScopes.includes(scope)} onClick={() => toggleDiagnosticsScope(scope)}>
                  <strong>{scope === "diagnostics.read" ? t("sync.diagnostics.scope.read") : t("sync.diagnostics.scope.control")}</strong>
                  <span>{scope === "diagnostics.read" ? t("sync.diagnostics.scope.read.hint") : t("sync.diagnostics.scope.control.hint")}</span>
                </button>)}
              </div>
              <div className="sync-diagnostics-field">
                <div className="sync-diagnostics-label">{t("sync.diagnostics.ttl")}</div>
                <div className="seg seg-wide" role="radiogroup" aria-label={t("sync.diagnostics.ttl.a11y")}>
                  {DIAGNOSTICS_TTLS.map((ttl) => <button type="button" className="seg-btn" key={ttl} role="radio" aria-checked={diagnosticsTtl === ttl} onClick={() => setDiagnosticsTtl(ttl)}>{ttl === "15m" ? t("sync.diagnostics.ttl.15m") : ttl === "1h" ? t("sync.diagnostics.ttl.1h") : t("sync.diagnostics.ttl.session")}</button>)}
                </div>
              </div>
              <div className="sync-diagnostics-field">
                <label className="sync-field"><span>{t("sync.diagnostics.target")}</span><select className="sync-input" value={diagnosticsTarget} onChange={(event) => setDiagnosticsTarget(event.target.value)} aria-label={t("sync.diagnostics.target")}>
                  <option value="">{pairedDiagnosticsDevices.length ? t("sync.diagnostics.target.choose") : t("sync.diagnostics.target.empty")}</option>
                  {pairedDiagnosticsDevices.map((device) => <option key={device.device_id} value={device.device_id}>{device.display_name} · {device.origin} ({device.os})</option>)}
                </select></label>
                <div className="sync-card-copy">{t("sync.diagnostics.target.hint")}</div>
              </div>
              <div className="sync-diagnostics-field">
                <div className="sync-diagnostics-label">{t("sync.diagnostics.actions")}</div>
                <div className="sync-diagnostics-actions">
                  <button type="button" className="btn-accent" disabled={commandBusy || !diagnosticsScopes.length || !diagnosticsTarget || !pairedDiagnosticsDevices.some((device) => device.device_id === diagnosticsTarget)} onClick={createDiagnosticsSession}>{t("sync.diagnostics.create_qr")}</button>
                  <button type="button" className="btn-quiet" disabled={!diagnosticsConfig || commandBusy} onClick={() => void copyDiagnosticsConfig()}>{diagnosticsCopied ? t("sync.diagnostics.copied") : t("sync.diagnostics.copy_code")}</button>
                </div>
                <div className="sync-card-copy">{t("sync.diagnostics.action_hint")}</div>
              </div>
            </fieldset>
            {diagnosticsQrDataUrl && diagnosticsConfig ? <div className="sync-qr-block">
              <img className="sync-qr" src={diagnosticsQrDataUrl} alt={t("sync.diagnostics.qr_alt")} />
              <div className="sheet-hint">{t("sync.diagnostics.qr_hint")}</div>
              <div className="sheet-actions">
                <a className="btn-quiet" href={diagnosticsQrDataUrl} download="tokenme-diagnostics.png">{t("sync.diagnostics.download_qr")}</a>
                {diagnosticsConfigUrl ? <a className="btn-quiet" href={diagnosticsConfigUrl} download="tokenme-diagnostics.json">{t("sync.diagnostics.download_json")}</a> : null}
              </div>
            </div> : null}
            <div className="sync-diagnostics-subsection">
              <div className="sync-diagnostics-label">{t("sync.diagnostics.sessions")}</div>
              {diagnosticsSessions.length ? <div className="sync-diagnostics-session-list">{diagnosticsSessions.map((session) => <div className="sync-diagnostics-session" key={session.session_id}><strong>{session.origin}</strong><span>{session.target_device} · {session.scopes.join(", ")}</span><span>{t("sync.diagnostics.session.expires", { time: new Date(session.expires_at_ms).toLocaleString() })}</span><button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void runDiagnostics({ action: "revoke_grant", grant_id: session.session_id })}>{t("sync.diagnostics.revoke_session")}</button></div>)}</div> : <div className="sync-empty-card"><strong>{t("sync.diagnostics.sessions.empty")}</strong><span>{t("sync.diagnostics.sessions.empty_hint")}</span></div>}
            </div>
            <div className="sync-diagnostics-operation" data-state={diagnosticsOperation?.state ?? "idle"} aria-live="polite">
              <span className="sync-diagnostics-label">{t("sync.diagnostics.operation")}</span>
              <strong>{diagnosticsOperation ? diagnosticsActionLabel(diagnosticsOperation.action) : t("sync.diagnostics.operation.none")}</strong>
              {diagnosticsOperation ? <span className="sync-diagnostics-operation-state">{diagnosticsOperationStateLabel(diagnosticsOperation.state)} · {t("sync.diagnostics.operation.at", { time: new Date(diagnosticsOperation.at_ms).toLocaleTimeString() })} · {t("sync.diagnostics.operation.id", { id: diagnosticsOperation.operation_id })}{diagnosticsOperation.error_code ? ` · ${diagnosticsOperation.error_code}` : ""}</span> : null}
            </div>
            {diagnosticsRevokeConfirm ? <div className="sync-confirm" role="alertdialog" aria-live="assertive" aria-label={t("sync.diagnostics.revoke_all.confirm")}>
              <div className="sync-confirm-copy"><strong>{t("sync.diagnostics.revoke_all")}</strong><span>{t("sync.diagnostics.revoke_all.confirm")}</span></div>
              <div className="sync-actions"><button type="button" className="btn-accent" disabled={commandBusy} onClick={() => void runDiagnostics({ action: "revoke_all" })}>{t("sync.confirm.accept")}</button><button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => setDiagnosticsRevokeConfirm(false)}>{t("sync.confirm.cancel")}</button></div>
            </div> : null}
            <div className="sync-diagnostics-footer">
              <button type="button" className="btn-quiet" disabled={!remoteDiagnosticsReady || commandBusy} onClick={() => setDiagnosticsRevokeConfirm(true)}>{t("sync.diagnostics.revoke_all")}</button>
              <button type="button" className="btn-quiet" disabled={!diagnosticsError && !diagnosticsOperation} onClick={clearDiagnosticsErrors}>{t("sync.diagnostics.clear_errors")}</button>
            </div>
          </section>
          {!isSecondary ? <div className="sheet-hint">{snapshot.role === "unconfigured" ? t("sync.auto_disabled") : null}</div> : null}
          <div className="sheet-summary num">{t("sync.queue", { pending: snapshot.pending, bytes: snapshot.pending_bytes })}</div>
        </div>
      </div>
    </div>
  );
}

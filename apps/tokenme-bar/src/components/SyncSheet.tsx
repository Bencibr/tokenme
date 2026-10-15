import { useCallback, useEffect, useRef, useState } from "react";
import jsQR from "jsqr";
import QRCode from "qrcode";
import type { SyncDevicesSnapshot, SyncManagementRequest } from "../types";
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
  const hasPendingRequest = snapshot.pairing_requests.some((request) => request.status === "pending");
  const awaitingSecondaryApproval = isSecondary && snapshot.service_running && !snapshot.stream_id;
  const shouldPoll = snapshot.service_running && (hasPendingRequest || awaitingSecondaryApproval);
  const commandBusy = busy || initializing || discovering;
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
                <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => { if (window.confirm(t("sync.unpair.confirm"))) void run({ action: "unpair" }); }}>{t("sync.unpair")}</button>
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
                <button type="button" role="switch" className="switch" aria-label={t("sync.allow_lan")} aria-checked={allowLan} onClick={() => setAllowLan((value) => !value)} disabled={commandBusy} />
              </div>
              <button type="button" className="btn-accent" disabled={commandBusy} onClick={() => { if (window.confirm(t("sync.switch_primary.confirm"))) void run({ action: "start_primary", bind, advertise_host: advertiseHost.trim() || null, allow_lan: allowLan }); }}>{t("sync.switch_primary")}</button>
            </div> : null}
          </> : (isPrimary || setupMode === "primary") ? <>
            <div className="sheet-row">
              <div><div className="sheet-label">{t("sync.primary")}</div><div className="sheet-hint">{t("sync.primary.hint")}</div></div>
              <div className="sheet-actions">
                <button type="button" className="btn-accent" disabled={commandBusy || snapshot.service_running} onClick={() => void run({ action: "start_primary", bind, advertise_host: advertiseHost.trim() || null, allow_lan: allowLan })}>{t("sync.start")}</button>
                <button type="button" className="btn-quiet" disabled={commandBusy || !snapshot.service_running} onClick={() => void run({ action: "stop" })}>{t("sync.stop")}</button>
                {isPrimary && !snapshot.service_running ? <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => { if (window.confirm(t("sync.switch_secondary.confirm"))) void run({ action: "switch_to_secondary" }, () => chooseSetupMode("discovery")); }}>{t("sync.switch_secondary")}</button> : null}
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
              <button type="button" role="switch" className="switch" aria-label={t("sync.allow_lan")} aria-checked={allowLan} onClick={() => setAllowLan((value) => !value)} disabled={commandBusy || snapshot.service_running} />
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
            return <div className="sheet-row" key={request.request_id}><div><div className="sheet-label">{request.display_name} · {request.origin}</div><div className="sheet-hint">{request.os} · {request.device_id} · {requestStatus}</div></div>{request.status === "pending" ? <div className="sheet-actions"><button type="button" className="btn-accent" disabled={commandBusy} onClick={() => void run({ action: "approve", request_id: request.request_id })}>{t("sync.approve")}</button><button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void run({ action: "reject", request_id: request.request_id })}>{t("sync.reject")}</button></div> : null}</div>;
          })}{hasPendingRequest ? <div className="sync-poll-status">{pollStopped ? t("sync.pairing.polling_stopped") : <Loading size={14} label={t("sync.pairing.polling")} />}<button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void refresh()}>{t("sync.pairing.retry_status")}</button></div> : null}</div> : null}
          {awaitingSecondaryApproval ? <div className="sheet-block"><div className="sheet-hint">{t("sync.pairing.waiting_primary")}</div><div className="sync-poll-status">{pollStopped ? t("sync.pairing.polling_stopped") : <Loading size={14} label={t("sync.pairing.polling")} />}<button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void refresh()}>{t("sync.pairing.retry_status")}</button></div></div> : null}
          {snapshot.devices.length ? <div className="sheet-block"><div className="sheet-label">{t("sync.devices")}</div>{snapshot.devices.map((device) => <div className="sheet-row" key={device.device_id}><div><div className="sheet-label">{device.display_name} · {device.origin}</div><div className="sheet-hint">{device.os} · {t(`scope.device.${device.status === "reconcile_required" ? "reconcile" : device.status}` as "scope.device.paired")}</div></div><div className="sheet-actions">{device.status === "paused" ? <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void run({ action: "resume", device_id: device.device_id })}>{t("sync.resume")}</button> : device.status === "paired" ? <button type="button" className="btn-quiet" disabled={commandBusy} onClick={() => void run({ action: "pause", device_id: device.device_id })}>{t("sync.pause")}</button> : null}<button type="button" className="btn-quiet" disabled={commandBusy || device.status === "revoked"} onClick={() => { if (window.confirm(t("sync.revoke.confirm", { name: device.display_name }))) void run({ action: "revoke", device_id: device.device_id }); }}>{t("sync.revoke")}</button></div></div>)}</div> : null}
          {!isSecondary ? <div className="sheet-hint">{snapshot.role === "unconfigured" ? t("sync.auto_disabled") : null}</div> : null}
          <div className="sheet-summary num">{t("sync.queue", { pending: snapshot.pending, bytes: snapshot.pending_bytes })}</div>
        </div>
      </div>
    </div>
  );
}

import { useState } from "react";
import type { SyncDevicesSnapshot, SyncManagementRequest } from "../types";
import { bridge } from "../lib/bridge";
import { t } from "../lib/i18n";
import { IconClose, IconWarn } from "./Icons";

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

export function SyncSheet({ snapshot, onSnapshot, onClose }: { snapshot: SyncDevicesSnapshot; onSnapshot: (next: SyncDevicesSnapshot) => void; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [bind, setBind] = useState("127.0.0.1:47821");
  const [allowLan, setAllowLan] = useState(false);
  const [invite, setInvite] = useState("");
  const [origin, setOrigin] = useState("secondary");
  const [copied, setCopied] = useState(false);

  const run = async (request: SyncManagementRequest) => {
    setBusy(true);
    setError(null);
    try {
      onSnapshot(await bridge.syncDevices(request));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const isSecondary = snapshot.role === "secondary";
  const statusText = isSecondary
    ? snapshot.service_running ? t("sync.auto_running") : t("sync.auto_offline")
    : snapshot.service_running ? t("sync.running") : t("sync.stopped");
  const roleLabel = snapshot.role === "primary"
    ? t("sync.role.primary")
    : snapshot.role === "secondary"
      ? t("sync.role.secondary")
      : t("sync.role.unconfigured");
  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet sheet-srv" role="dialog" aria-modal="true" aria-label={t("sync.title")} onClick={(event) => event.stopPropagation()}>
        <div className="sheet-hd">
          <div className="sheet-hd-main"><h2 className="sec-label">{t("sync.title")}</h2></div>
          <button type="button" className="sheet-close" aria-label={t("sync.close")} onClick={onClose}><IconClose size={12} /></button>
        </div>
        <div className="sheet-body">
          <div className="sheet-summary">
            <span>{t("sync.role")}: {roleLabel}</span>
            <span className="dot-sep" aria-hidden="true" />
            <span>{statusText}</span>
            {snapshot.endpoint ? <><span className="dot-sep" aria-hidden="true" /><span className="num">{snapshot.endpoint}</span></> : null}
            {snapshot.stream_id ? <><span className="dot-sep" aria-hidden="true" /><span className="num">{snapshot.stream_id} · e{snapshot.data_epoch ?? "-"}</span></> : null}
            <span className="dot-sep" aria-hidden="true" /><span>{t("sync.last_connected")}: {snapshot.last_connected_at_ms ? new Date(snapshot.last_connected_at_ms).toLocaleString() : "-"}</span>
          </div>
          {error || snapshot.last_error ? <div className="sheet-notice" role="alert"><IconWarn size={12} />{error ?? snapshot.last_error}</div> : null}
          {isSecondary ? <div className="sheet-row">
            <div><div className="sheet-label">{t("sync.secondary")}</div><div className="sheet-hint">{t("sync.secondary.hint")}</div></div>
            <div className="sheet-actions">
              <button type="button" className="btn-accent" disabled={busy || snapshot.service_running} onClick={() => void run({ action: "start_secondary" })}>{t("sync.start")}</button>
              <button type="button" className="btn-quiet" disabled={busy || !snapshot.service_running} onClick={() => void run({ action: "stop" })}>{t("sync.stop")}</button>
            </div>
          </div> : <>
            <div className="sheet-row">
              <div><div className="sheet-label">{t("sync.primary")}</div><div className="sheet-hint">{t("sync.primary.hint")}</div></div>
              <div className="sheet-actions">
                <button type="button" className="btn-accent" disabled={busy || snapshot.service_running} onClick={() => void run({ action: "start_primary", bind, advertise_host: null, allow_lan: allowLan })}>{t("sync.start")}</button>
                <button type="button" className="btn-quiet" disabled={busy || !snapshot.service_running} onClick={() => void run({ action: "stop" })}>{t("sync.stop")}</button>
              </div>
            </div>
            <div className="sheet-row">
              <label className="sheet-label" htmlFor="sync-bind">{t("sync.bind")}</label>
              <input id="sync-bind" className="sync-input" value={bind} onChange={(event) => setBind(event.target.value)} disabled={busy || snapshot.service_running} />
            </div>
            <div className="sheet-row">
              <div><div className="sheet-label">{t("sync.allow_lan")}</div><div className="sheet-hint">{t("sync.allow_lan.hint")}</div></div>
              <button type="button" role="switch" className="switch" aria-checked={allowLan} onClick={() => setAllowLan((value) => !value)} disabled={busy || snapshot.service_running} />
            </div>
            <div className="sheet-row">
              <div><div className="sheet-label">{t("sync.invite")}</div><div className="sheet-hint">{t("sync.invite.hint")}</div></div>
              <div className="sheet-actions">
                <button type="button" className="btn-quiet" disabled={busy || !snapshot.service_running} onClick={() => void run({ action: "create_invite", expires_at_ms: null })}>{t("sync.create_invite")}</button>
                {snapshot.invite_url ? <button type="button" className="btn-quiet" disabled={busy} onClick={() => void copyText(snapshot.invite_url!).then(setCopied)}>{copied ? t("sync.copied") : t("sync.copy_invite")}</button> : null}
              </div>
            </div>
            {snapshot.invite_url ? <textarea className="sync-invite" readOnly value={snapshot.invite_url} aria-label={t("sync.invite")} /> : null}
          </>}
          <div className="sheet-block">
            <div className="sheet-label">{t("sync.pair")}</div>
            <div className="sheet-hint">{t("sync.pair.hint")}</div>
            <input className="sync-input" value={origin} onChange={(event) => setOrigin(event.target.value)} placeholder={t("sync.origin")} disabled={busy} />
            <textarea className="sync-invite" value={invite} onChange={(event) => setInvite(event.target.value)} placeholder={t("sync.paste_invite")} />
            <button type="button" className="btn-accent" disabled={busy || !invite.trim()} onClick={() => void run({ action: "pair", invite: invite.trim(), origin, display_name: null, os: null })}>{t("sync.pair")}</button>
          </div>
          {snapshot.pairing_requests.length ? <div className="sheet-block"><div className="sheet-label">{t("sync.requests")}</div>{snapshot.pairing_requests.filter((request) => request.status === "pending").map((request) => <div className="sheet-row" key={request.request_id}><div><div className="sheet-label">{request.display_name} · {request.origin}</div><div className="sheet-hint">{request.os} · {request.device_id}</div></div><div className="sheet-actions"><button type="button" className="btn-accent" disabled={busy} onClick={() => void run({ action: "approve", request_id: request.request_id })}>{t("sync.approve")}</button><button type="button" className="btn-quiet" disabled={busy} onClick={() => void run({ action: "reject", request_id: request.request_id })}>{t("sync.reject")}</button></div></div>)}</div> : null}
          {snapshot.devices.length ? <div className="sheet-block"><div className="sheet-label">{t("sync.devices")}</div>{snapshot.devices.map((device) => <div className="sheet-row" key={device.device_id}><div><div className="sheet-label">{device.display_name} · {device.origin}</div><div className="sheet-hint">{device.os} · {t(`scope.device.${device.status === "reconcile_required" ? "reconcile" : device.status}` as "scope.device.paired")}</div></div><div className="sheet-actions">{device.status === "paused" ? <button type="button" className="btn-quiet" disabled={busy} onClick={() => void run({ action: "resume", device_id: device.device_id })}>{t("sync.resume")}</button> : device.status === "paired" ? <button type="button" className="btn-quiet" disabled={busy} onClick={() => void run({ action: "pause", device_id: device.device_id })}>{t("sync.pause")}</button> : null}<button type="button" className="btn-quiet" disabled={busy || device.status === "revoked"} onClick={() => void run({ action: "revoke", device_id: device.device_id })}>{t("sync.revoke")}</button></div></div>)}</div> : null}
          {!isSecondary ? <div className="sheet-hint">{snapshot.role === "unconfigured" ? t("sync.auto_disabled") : null}</div> : null}
          <div className="sheet-summary num">{t("sync.queue", { pending: snapshot.pending, bytes: snapshot.pending_bytes })}</div>
        </div>
      </div>
    </div>
  );
}

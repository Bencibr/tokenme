import { useEffect, useState } from "react";
import type { PanelSettings } from "../types";
import { bridge } from "../lib/bridge";
import { IconClose } from "./Icons";

const INTERVALS: { secs: number; label: string }[] = [
  { secs: 15, label: "15 秒" },
  { secs: 30, label: "30 秒" },
  { secs: 60, label: "1 分钟" },
  { secs: 300, label: "5 分钟" },
];

/**
 * The panel's only knobs, as a bottom sheet: fallback refresh cadence, the
 * login-item switch, and the build version. Both switches apply on click —
 * there is no dirty state to save, so there is no save button.
 */
export function SettingsSheet({ onClose }: { onClose: () => void }) {
  const [settings, setSettings] = useState<PanelSettings | null>(null);

  useEffect(() => {
    let alive = true;
    void bridge.panelSettings().then((s) => alive && setSettings(s));
    return () => {
      alive = false;
    };
  }, []);

  const setAutostart = (on: boolean) => {
    setSettings((s) => (s ? { ...s, autostart: on } : s));
    void bridge.setAutostart(on);
  };

  const setRefresh = (secs: number) => {
    setSettings((s) => (s ? { ...s, refresh_secs: secs } : s));
    void bridge.setRefreshSecs(secs);
  };

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div
        className="sheet"
        role="dialog"
        aria-modal="true"
        aria-label="设置"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="sheet-hd">
          <h2 className="sec-label">设置</h2>
          <button type="button" className="sheet-close" aria-label="关闭设置" onClick={onClose}>
            <IconClose size={12} />
          </button>
        </div>

        <div className="sheet-row">
          <div>
            <div className="sheet-label">自动刷新</div>
            <div className="sheet-hint">文件变化时仍会立即刷新</div>
          </div>
          <div className="seg" role="radiogroup" aria-label="自动刷新间隔">
            {INTERVALS.map((i) => (
              <button
                key={i.secs}
                type="button"
                role="radio"
                className="seg-btn"
                aria-checked={settings?.refresh_secs === i.secs}
                onClick={() => setRefresh(i.secs)}
              >
                {i.label}
              </button>
            ))}
          </div>
        </div>

        <div className="sheet-row">
          <div className="sheet-label">开机自动启动</div>
          <button
            type="button"
            role="switch"
            className="switch"
            aria-checked={settings?.autostart ?? false}
            aria-label="开机自动启动"
            onClick={() => setAutostart(!(settings?.autostart ?? false))}
          />
        </div>

        <p className="sheet-foot num">tokenme v{settings?.version ?? "…"} · 数据只存在本机</p>
      </div>
    </div>
  );
}

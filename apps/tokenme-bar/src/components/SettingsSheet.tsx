import { useEffect, useState } from "react";
import type { PanelSettings, ThemeKey } from "../types";
import { bridge } from "../lib/bridge";
import { IconClose } from "./Icons";

const INTERVALS: { secs: number; label: string }[] = [
  { secs: 15, label: "15 秒" },
  { secs: 30, label: "30 秒" },
  { secs: 60, label: "1 分钟" },
  { secs: 300, label: "5 分钟" },
];

const THEMES: { key: ThemeKey; label: string }[] = [
  { key: "system", label: "跟随系统" },
  { key: "light", label: "浅色" },
  { key: "dark", label: "深色" },
];

/**
 * The panel's only knobs, as a bottom sheet: fallback refresh cadence, the
 * login-item switch, and the build version. Both switches apply on click —
 * there is no dirty state to save, so there is no save button.
 */
export function SettingsSheet({ onClose, onMoney }: { onClose: () => void; onMoney: (on: boolean) => void }) {
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

  // The bridge applies `data-theme` synchronously, so the pick is visible
  // before the persist round-trip answers.
  const setTheme = (theme: ThemeKey) => {
    setSettings((s) => (s ? { ...s, theme } : s));
    void bridge.setTheme(theme);
  };

  const setShowMoney = (on: boolean) => {
    setSettings((s) => (s ? { ...s, show_money: on } : s));
    onMoney(on);
    void bridge.setShowMoney(on);
  };

  const quit = () => {
    void bridge.quit();
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
          <div className="sheet-label">外观</div>
          <div className="seg" role="radiogroup" aria-label="外观主题">
            {THEMES.map((t) => (
              <button
                key={t.key}
                type="button"
                role="radio"
                className="seg-btn"
                aria-checked={settings?.theme === t.key}
                onClick={() => setTheme(t.key)}
              >
                {t.label}
              </button>
            ))}
          </div>
        </div>

        <div className="sheet-row">
          <div>
            <div className="sheet-label">金额折算</div>
            <div className="sheet-hint">关闭后只显示 tokens 与 credits</div>
          </div>
          <button
            type="button"
            role="switch"
            className="switch"
            aria-checked={settings?.show_money ?? true}
            aria-label="金额折算"
            onClick={() => setShowMoney(!(settings?.show_money ?? true))}
          />
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

        {bridge.live ? (
          <div className="sheet-row sheet-row-danger">
            <div>
              <div className="sheet-label">退出程序</div>
              <div className="sheet-hint">关闭面板并退出托盘进程</div>
            </div>
            <button type="button" className="quit-btn" onClick={quit}>
              退出
            </button>
          </div>
        ) : null}

        <p className="sheet-foot num">tokenme v{settings?.version ?? "…"} · 数据只存在本机</p>
      </div>
    </div>
  );
}

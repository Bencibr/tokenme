import { useEffect, useState } from "react";
import type { PanelSettings, ThemeKey, UpdateStatus } from "../types";
import { bridge, isWindows } from "../lib/bridge";
import { CONTACT_EMAIL, RELEASE_PAGE_URL } from "../lib/about";
import { t } from "../lib/i18n";
import { IconClose } from "./Icons";

const INTERVALS: { secs: number; label: () => string }[] = [
  { secs: 15, label: () => t("set.15s") },
  { secs: 30, label: () => t("set.30s") },
  { secs: 60, label: () => t("set.1m") },
  { secs: 300, label: () => t("set.5m") },
];

const THEMES: { key: ThemeKey; label: () => string }[] = [
  { key: "system", label: () => t("set.theme.system") },
  { key: "light", label: () => t("set.theme.light") },
  { key: "dark", label: () => t("set.theme.dark") },
];

/**
 * The panel's only knobs, as a bottom sheet: fallback refresh cadence, the
 * login-item switch, and the version. Both switches apply on click —
 * there is no dirty state to save, so there is no save button.
 */
export function SettingsSheet({ onClose, onEmptyTools }: { onClose: () => void; onEmptyTools: (on: boolean) => void }) {
  const [settings, setSettings] = useState<PanelSettings | null>(null);
  const [update, setUpdate] = useState<UpdateStatus | null>(null);
  const [updateBusy, setUpdateBusy] = useState(false);
  // 0–100 while the artifact streams in; the button reads the set.dl label.
  const [progress, setProgress] = useState<number | null>(null);

  useEffect(() => bridge.onUpdateProgress((p) => setProgress(p.percent)), []);

  useEffect(() => {
    let alive = true;
    void bridge.panelSettings().then((s) => {
      if (!alive) return;
      setSettings(s);
      // 打开设置就问一次（开关开着时）：底部的版本行不必等到用户切开关才有答案。
      if (s.auto_update_check && s.version !== "dev") {
        setUpdateBusy(true);
        void bridge
          .checkUpdate()
          .then((status) => {
            // quiet = 检查失败或已是最新：没有可行动的信息就不渲染任何行。
            if (alive) setUpdate(status.phase === "available" || status.phase === "downloaded" ? status : null);
          })
          .catch(() => alive && setUpdate(null))
          .finally(() => alive && setUpdateBusy(false));
      }
    });
    return () => {
      alive = false;
    };
  }, []);



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

  const setShowEmptyTools = (on: boolean) => {
    setSettings((s) => (s ? { ...s, show_empty_tools: on } : s));
    onEmptyTools(on);
    void bridge.setShowEmptyTools(on);
  };

  const setBubble = (on: boolean) => {
    setSettings((s) => (s ? { ...s, bubble_enabled: on } : s));
    void bridge.setBubbleEnabled(on);
  };

  const setAutostart = (on: boolean) => {
    setSettings((s) => (s ? { ...s, autostart: on } : s));
    void bridge.setAutostart(on);
  };

  const setHostExitPause = (on: boolean) => {
    setSettings((s) => (s ? { ...s, host_exit_pause: on } : s));
    void bridge.setHostExitPause(on);
  };

  const setAutoUpdateCheck = (on: boolean) => {
    setSettings((s) => (s ? { ...s, auto_update_check: on } : s));
    void bridge.setAutoUpdateCheck(on);
    // 开启的一刻就问一次：用户不必等到下次启动才知道有没有新版。
    if (on) {
      setUpdateBusy(true);
      void bridge
        .checkUpdate()
        .then((status) => {
          // quiet = 检查失败或已是最新：没有可行动的信息就不渲染任何行，
          // 否则会留下一个空白的弹窗条（用户报修的那块）。
          setUpdate(status.phase === "available" || status.phase === "downloaded" ? status : null);
        })
        .catch(() => setUpdate(null))
        .finally(() => setUpdateBusy(false));
    } else {
      setUpdate(null);
    }
  };

  const runUpdate = async () => {
    setUpdateBusy(true);
    try {
      const downloaded = await bridge.downloadUpdate();
      setUpdate(downloaded);
      if (downloaded.phase === "downloaded") {
        await bridge.installUpdate();
      }
    } catch (e) {
      setUpdate({ phase: "error", message: String(e), version: null });
    } finally {
      setUpdateBusy(false);
      setProgress(null);
    }
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
        aria-label={t("set.title")}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="sheet-hd">
          <h2 className="sec-label">{t("set.title")}</h2>
          <button type="button" className="sheet-close" aria-label={t("set.close.a11y")} onClick={onClose}>
            <IconClose size={12} />
          </button>
        </div>

        <div className="sheet-row">
          <div>
            <div className="sheet-label">{t("set.refresh")}</div>
            <div className="sheet-hint">{t("set.refresh.hint")}</div>
          </div>
          <div className="seg" role="radiogroup" aria-label={t("set.interval.a11y")}>
            {INTERVALS.map((i) => (
              <button
                key={i.secs}
                type="button"
                role="radio"
                className="seg-btn"
                aria-checked={settings?.refresh_secs === i.secs}
                onClick={() => setRefresh(i.secs)}
              >
                {i.label()}
              </button>
            ))}
          </div>
        </div>

        {isWindows ? (
          <div className="sheet-row">
            <div>
              <div className="sheet-label">{t("set.bubble")}</div>
              <div className="sheet-hint">{t("set.bubble.hint")}</div>
            </div>
            <button
              type="button"
              role="switch"
              className="switch"
              aria-checked={settings?.bubble_enabled ?? true}
              aria-label={t("set.bubble")}
              onClick={() => setBubble(!(settings?.bubble_enabled ?? true))}
            />
          </div>
        ) : null}

        <div className="sheet-row">
          <div className="sheet-label">{t("set.theme")}</div>
          <div className="seg" role="radiogroup" aria-label={t("set.theme.a11y")}>
            {THEMES.map((th) => (
              <button
                key={th.key}
                type="button"
                role="radio"
                className="seg-btn"
                aria-checked={settings?.theme === th.key}
                onClick={() => setTheme(th.key)}
              >
                {th.label()}
              </button>
            ))}
          </div>
        </div>

        <div className="sheet-row">
          <div>
            <div className="sheet-label">{t("set.showempty")}</div>
            <div className="sheet-hint">{t("set.showempty.hint")}</div>
          </div>
          <button
            type="button"
            role="switch"
            className="switch"
            aria-checked={settings?.show_empty_tools ?? false}
            aria-label={t("set.showempty")}
            onClick={() => setShowEmptyTools(!(settings?.show_empty_tools ?? false))}
          />
        </div>

        <div className="sheet-row">
          <div>
            <div className="sheet-label">{t("set.pause")}</div>
            <div className="sheet-hint">{t("set.pause.hint")}</div>
          </div>
          <button
            type="button"
            role="switch"
            className="switch"
            aria-checked={settings?.host_exit_pause ?? true}
            aria-label={t("set.pause")}
            onClick={() => setHostExitPause(!(settings?.host_exit_pause ?? true))}
          />
        </div>

        <div className="sheet-row">
          <div>
            <div className="sheet-label">{t("set.autoupdate")}</div>
            <div className="sheet-hint">{t("set.autoupdate.hint")}</div>
          </div>
          <button
            type="button"
            role="switch"
            className="switch"
            aria-checked={settings?.auto_update_check ?? true}
            aria-label={t("set.autoupdate")}
            onClick={() => setAutoUpdateCheck(!(settings?.auto_update_check ?? true))}
          />
        </div>

        <div className="sheet-row">
          <div className="sheet-label">{t("set.autostart")}</div>
          <button
            type="button"
            role="switch"
            className="switch"
            aria-checked={settings?.autostart ?? false}
            aria-label={t("set.autostart")}
            onClick={() => setAutostart(!(settings?.autostart ?? false))}
          />
        </div>

        <div className="sheet-row">
          <div>
            <div className="sheet-label">{t("set.contact")}</div>
            <div className="sheet-hint">{t("set.contact.hint")}</div>
          </div>
          <div className="sheet-actions">
            <button
              type="button"
              className="sheet-link"
              title={CONTACT_EMAIL}
              onClick={() => void bridge.openExternal(`mailto:${CONTACT_EMAIL}`)}
            >
              {t("set.mail")}
            </button>
            <button
              type="button"
              className="sheet-link"
              onClick={() => void bridge.openExternal(RELEASE_PAGE_URL)}
            >
              Releases
            </button>
            <button
              type="button"
              className="sheet-link"
              title={t("set.logs.tip")}
              onClick={() => void bridge.openLogDir()}
            >
              {t("set.logs")}
            </button>
          </div>
        </div>

        {bridge.live ? (
          <div className="sheet-row sheet-row-danger">
            <div>
              <div className="sheet-label">{t("set.quit")}</div>
              <div className="sheet-hint">{t("set.quit.hint")}</div>
            </div>
            <button type="button" className="quit-btn" onClick={quit}>
              {t("set.quit.btn")}
            </button>
          </div>
        ) : null}

        <p className="sheet-foot num">
          <span>TokenMe v{settings?.version ?? "…"}</span>
          {update ? (
            <span className="sheet-foot-update">
              <span className={update.phase === "error" ? "update-error" : undefined}>{update.message}</span>
              {update.phase === "available" || update.phase === "downloaded" ? (
                <button
                  type="button"
                  className="sheet-action"
                  disabled={updateBusy}
                  onClick={() => void runUpdate()}
                >
                  {updateBusy
                    ? progress != null
                      ? t("set.dl", { p: Math.round(progress) })
                      : t("set.busy")
                    : update.phase === "available"
                      ? t("set.download")
                      : t("set.reboot")}
                </button>
              ) : null}
            </span>
          ) : null}
        </p>
      </div>
    </div>
  );
}

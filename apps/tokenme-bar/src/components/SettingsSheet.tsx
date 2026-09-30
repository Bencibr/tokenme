import { useEffect, useState } from "react";
import type { PanelSettings, ThemeKey, UpdateStatus } from "../types";
import { bridge, isWindows } from "../lib/bridge";
import { CONTACT_EMAIL, RELEASE_PAGE_URL } from "../lib/about";
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
 * login-item switch, and the version. Both switches apply on click —
 * there is no dirty state to save, so there is no save button.
 */
export function SettingsSheet({
  onClose,
  onMoney,
  onUnused,
}: {
  onClose: () => void;
  onMoney: (on: boolean) => void;
  onUnused: (on: boolean) => void;
}) {
  const [settings, setSettings] = useState<PanelSettings | null>(null);
  const [update, setUpdate] = useState<UpdateStatus | null>(null);
  const [updateBusy, setUpdateBusy] = useState(false);
  // 0–100 while the artifact streams in; the button reads 下载中 {n}%.
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

  const setShowMoney = (on: boolean) => {
    setSettings((s) => (s ? { ...s, show_money: on } : s));
    onMoney(on);
    void bridge.setShowMoney(on);
  };

  const setShowUnusedTools = (on: boolean) => {
    setSettings((s) => (s ? { ...s, show_unused_tools: on } : s));
    onUnused(on);
    void bridge.setShowUnusedTools(on);
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

        {isWindows ? (
          <div className="sheet-row">
            <div>
              <div className="sheet-label">边缘悬浮水滴</div>
              <div className="sheet-hint">显示今日 Token 数，靠近屏幕边缘时自动吸附</div>
            </div>
            <button
              type="button"
              role="switch"
              className="switch"
              aria-checked={settings?.bubble_enabled ?? true}
              aria-label="边缘悬浮水滴"
              onClick={() => setBubble(!(settings?.bubble_enabled ?? true))}
            />
          </div>
        ) : null}

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
          <div>
            <div className="sheet-label">显示未使用工具</div>
            <div className="sheet-hint">开启后工具页列出本期会话数为 0 的工具</div>
          </div>
          <button
            type="button"
            role="switch"
            className="switch"
            aria-checked={settings?.show_unused_tools ?? false}
            aria-label="显示未使用工具"
            onClick={() => setShowUnusedTools(!(settings?.show_unused_tools ?? false))}
          />
        </div>

        <div className="sheet-row">
          <div>
            <div className="sheet-label">退出后暂停配额</div>
            <div className="sheet-hint">工具退出后停止探测其配额，保留最后数值</div>
          </div>
          <button
            type="button"
            role="switch"
            className="switch"
            aria-checked={settings?.host_exit_pause ?? true}
            aria-label="退出后暂停配额"
            onClick={() => setHostExitPause(!(settings?.host_exit_pause ?? true))}
          />
        </div>

        <div className="sheet-row">
          <div>
            <div className="sheet-label">自动检查更新</div>
            <div className="sheet-hint">开启后启动时和每天各静默检查一次，发现新版本在底部提示</div>
          </div>
          <button
            type="button"
            role="switch"
            className="switch"
            aria-checked={settings?.auto_update_check ?? true}
            aria-label="自动检查更新"
            onClick={() => setAutoUpdateCheck(!(settings?.auto_update_check ?? true))}
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

        <div className="sheet-row">
          <div>
            <div className="sheet-label">联系我们</div>
            <div className="sheet-hint">问题反馈 · 版本发布 · 交流群见 README</div>
          </div>
          <div className="sheet-actions">
            <button
              type="button"
              className="sheet-link"
              title={CONTACT_EMAIL}
              onClick={() => void bridge.openExternal(`mailto:${CONTACT_EMAIL}`)}
            >
              发邮件
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
              title="崩溃与异常都记录在这里，反馈问题时附上"
              onClick={() => void bridge.openLogDir()}
            >
              日志
            </button>
          </div>
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
                      ? `下载中 ${Math.round(progress)}%`
                      : "处理中…"
                    : update.phase === "available"
                      ? "下载并安装"
                      : "重启到新版本"}
                </button>
              ) : null}
            </span>
          ) : null}
        </p>
      </div>
    </div>
  );
}

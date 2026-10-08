import { useEffect, useState } from "react";
import type { NotifyState, NotifyTierKey, PanelSettings, ProbeTool, ThemeKey, UpdateStatus } from "../types";
import { bridge, isWindows } from "../lib/bridge";
import { CONTACT_EMAIL, RELEASE_PAGE_URL } from "../lib/about";
import { t } from "../lib/i18n";
import { toolDisplay } from "../lib/format";
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

const TIERS: { key: NotifyTierKey; label: () => string }[] = [
  { key: "both", label: () => t("set.tiers.both") },
  { key: "exhausted", label: () => t("set.tiers.exhausted") },
  { key: "off", label: () => t("set.tiers.off") },
];

type Tab = "general" | "alert" | "advanced" | "about";
// Label keys are literals rather than a `set.tab.${k}` template so the parity
// gate (scripts/check-i18n-parity.py) can see each tab's copy is reached.
const TABS: { key: Tab; label: () => string }[] = [
  { key: "general", label: () => t("set.tab.general") },
  { key: "alert", label: () => t("set.tab.alert") },
  { key: "advanced", label: () => t("set.tab.advanced") },
  { key: "about", label: () => t("set.tab.about") },
];
type List = "probe" | "mute";

/** The engine's own name when it has one (CatPaw, not the frontend's Catpaw);
 *  the probes with no log adapter — copilot, gemini — fall back to the map. */
function nameOf(p: ProbeTool, displays: Record<string, string>): string {
  return displays[p.id] ?? toolDisplay(p.id);
}

/**
 * The panel's knobs as a bottom sheet of a fixed 65%: four tabs, and the two
 * settings whose value is a list of tools open a page inside the same sheet
 * (`›`) rather than expanding in place — an inline expansion would push the
 * sheet past 460 px of body and take the tab strip off screen with it.
 *
 * The height is written, not capped: a `max-height` sheet changes height when a
 * tab is shorter than the last, and a panel that jumps under the cursor loses
 * the muscle memory the whole sheet depends on. The footer (version · update ·
 * quit) sits outside the tabs for the same reason — those are state and a
 * global action, not a group of settings.
 */
export function SettingsSheet({
  onClose,
  onEmptyTools,
  onRefreshSecs,
  onMoney,
  onPolling,
  onOpenServers,
  displays,
}: {
  onClose: () => void;
  onEmptyTools: (on: boolean) => void;
  /** The panel times its "updates stopped" line against this cadence. */
  onRefreshSecs: (secs: number) => void;
  /** Dollar figures on/off, so the panel repaints without waiting for a report. */
  onMoney: (on: boolean) => void;
  /** The master switch, so the quota section can say it stopped immediately. */
  onPolling: (on: boolean) => void;
  /** `↗` — the server page is its own sheet; the settings sheet only opens it. */
  onOpenServers: () => void;
  displays: Record<string, string>;
}) {
  const [settings, setSettings] = useState<PanelSettings | null>(null);
  const [update, setUpdate] = useState<UpdateStatus | null>(null);
  const [updateBusy, setUpdateBusy] = useState(false);
  // 0–100 while the artifact streams in; the button reads the set.dl label.
  const [progress, setProgress] = useState<number | null>(null);
  const [tab, setTab] = useState<Tab>("general");
  const [list, setList] = useState<List | null>(null);
  const [probes, setProbes] = useState<ProbeTool[]>([]);
  const [showAll, setShowAll] = useState(false);
  const [notify, setNotify] = useState<NotifyState>("unknown");

  useEffect(() => bridge.onUpdateProgress((p) => setProgress(p.percent)), []);

  useEffect(() => {
    let alive = true;
    void bridge.panelSettings().then((s) => {
      if (!alive) return;
      setSettings(s);
      onRefreshSecs(s.refresh_secs);
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
    void bridge.notifyStatus().then((s) => alive && setNotify(s));
    void bridge.probeTools().then((p) => alive && setProbes(p));
    return () => {
      alive = false;
    };
  }, []);

  const patch = (fn: (s: PanelSettings) => PanelSettings) =>
    setSettings((s) => (s ? fn(s) : s));

  const setRefresh = (secs: number) => {
    patch((s) => ({ ...s, refresh_secs: secs }));
    onRefreshSecs(secs);
    void bridge.setRefreshSecs(secs);
  };

  // The bridge applies `data-theme` synchronously, so the pick is visible
  // before the persist round-trip answers.
  const setTheme = (theme: ThemeKey) => {
    patch((s) => ({ ...s, theme }));
    void bridge.setTheme(theme);
  };

  const setMoney = (on: boolean) => {
    patch((s) => ({ ...s, show_money: on }));
    onMoney(on);
    void bridge.setShowMoney(on);
  };

  const setShowEmptyTools = (on: boolean) => {
    patch((s) => ({ ...s, show_empty_tools: on }));
    onEmptyTools(on);
    void bridge.setShowEmptyTools(on);
  };

  const setBubble = (on: boolean) => {
    patch((s) => ({ ...s, bubble_enabled: on }));
    void bridge.setBubbleEnabled(on);
  };

  const setAutostart = (on: boolean) => {
    patch((s) => ({ ...s, autostart: on }));
    void bridge.setAutostart(on);
  };

  const setHostExitPause = (on: boolean) => {
    patch((s) => ({ ...s, host_exit_pause: on }));
    void bridge.setHostExitPause(on);
  };

  /** The master switch also repaints the per-tool page: `polling` is the
   *  composed answer, and a page that kept showing green switches after the
   *  switch above went off would be a page of lies. */
  const setQuotaPolling = (on: boolean) => {
    patch((s) => ({ ...s, quota_polling: on }));
    setProbes((ps) => ps.map((p) => ({ ...p, polling: on && !isOff(p.id) })));
    // The quota section reads this too: the switch and the "已停止探测" line must
    // answer in the same frame, or the panel looks like it ignored the click.
    onPolling(on);
    void bridge.setQuotaPolling(on);
  };

  const isOff = (id: string) => (settings?.quota_probes_off ?? []).includes(id);

  const setToolPolling = (id: string, on: boolean) => {
    patch((s) => ({
      ...s,
      quota_probes_off: on ? s.quota_probes_off.filter((t0) => t0 !== id) : [...s.quota_probes_off, id].sort(),
    }));
    setProbes((ps) => ps.map((p) => (p.id === id ? { ...p, polling: (settings?.quota_polling ?? true) && on } : p)));
    void bridge.setToolPolling(id, on);
  };

  const setTiers = (tiers: NotifyTierKey) => {
    patch((s) => ({ ...s, notify_tiers: tiers }));
    void bridge.setNotifyTiers(tiers);
  };

  const isMuted = (id: string) => (settings?.notify_muted ?? []).includes(id);

  const setToolMuted = (id: string, on: boolean) => {
    patch((s) => ({
      ...s,
      notify_muted: on ? [...new Set([...s.notify_muted, id])].sort() : s.notify_muted.filter((t0) => t0 !== id),
    }));
    void bridge.setToolMuted(id, on);
  };

  const setAutoUpdateCheck = (on: boolean) => {
    patch((s) => ({ ...s, auto_update_check: on }));
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

  /** 关于页的“立即检查”：只做版本比较。“下载并安装 / 重启”是检查结果
   *  在底栏升起的按钮，检查本身上是当前版本时绝不能换包。 */
  const checkNow = async () => {
    setUpdateBusy(true);
    try {
      const status = await bridge.checkUpdate();
      // message 为空的应答没有可渲染的行；其余（含 quiet 的失败说明）都进底栏。
      setUpdate(status.message ? status : null);
    } catch (e) {
      setUpdate({ phase: "error", message: String(e), version: null });
    } finally {
      setUpdateBusy(false);
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

  /** `›` opens a page inside this sheet; `↗` opens a different sheet. The two
   *  glyphs are the whole contract and must not be interchanged. */
  const navRow = (key: List, label: string, hint: string, count: string) => (
    <div
      className="sheet-row sheet-row-nav"
      role="button"
      tabIndex={0}
      onClick={() => setList(key)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          setList(key);
        }
      }}
    >
      <div>
        <div className="sheet-label">{label}</div>
        <div className="sheet-hint">{hint}</div>
      </div>
      <div className="sheet-nav-end">
        <span className="sheet-value">{count}</span>
        <span className="sheet-chev" aria-hidden="true">
          ›
        </span>
      </div>
    </div>
  );

  const switchRow = (
    label: string,
    hint: string | null,
    on: boolean,
    flip: (next: boolean) => void,
    disabled = false,
    tag: string | null = null,
  ) => (
    <div className={`sheet-row${disabled ? " sheet-row-locked" : ""}`}>
      <div>
        <div className="sheet-label">
          {label}
          {tag ? <span className="sheet-tag">{tag}</span> : null}
        </div>
        {hint ? <div className="sheet-hint">{hint}</div> : null}
      </div>
      <button
        type="button"
        role="switch"
        className="switch"
        aria-checked={on}
        aria-label={label}
        disabled={disabled}
        onClick={() => flip(!on)}
      />
    </div>
  );

  const offCount = settings?.quota_probes_off.length ?? 0;
  const mutedCount = settings?.notify_muted.length ?? 0;
  const local = probes.filter((p) => p.answered_here);
  const listed = showAll ? probes : local;
  const listedOn = list === "probe" ? listed.filter((p) => p.polling).length : listed.filter((p) => !isMuted(p.id)).length;
  const probeLocked = settings ? !settings.quota_polling : false;
  const muteLocked = notify !== "granted";

  const body = () => {
    if (!settings) return null;
    if (list === "probe" || list === "mute") {
      const probing = list === "probe";
      const locked = probing ? probeLocked : muteLocked;
      return (
        <>
          <div className="sheet-subhd">
            <button type="button" className="sheet-back" onClick={() => setList(null)}>
              <span aria-hidden="true">‹</span>
              {t("set.back")}
            </button>
            <h2 className="sec-label">{probing ? t("set.perprobe") : t("set.muted")}</h2>
          </div>
          {/* The count is this page's live state — under a lock nothing is live, and
              a "0 polling" line would contradict the greyed switches below it. */}
          {locked ? (
            <p className="sheet-locknote">{probing ? t("set.probe.locked") : t("set.mute.locked")}</p>
          ) : (
            <p className="sheet-summary num">
              {t(probing ? "set.probe.summary" : "set.mute.summary", {
                n: listed.length,
                on: listedOn,
                off: listed.length - listedOn,
              })}
            </p>
          )}
          <div className={`sheet-list${locked ? " sheet-list-locked" : ""}`}>
            {probing
              ? switchRow(t("set.pause"), t("set.pause.hint"), settings.host_exit_pause, setHostExitPause, locked)
              : null}
            {listed.map((p) =>
              probing
                ? switchRow(
                    nameOf(p, displays),
                    null,
                    !isOff(p.id),
                    (next) => setToolPolling(p.id, next),
                    locked,
                    // The badge is the page's one piece of real advice: these
                    // four are the probes nothing else can stop.
                    p.host_gated ? null : t("set.probe.nohost"),
                  )
                : switchRow(nameOf(p, displays), null, isMuted(p.id), (next) => setToolMuted(p.id, next), locked),
            )}
            <button type="button" className="sheet-scope" onClick={() => setShowAll((v) => !v)}>
              {showAll
                ? t("set.probe.showlocal", { n: local.length })
                : t("set.probe.showall", { n: probes.length, m: probes.length - local.length })}
            </button>
          </div>
        </>
      );
    }
    return (
      <>
        <div className="sheet-tabs" role="tablist" onKeyDown={(e) => {
          const at = TABS.findIndex((x) => x.key === tab);
          const step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
          if (!step) return;
          e.preventDefault();
          setTab(TABS[(at + step + TABS.length) % TABS.length].key);
        }}>
          {TABS.map(({ key, label }) => (
            <button
              key={key}
              type="button"
              role="tab"
              className="sheet-tab"
              aria-selected={tab === key}
              onClick={() => setTab(key)}
            >
              {label()}
            </button>
          ))}
        </div>
        {tab === "general" ? (
          <>
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
                    aria-checked={settings.refresh_secs === i.secs}
                    onClick={() => setRefresh(i.secs)}
                  >
                    {i.label()}
                  </button>
                ))}
              </div>
            </div>
            <div className="sheet-row">
              <div className="sheet-label">{t("set.theme")}</div>
              <div className="seg" role="radiogroup" aria-label={t("set.theme.a11y")}>
                {THEMES.map((th) => (
                  <button
                    key={th.key}
                    type="button"
                    role="radio"
                    className="seg-btn"
                    aria-checked={settings.theme === th.key}
                    onClick={() => setTheme(th.key)}
                  >
                    {th.label()}
                  </button>
                ))}
              </div>
            </div>
            {switchRow(t("set.money"), t("set.money.hint"), settings.show_money, setMoney)}
            {switchRow(t("set.showempty"), t("set.showempty.hint"), settings.show_empty_tools, setShowEmptyTools)}
            {isWindows ? switchRow(t("set.bubble"), t("set.bubble.hint"), settings.bubble_enabled, setBubble) : null}
          </>
        ) : null}
        {tab === "alert" ? (
          <>
            <div className="sheet-row">
              <div>
                <div className="sheet-label">{t("set.notify")}</div>
                <div className="sheet-hint">{t("set.notify.hint")}</div>
              </div>
              {notify === "granted" ? (
                <span className="sheet-status">
                  <span className="sheet-status-dot" aria-hidden="true" />
                  {t("set.notify.granted")}
                </span>
              ) : (
                <button
                  type="button"
                  className="sheet-action sheet-action-warn"
                  onClick={() => void bridge.notifyEnable().then((s) => setNotify(s))}
                >
                  {t("set.notify.enable")}
                </button>
              )}
            </div>
            <div className="sheet-block">
              <div className="sheet-label">{t("set.tiers")}</div>
              <div className="sheet-hint">{t("set.tiers.hint")}</div>
              <div className="seg seg-wide" role="radiogroup" aria-label={t("set.tiers")}>
                {TIERS.map((tr) => (
                  <button
                    key={tr.key}
                    type="button"
                    role="radio"
                    className="seg-btn"
                    aria-checked={settings.notify_tiers === tr.key}
                    onClick={() => setTiers(tr.key)}
                  >
                    {tr.label()}
                  </button>
                ))}
              </div>
            </div>
            {navRow("mute", t("set.muted"), t("set.muted.hint"), mutedCount ? t("set.muted.count", { n: mutedCount }) : t("set.none"))}
          </>
        ) : null}
        {tab === "advanced" ? (
          <>
            {switchRow(t("set.polling"), t("set.polling.hint"), settings.quota_polling, setQuotaPolling)}
            {navRow("probe", t("set.perprobe"), t("set.perprobe.hint"), offCount ? t("set.perprobe.off", { n: offCount }) : t("set.perprobe.all"))}
            {switchRow(t("set.autostart"), null, settings.autostart, setAutostart)}
            {switchRow(t("set.autoupdate"), t("set.autoupdate.hint"), settings.auto_update_check, setAutoUpdateCheck)}
            <div className="sheet-row sheet-row-nav" role="button" tabIndex={0} onClick={onOpenServers} onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                onOpenServers();
              }
            }}>
              <div>
                <div className="sheet-label">{t("set.servers")}</div>
                <div className="sheet-hint">{t("set.servers.hint")}</div>
              </div>
              <span className="sheet-chev" aria-hidden="true">
                ↗
              </span>
            </div>
          </>
        ) : null}
        {tab === "about" ? (
          <>
            <div className="sheet-row">
              <div>
                <div className="sheet-label">{t("set.version")}</div>
                <div className="sheet-hint">{t("set.version.hint", { b: settings.build })}</div>
              </div>
              <span className="sheet-value num">v{settings.version}</span>
            </div>
            <div className="sheet-row">
              <div>
                <div className="sheet-label">{t("set.check")}</div>
                <div className="sheet-hint">{t("set.check.hint")}</div>
              </div>
              <button type="button" className="sheet-action" disabled={updateBusy} onClick={() => void checkNow()}>
                {updateBusy ? t("set.busy") : t("set.check.now")}
              </button>
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
          </>
        ) : null}
      </>
    );
  };

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div
        className="sheet sheet-paged"
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
        <div className="sheet-body">{body()}</div>
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
          {bridge.live ? (
            <button type="button" className="quit-btn" onClick={quit} title={t("set.quit.hint")}>
              {t("set.quit")}
            </button>
          ) : null}
        </p>
      </div>
    </div>
  );
}

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { PageKey, PanelSettings, PeriodKey, Report, TrayMode, TrayState } from "./types";
import { bridge, inTauri, isWindows, applyTheme, applyMoney } from "./lib/bridge";
import { checkForUpdate, type UpdateInfo } from "./lib/update";
import { RELEASE_PAGE_URL } from "./lib/about";
import { DisplayCtx } from "./lib/display";
import { Loading } from "./components/Loading";
import { localDate } from "./lib/format";
import { lang, t } from "./lib/i18n";
import { useEscape, useTicker } from "./lib/hooks";
import { CallTabs } from "./components/CallTabs";
import { EmptyState } from "./components/EmptyState";
import { Header } from "./components/Header";
import { Heatmap } from "./components/Heatmap";
import { QuotaStrip } from "./components/QuotaStrip";
import { RankedList } from "./components/RankedList";
import { Sessions } from "./components/Sessions";
import { SettingsSheet } from "./components/SettingsSheet";
import { Sources } from "./components/Sources";
import { IconProvider } from "./components/ToolIcon";
import { StatusBar } from "./components/StatusBar";
import { ToolsSection } from "./components/ToolsSection";
import { BubbleApp } from "./components/BubbleApp";

/** 托盘显示模式循环顺序:仅托盘 → 仅Token → 仅花费 → 三个托盘组合。 */
const MODES: TrayMode[] = ["tray_only", "tokens_only", "cost_only", "tray_tokens", "tray_cost", "tray_tokens_cost"];

export default function App() {
  // The bubble is a runtime-created Windows window that shares this frontend
  // entrypoint. Keeping it in a separate component prevents panel hooks from
  // ever running in the utility window.
  const isBubble = inTauri && getCurrentWindow().label === "bubble";
  return isBubble ? <BubbleApp /> : <PanelApp />;
}

function PanelApp() {
  const [report, setReport] = useState<Report | null>(null);
  const [period, setPeriod] = useState<PeriodKey>("day");
  // Dev/QA affordance: ?page=ranks deep-links a page; anything unknown is overview.
const [page, setPage] = useState<PageKey>(() => {
  const p = new URLSearchParams(location.search).get("page");
  return p === "tools" || p === "ranks" || p === "detail" ? p : "overview";
});
  const scroll = useRef<HTMLElement | null>(null);
  const [tray, setTray] = useState<TrayState | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [showMoney, setShowMoney] = useState(true);
  // QA pin, same family as ?lang= / ?theme= / ?page=: freeze the zero-session
  // switch without touching persistence.
  const [showEmptyTools, setShowEmptyTools] = useState(() => {
    const pin = new URLSearchParams(location.search).get("showempty");
    return pin === "1" || pin === "true";
  });
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const tick = useTicker(30_000);

  useEffect(() => {
    // The native chrome (tray menu, tooltip, updater copy) cannot see the
    // system locale the way the webview can — hand it the verdict once.
    void bridge.setUiLang(lang);
    let alive = true;
    void bridge
      .fetchReport(false)
      .then((r) => alive && setReport(r))
      .catch((e: unknown) => alive && setError(String(e)));
    void bridge.trayState().then((s) => alive && setTray(s));
    // The theme pin and the money flag live in settings.json; applying them
    // before the first paint matters more than the rest of the sheet's state,
    // so they load at boot.
    let known: PanelSettings | null = null;
    // One quiet version check per boot plus one per day: a menu-bar panel
    // stays running for weeks, so boot-only would effectively never check.
    // Any failure stays silent. With auto_update_check on, the check runs
    // through the updater commands so the settings sheet can offer a
    // one-click download + install.
    const quietCheck = () => {
      const s = known;
      if (!s || !inTauri || s.version === "dev") return;
      if (s.auto_update_check) {
        void bridge
          .checkUpdate()
          .then((status) => {
            if (status.phase === "available" && status.version) {
              alive && setUpdate({ latest: status.version, url: RELEASE_PAGE_URL });
            }
          })
          .catch(() => {});
      } else {
        void checkForUpdate(s.version).then((u) => alive && setUpdate(u));
      }
    };
    void bridge.panelSettings().then((s) => {
      if (!alive) return;
      known = s;
      applyTheme(s.theme);
      applyMoney(s.show_money);
      setShowMoney(s.show_money);
      if (new URLSearchParams(location.search).get("showempty") === null) {
        setShowEmptyTools(s.show_empty_tools);
      }
      quietCheck();
    });
    const updateTick = window.setInterval(quietCheck, 24 * 60 * 60 * 1000);
    const un = bridge.onReport((r) => {
      setReport(r);
      setError(null);
    });
    // The tray menu's "本周"/"今日" entries open the panel already focused on
    // that period, so the numbers match the menu item that was clicked.
    const unPeriod = bridge.onTrayPeriod((p) => {
      if (p === "day" || p === "week" || p === "month") setPeriod(p);
    });
    return () => {
      alive = false;
      window.clearInterval(updateTick);
      un();
      unPeriod();
    };
  }, []);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      // Full refresh: fresh price table (the engine re-summarizes when it
      // lands), re-ingest, and — engine-side — a forced quota re-probe.
      if (inTauri) void bridge.refreshPricing();
      setReport(await bridge.fetchReport(true));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  const goPage = useCallback((next: PageKey) => setPage(next), []);
  const closePanel = useCallback(() => {
    if (inTauri) void getCurrentWindow().hide();
  }, []);
  // A new page that keeps the old scroll offset lands mid-list.
  useEffect(() => {
    scroll.current?.scrollTo(0, 0);
  }, [page]);

  const cycleTrayMode = useCallback(async () => {
    const next = MODES[(MODES.indexOf(tray?.mode ?? "tray_tokens") + 1) % MODES.length];
    setTray(await bridge.setTrayMode(next));
  }, [tray?.mode]);

  // Escape dismisses the sheet first, then the panel; focus-loss dismissal is
  // handled natively so it also works while another app holds keyboard focus.
  useEscape(useCallback(() => {
    if (settingsOpen) {
      setSettingsOpen(false);
      return;
    }
    if (inTauri) void getCurrentWindow().hide();
  }, [settingsOpen]));

  const now = useMemo(() => Date.now(), [tick]);
  const events = useMemo(() => (report ? report.sources.reduce((a, s) => a + s.events_ingested, 0) : 0), [report]);
  const isEmpty = !!report && report.sources.length > 0 && report.sources.every((s) => !s.detected);

  // The tools page lists every *detected* source, not just the ones that
  // billed this window: a credits-only tool that metered nothing here, or a
  // source whose store is unreadable, would otherwise vanish for weeks. The
  // sheet's zero-session switch hides that tail again (default off).
  // Hooks rule: this must sit above the boot screen's early return.
  const tools = useMemo(() => {
    if (!report) return [];
    const w = report[period];
    const inWindow = new Set(w.breakdown.tools.map((t) => t.key));
    const silent = report.sources
      .filter((s) => s.detected && !inWindow.has(s.id))
      .map((s) => ({
        key: s.id,
        label: s.display,
        counts: { input: 0, cache_creation: 0, cache_read: 0, output: 0, reasoning: 0, credits: 0 },
        total_tokens: 0,
        cost: 0,
        requests: 0,
        sessions: 0,
        priced: true,
      }));
    return [...w.breakdown.tools, ...silent].filter((t) => showEmptyTools || t.sessions > 0);
  }, [report, period, showEmptyTools]);

  if (!report) {
    // "indexing" is the backend sentinel for "first scan still running"; the
    // report-updated event swaps this screen out once the data lands.
    const indexing = !error || error === "indexing";
    return (
      <div className="panel" data-boot>
        <div className="boot">
          <Loading size={34} />
          <span className="boot-text">{indexing ? t("boot.indexing") : t("boot.failed", { e: error })}</span>
          {!indexing ? (
            <button type="button" className="boot-retry" onClick={() => void refresh()}>
              {t("boot.retry")}
            </button>
          ) : null}
        </div>
      </div>
    );
  }

  const win = report[period];

  return (
    <DisplayCtx.Provider value={{ money: showMoney }}>
    <IconProvider>
      <div className="panel" data-open data-page={page}>
        <Header
          report={report}
          period={period}
          onPeriod={setPeriod}
          page={page}
          onPage={goPage}
          onClose={isWindows ? closePanel : undefined}
        />

        <main className="scroll" tabIndex={-1} ref={scroll}>
          {isEmpty ? (
            <EmptyState sources={report.sources} onRefresh={() => void refresh()} />
          ) : (
            /* `key` remounts the page, which is what the entry animation needs. */
            <div className="page" key={page}>
              {page === "overview" ? (
                <>
                  <Heatmap cells={report.heatmap} today={localDate(report.generated_at_ms)} hours={report.hourly} />
                  <QuotaStrip quotas={report.quotas} now={now} pending={report.quotas_pending} />
                </>
              ) : null}
              {page === "tools" ? <ToolsSection tools={tools} /> : null}
              {page === "ranks" ? (
                <>
                  <RankedList label={t("sec.models")} items={win.breakdown.models} limit={8} unpriced={win.summary.unpriced} />
                  <RankedList label={t("sec.projects")} items={win.breakdown.projects} limit={8} />
                  <CallTabs mcps={win.breakdown.mcps} skills={win.breakdown.skills} />
                </>
              ) : null}
              {page === "detail" ? (
                <>
                  <Sessions rows={report.recent_sessions} now={now} />
                  <Sources sources={report.sources} allTime={report.all_time} />
                </>
              ) : null}
            </div>
          )}
        </main>

        <StatusBar
          pricing={report.pricing}
          events={events}
          loading={loading}
          onRefresh={() => void refresh()}
          onOpenSettings={() => setSettingsOpen(true)}
          tray={inTauri ? { mode: tray?.mode ?? "tray_tokens", onCycle: () => void cycleTrayMode() } : null}
          update={update}
        />

        {settingsOpen ? (
          <SettingsSheet
            onClose={() => setSettingsOpen(false)}
            onEmptyTools={setShowEmptyTools}
          />
        ) : null}
      </div>
    </IconProvider>
    </DisplayCtx.Provider>
  );
}

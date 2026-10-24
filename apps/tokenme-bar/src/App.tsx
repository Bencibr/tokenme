import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { MachineScope, PageKey, PanelSettings, PeriodKey, QuotaDisplayMode, Report, ServerView, SyncDevicesSnapshot, SyncRecord, TrayMode, TrayState, TtftSnapshot } from "./types";
import { bridge, inTauri, isWindows, applyTheme, applyMoney } from "./lib/bridge";
import { checkForUpdate, type UpdateInfo } from "./lib/update";
import { RELEASE_PAGE_URL } from "./lib/about";
import { DisplayCtx } from "./lib/display";
import { Loading } from "./components/Loading";
import { localDate, relativeTime } from "./lib/format";
import { lang, t } from "./lib/i18n";
import { useEscape, useTicker } from "./lib/hooks";
import { CallTabs } from "./components/CallTabs";
import { EmptyState } from "./components/EmptyState";
import { Header } from "./components/Header";
import { Heatmap } from "./components/Heatmap";
import { QuotaStrip } from "./components/QuotaStrip";
import { RankedList } from "./components/RankedList";
import { isManagedOrigin } from "./components/ScopeDropdown";
import { Sessions } from "./components/Sessions";
import { SettingsSheet } from "./components/SettingsSheet";
import { ServerSheet } from "./components/ServerSheet";
import { SyncSheet } from "./components/SyncSheet";
import { Sources } from "./components/Sources";
import { IconProvider } from "./components/ToolIcon";
import { StatusBar } from "./components/StatusBar";
import { ToolsSection } from "./components/ToolsSection";
import { BubbleApp } from "./components/BubbleApp";
import { PetNoticeApp } from "./components/PetNoticeApp";
import { PetPreview } from "./components/PetPreview";
import { SpeedPage } from "./components/SpeedPage";

/** 托盘显示模式循环顺序:仅托盘 → 仅Token → 仅花费 → 三个托盘组合。 */
const MODES: TrayMode[] = ["tray_only", "tokens_only", "cost_only", "tray_tokens", "tray_cost", "tray_tokens_cost"];

// App-level sync reads race: the boot read, the report-updated refresh and the
// sync-updated push all settle out of order, and an older read settling late
// must not clobber the newer answer already applied. This module-scoped
// monotonic sequence is the judge — a read applies `setSyncDevices` only when
// its seq is still the newest.
let syncReadSeq = 0;

export default function App() {
  // The bubble is a runtime-created Windows window that shares this frontend
  // entrypoint. Keeping it in a separate component prevents panel hooks from
  // ever running in the utility window.
  const isBubble = inTauri && getCurrentWindow().label === "bubble";
  if (!inTauri && new URLSearchParams(location.search).has("pet-preview")) return <PetPreview />;
  if ((inTauri && getCurrentWindow().label === "pet-notice") || (!inTauri && new URLSearchParams(location.search).has("pet-notice"))) return <PetNoticeApp />;
  return isBubble ? <BubbleApp /> : <PanelApp />;
}

function PanelApp() {
  const [report, setReport] = useState<Report | null>(null);
  // Dev/QA affordance: ?period=week deep-links a stats period; anything unknown
// is today. The tray's 本周/今日 entries still override it at runtime.
const [period, setPeriod] = useState<PeriodKey>(() => {
  const p = new URLSearchParams(location.search).get("period");
  return p === "week" || p === "month" || p === "year" ? p : "day";
});
  // Dev/QA affordance: ?page=ranks deep-links a page; anything unknown is overview.
const [page, setPage] = useState<PageKey>(() => {
  const p = new URLSearchParams(location.search).get("page");
  return p === "tools" || p === "ranks" || p === "detail" || p === "speed" ? p : "overview";
});
  const scroll = useRef<HTMLElement | null>(null);
  const [tray, setTray] = useState<TrayState | null>(null);
  const [loading, setLoading] = useState(false);
  // When the current manual refresh was asked for. `get_report(force)` returns
  // the still-current report at once and the fresh one arrives as a later
  // event, so without this stamp the footer would sit on the old age for the
  // whole multi-second pass with no sign the click registered. `?pending=<s>`
  // is the QA pin, same family as `?restored=`: it seeds the stamp in the past
  // so the busy chip can be inspected in a browser.
  const [refreshPending, setRefreshPending] = useState<number | null>(() => {
    const raw = new URLSearchParams(location.search).get("pending");
    if (raw === null || raw === "") return null;
    const secs = Number(raw);
    return Number.isFinite(secs) && secs >= 0 ? Date.now() - secs * 1000 : null;
  });
  const [error, setError] = useState<string | null>(null);
  // QA pin, same family as ?page= / ?theme=: open the settings sheet on load so
  // a headless screenshot can measure the tab strip and the 65% height instead
  // of a panel that never showed them.
  const [settingsOpen, setSettingsOpen] = useState(
    () => new URLSearchParams(location.search).get("sheet") === "settings",
  );
  // Remote servers: the list powers the footer icon's dot and the scope menu's
  // "manual" tags; the sheet itself only borrows it.
  const [servers, setServers] = useState<ServerView[]>([]);
  const [serversLoaded, setServersLoaded] = useState(false);
  const [serversOpen, setServersOpen] = useState(false);
  const [syncOpen, setSyncOpen] = useState(false);
  const [syncDevices, setSyncDevices] = useState<SyncDevicesSnapshot>({ primary_id: null, primary_display_name: null, primary_os: null, role: "unconfigured", primary_auto_approve_enabled: false, service_running: false, primary_starting: false, primary_data_sharing_enabled: false, service_health: null, endpoint: null, certificate_pin: null, stream_id: null, data_epoch: null, upload_complete: null, download_complete: null, last_error: null, discovery_error: null, last_connected_at_ms: null, operation_state: "idle", last_checked_at_ms: null, invite_url: null, invite_expires_at_ms: null, pairing_pending: false, discovered_primaries: [], devices: [], pairing_requests: [], primary_link_live: false });
  // The sheet's own close guard (a running install must not be dismissed) —
  // Escape asks through this ref instead of closing the sheet blind.
  const serverCloseRef = useRef<() => void>(() => {});
  // The scope dropdown's open state lives here rather than inside the header,
  // only so Escape can dismiss it before the sheet and the panel itself.
  const [scopeMenuOpen, setScopeMenuOpen] = useState(false);
  const [showMoney, setShowMoney] = useState(false);
  // The quota section must say when the numbers stopped moving because the user
  // asked them to — a frozen figure with no explanation reads as a broken panel.
  const [quotaPolling, setQuotaPolling] = useState(true);
  const [quotaDisplayMode, setQuotaDisplayMode] = useState<QuotaDisplayMode>("used");
  const [showCycleTokens, setShowCycleTokens] = useState(false);
  const [ttftSnapshot, setTtftSnapshot] = useState<TtftSnapshot>({ revision: 0, enabled: false, claude_receiver: "disabled", claude_config_status: "disabled", samples: [] });
  // QA pin, same family as ?lang= / ?theme= / ?page=: freeze the unused-tools
  // switch without touching persistence.
  const [hideUnusedTools, setHideUnusedTools] = useState(() => {
    const pin = new URLSearchParams(location.search).get("hideunused");
    return pin === "1" || pin === "true";
  });
  const [hideUnusedQuota, setHideUnusedQuota] = useState(false);
  const [hideUnusedEstimation, setHideUnusedEstimation] = useState(false);
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [refreshSecs, setRefreshSecs] = useState(30);
  // The tick re-renders the "X 秒前" stamps; its granularity follows the
  // configured cadence (floored at 5s, capped at 30s) — a 15s setting read
  // through a 30s tick is how "最后刷新" could look a minute stale.
  const tick = useTicker(Math.min(30_000, Math.max(5_000, refreshSecs * 1000)));

  useEffect(() => {
    let alive = true;
    void bridge.ttftSnapshot().then((snapshot) => { if (alive) setTtftSnapshot(snapshot); }).catch(() => {});
    const un = bridge.onTtftUpdated((snapshot) => setTtftSnapshot(snapshot));
    return () => { alive = false; un(); };
  }, []);

  // Sync state can change out-of-band: the primary's service thread pushes
  // `sync-updated` the moment a pairing request persists (or an auto-approve
  // lands), and mutations plus the recovery loop push their flips. Re-read
  // status once per event. The reads are guarded by the module-scoped
  // sequence above; an event landing while a read is in flight schedules
  // exactly one trailing re-read instead of being swallowed by the old
  // in-flight guard together with a possibly-stale winner.
  const syncReadsInFlight = useRef(0);
  const syncReadTrailing = useRef(false);
  const readSyncDevices = useCallback<(read?: Promise<SyncDevicesSnapshot>) => void>((read) => {
    if (syncReadsInFlight.current > 0) {
      if (read !== undefined) syncReadTrailing.current = true;
      return;
    }
    const seq = ++syncReadSeq;
    syncReadsInFlight.current += 1;
    void (read ?? bridge.syncDevices({ action: "status" })).then((v) => {
      if (seq === syncReadSeq) setSyncDevices(v);
    }).catch(() => {}).finally(() => {
      syncReadsInFlight.current -= 1;
      if (syncReadsInFlight.current === 0 && syncReadTrailing.current) {
        syncReadTrailing.current = false;
        window.setTimeout(() => readSyncDevices(), 0);
      }
    });
  }, []);

  // Slow fallback for the push: while a role's service is running, a 5s read
  // catches transitions that predate the event (older peers, anything the
  // runtime flips silently). Pauses while the sheet is open — the sheet has
  // its own poll loop for the state the user is looking at.
  const syncServiceLive = syncDevices.role !== "unconfigured" && syncDevices.service_running && !syncOpen;
  useEffect(() => {
    if (!syncServiceLive) return;
    const timer = window.setInterval(() => readSyncDevices(), 5000);
    return () => window.clearInterval(timer);
  }, [readSyncDevices, syncServiceLive]);

  useEffect(() => {
    // The native chrome (tray menu, tooltip, updater copy) cannot see the
    // system locale the way the webview can — hand it the verdict once.
    void bridge.setUiLang(lang);
    let alive = true;
    void bridge
      .fetchReport(false)
      .then((r) => alive && setReport(r))
      .catch((e: unknown) => alive && setError(String(e)));
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
      setQuotaPolling(s.quota_polling);
      setQuotaDisplayMode(s.quota_display_mode);
      setShowCycleTokens(s.show_cycle_tokens);
      // The idle cadence is what the "updates stopped" line measures against.
      setRefreshSecs(s.refresh_secs);
      const params = new URLSearchParams(location.search);
      if (params.get("showempty") === null) {
        setHideUnusedTools(s.hide_unused_tools);
      }
      // The ?hideunused pin freezes all three visibility mirrors: the mount
      // read would otherwise clobber what the pin just set.
      if (params.get("hideunused") === null) {
        setHideUnusedQuota(s.hide_unused_quota);
        setHideUnusedEstimation(s.hide_unused_estimation);
      }
      quietCheck();
    });
    const updateTick = window.setInterval(quietCheck, 24 * 60 * 60 * 1000);
    // get_tray_state needs a published report to answer; the mount-time read
    // loses that race ("indexing") on a cold boot, so the label would pin to
    // the default mode forever. Re-read on every report — the status bar then
    // tracks the persisted 显示模式 instead of the fallback.
    const readTray = () =>
      void bridge
        .trayState()
        .then((s) => {
          if (alive && s) setTray(s);
        })
        .catch(() => {});
    readTray();
    // Remote servers boot once and then ride the event: every install, sync
    // and edit republishes the whole list, so there is no polling here.
    void bridge
      .servers()
      .then((v) => {
        if (alive) {
          setServers(v);
          setServersLoaded(true);
        }
      })
      .catch(() => {});
    // Boot read, routed through the guarded applier: its seq is captured at
    // read start, so an event read that starts later always outranks it.
    if (alive) readSyncDevices(bridge.syncDevices());
    const unServers = bridge.onServersUpdated((v) => {
      setServers(v);
      setServersLoaded(true);
    });
    // Real-time presence: the primary's WS lifecycle pushes the moment a
    // device's socket comes up or dies; refetch immediately so the device
    // rows flip online/offline without waiting for the poll cadence.
    const unSyncWs = bridge.onSyncDevicesChanged(() => {
      void bridge.syncDevices().then((v) => {
        if (alive) setSyncDevices(v);
      }).catch(() => {});
    });
    const unSync = bridge.onSyncUpdated(readSyncDevices);
    const un = bridge.onReport((r) => {
      setReport(r);
      setError(null);
      // Device receipts are not part of Report.syncs. Refresh the local sync
      // view after an engine publish so ScopeDropdown shows the latest ACK and
      // snapshot completion instead of a stale mount-time status. Same guarded
      // applier: a sync-updated read started after this one outranks it.
      readSyncDevices(bridge.syncDevices());
      // Any publish — this refresh's result, a cadence tick, a healed engine —
      // means the figures on screen are new, so the busy readout is done.
      setRefreshPending(null);
      readTray();
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
      unServers();
      unSyncWs();
      unSync();
    };
  }, [readSyncDevices]);

  const refresh = useCallback(async () => {
    setLoading(true);
    setRefreshPending(Date.now());
    try {
      // Full refresh: fresh price table (the engine re-summarizes when it
      // lands), re-ingest, and — engine-side — a forced quota re-probe.
      if (inTauri) void bridge.refreshPricing();
      setReport(await bridge.fetchReport(true));
      // Inside Tauri that was the still-current report and the fresh one comes
      // through report-updated, which clears the stamp; a browser's fixture
      // regenerates synchronously, so the return already IS the result.
      if (!inTauri) setRefreshPending(null);
      setError(null);
    } catch (e) {
      setRefreshPending(null);
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  const goPage = useCallback((next: PageKey) => setPage(next), []);
  const closePanel = useCallback(() => {
    if (inTauri) void bridge.hidePanel();
  }, []);
  // Ask the engine to re-fold; the panel keeps rendering the last report until
  // the new one arrives, and that report's own `scope` echo is what the UI
  // labels itself with — numbers and label can never disagree.
  const switchScope = useCallback((next: MachineScope) => {
    void bridge.setReportScope(next).catch((e: unknown) => setError(String(e)));
  }, []);
  // A new page that keeps the old scroll offset lands mid-list.
  useEffect(() => {
    scroll.current?.scrollTo(0, 0);
  }, [page]);

  const cycleTrayMode = useCallback(async () => {
    const next = MODES[(MODES.indexOf(tray?.mode ?? "tray_tokens") + 1) % MODES.length];
    setTray(await bridge.setTrayMode(next));
  }, [tray?.mode]);

  // Escape dismisses the scope menu first, then the sheets in their z-order
  // (settings above servers), then the panel; the server sheet's own guard
  // refuses while an install is running. Focus-loss dismissal is handled
  // natively so it also works while another app holds keyboard focus (hence
  // the menu can also close on outside-click only inside the webview — hiding
  // the panel with it open is harmless).
  useEscape(useCallback(() => {
    if (scopeMenuOpen) {
      setScopeMenuOpen(false);
      return;
    }
    if (settingsOpen) {
      setSettingsOpen(false);
      return;
    }
    if (serversOpen) {
      serverCloseRef.current();
      return;
    }
    if (syncOpen) {
      setSyncOpen(false);
      return;
    }
    if (inTauri) void bridge.hidePanel();
  }, [settingsOpen, scopeMenuOpen, serversOpen, syncOpen]));

  // A text field can only receive keys while the window owns the keyboard, and
  // the non-activating tray panel never does on Windows. While any input or
  // textarea holds focus, ask Rust for a keyboard session; blurring hands it
  // back, so the flyout stays non-activating for every other interaction.
  useEffect(() => {
    if (!inTauri || !isWindows) return;
    const textTarget = (target: EventTarget | null): boolean =>
      target instanceof HTMLElement &&
      (target.tagName === "INPUT" || target.tagName === "TEXTAREA");
    const onFocusIn = (e: FocusEvent) => {
      if (textTarget(e.target)) void bridge.setKeyboardMode(true);
    };
    const onFocusOut = (e: FocusEvent) => {
      if (textTarget(e.target)) void bridge.setKeyboardMode(false);
    };
    document.addEventListener("focusin", onFocusIn);
    document.addEventListener("focusout", onFocusOut);
    return () => {
      document.removeEventListener("focusin", onFocusIn);
      document.removeEventListener("focusout", onFocusOut);
    };
  }, []);

  const now = useMemo(() => Date.now(), [tick]);
  // QA pin, same family as ?showempty=: pretend the last publish is `?stale=<min>`
  // minutes old, so the frozen line can be driven in a browser without waiting
  // out — or killing — a live engine.
  const stalePin = useMemo(() => {
    const mins = Number(new URLSearchParams(location.search).get("stale"));
    return Number.isFinite(mins) && mins > 0 ? mins : null;
  }, []);
  const hasIndexedUsage = !!report && (
    report.all_time.total_tokens > 0 || report.all_time.cost > 0 || report.all_time.credits > 0 ||
    report.all_time.requests > 0 || report.all_time.sessions > 0
  );
  const isEmpty = !!report && report.sources.length > 0 && report.sources.every((s) => !s.detected) && !hasIndexedUsage;

  // Cold-start telemetry, stage 2 — and the handover. The native boot note covers
  // the wait for this panel's first pixels, and it has to stay until there are
  // figures to replace it with: reporting when the bundle merely executed retired
  // it at +3.2 s while the first report landed at +8.1 s, and the recording caught
  // the sheet sitting bare in between.
  const contentReported = useRef(false);
  useEffect(() => {
    if (report && !contentReported.current) {
      contentReported.current = true;
      void bridge.reportContent();
    }
  }, [report]);

  // A dead engine looks exactly like a quiet afternoon: the numbers stop moving
  // and nothing says they stopped. That is how this panel served a frozen
  // "today" for six hours on 2026-10-04. The engine re-publishes on every cadence
  // tick, and a tick can cost a whole pass (ingest plus vendor probes, tens of
  // seconds measured here), so five cadences is the point where work-in-flight
  // cannot explain the silence any more — floored at 5 minutes, and clamped to
  // the engine's own cadence range so the line matches the timer it watches.
  const staleAfterMs = Math.max(5 * Math.min(Math.max(refreshSecs, 10), 3600) * 1000, 300_000);
  // A report restored from the previous run's snapshot carries the same figures
  // the user last saw, which is why it is worth drawing a second after launch —
  // but it is not this run's fold, and the frozen line would report the engine as
  // dead while the engine is mid-scan. `?restored=<min>` is the QA pin: it stamps
  // the on-screen report as restored and `min` minutes old, so the label and its
  // tooltip can be read in a browser without relaunching into a real restore.
  const restoredPin = useMemo(() => {
    const raw = new URLSearchParams(location.search).get("restored");
    // `Number(null)` is 0, not NaN: a missing param must mean "no pin", not
    // "0 minutes old" — that bug pinned every live report as restored at
    // "刚刚" and kept the frozen line suppressed forever.
    if (raw === null || raw === "") return null;
    const mins = Number(raw);
    return Number.isFinite(mins) && mins >= 0 ? mins : null;
  }, []);
  const restored = useMemo(() => {
    if (!report) return null;
    if (!(report.from_previous_run || restoredPin !== null)) return null;
    const at = restoredPin === null ? report.generated_at_ms : Date.now() - restoredPin * 60_000;
    // Kept through a manual refresh too: the numbers on screen are still last
    // run's until the new report replaces them, so the label stays true.
    return { at };
  }, [report, restoredPin]);

  const frozen = useMemo(() => {
    if (!report || loading || restored) return null;
    const publishedAt = stalePin === null ? report.generated_at_ms : now - stalePin * 60_000;
    if (now - publishedAt <= staleAfterMs) return null;
    return { ago: relativeTime(publishedAt, now), afterSecs: Math.round(staleAfterMs / 1000) };
  }, [report, now, loading, stalePin, staleAfterMs, restored]);

  // The scope the numbers cover, exactly as the report echoes it. The UI never
  // guesses ahead of a switch: until the engine's re-folded report arrives,
  // every label keeps describing the numbers actually on screen.
  const scope = useMemo<MachineScope>(() => report?.scope ?? { kind: "all" }, [report]);

  // Machine sync (a Linux collector's bundles merged in by the engine): a
  // broken sync otherwise reads exactly like a quiet collector — the same
  // lesson as the frozen badge. Newest import wins the line; anything past
  // the threshold turns warning-hued. `?sync=<hours>` ages the newest import,
  // same QA-pin family as `?stale=`, so the warn state is drivable in a
  // browser without breaking a real sync.
  const syncPin = useMemo(() => {
    const hours = Number(new URLSearchParams(location.search).get("sync"));
    return Number.isFinite(hours) && hours > 0 ? hours : null;
  }, []);
  // The scope menu and the sync badge compare indexed origins with configured
  // SSH servers; an indexed origin can be historical/manual data even when
  // this set is empty. Hooks rule: above the boot screen's early return.
  const serverOrigins = useMemo(
    () => (serversLoaded ? new Set(servers.map((s) => s.name)) : undefined),
    [servers, serversLoaded],
  );
  const sync = useMemo(() => {
    // Only a machine that exists right now — a paired device or a configured
    // server — earns the badge. The index also remembers bundles from machines
    // long since removed, and "no data today" about one of those is noise:
    // same rule as the scope menu, so the badge never names a machine the
    // menu does not list.
    // Device-sync merges never write Report.syncs records (that list is the
    // legacy Linux-collector import log), so the paired machines themselves
    // are the freshness source for their origins: on the primary each paired
    // secondary carries its last merge, and on the secondary the primary's
    // last contact stands in for its downlink. Legacy records win only when
    // they are newer for the same origin.
    const deviceRecords: SyncRecord[] = [];
    if (syncDevices.role === "primary") {
      for (const device of syncDevices.devices) {
        if (device.status !== "paired" || device.last_merged_at_ms == null) continue;
        deviceRecords.push({
          origin: device.origin, file: "device-sync", rows: 0,
          imported_at_ms: device.last_merged_at_ms,
          window_lo_ms: device.last_merged_at_ms, window_hi_ms: device.last_merged_at_ms + 1,
        });
      }
    } else if (syncDevices.role === "secondary") {
      const origin = syncDevices.primary_display_name?.trim();
      if (origin && syncDevices.last_connected_at_ms != null) {
        deviceRecords.push({
          origin, file: "device-sync", rows: 0,
          imported_at_ms: syncDevices.last_connected_at_ms,
          window_lo_ms: syncDevices.last_connected_at_ms, window_hi_ms: syncDevices.last_connected_at_ms + 1,
        });
      }
    }
    const byOrigin = new Map<string, SyncRecord>();
    for (const r of deviceRecords) byOrigin.set(r.origin, r);
    for (const r of report?.syncs ?? []) {
      const existing = byOrigin.get(r.origin);
      if (!existing || r.imported_at_ms > existing.imported_at_ms) byOrigin.set(r.origin, r);
    }
    const all = [...byOrigin.values()]
      .filter((r) => isManagedOrigin(r.origin, syncDevices.devices, serverOrigins, syncDevices.role === "secondary"))
      .sort((a, b) => b.imported_at_ms - a.imported_at_ms);
    // The badge says whose numbers these are: under a machine scope it reports
    // that machine's merge only, and the local scope has no import to report.
    const records = scope.kind === "origin" ? all.filter((r) => r.origin === scope.name) : scope.kind === "local" ? [] : all;
    if (records.length === 0) return null;
    const importedAt = syncPin === null ? records[0].imported_at_ms : now - syncPin * 3_600_000;
    // A bundle can be imported today while containing only historical rows.
    // Use the report's per-origin total as the source of truth for whether
    // that machine actually has usage today; the import timestamp alone only
    // proves that the merge completed.
    const latestOrigin = records[0].origin;
    const todayTokens = report?.machines?.find((m) => m.origin === latestOrigin)?.today_tokens;
    const hasTodayData = todayTokens === undefined ? localDate(importedAt) === localDate(now) : todayTokens > 0;
    const mergedToday = localDate(importedAt) === localDate(now) && hasTodayData;
    return {
      latest: records[0],
      importedAt,
      age: relativeTime(importedAt, now),
      more: records.length - 1,
      mergedToday,
      stale: !mergedToday,
      rows: records.map((r, i) => ({
        origin: r.origin,
        file: r.file,
        rows: r.rows,
        // The manifest window is [lo, hi); the inclusive last day reads better.
        window: `${localDate(r.window_lo_ms)} → ${localDate(r.window_hi_ms - 1)}`,
        age: i === 0 ? relativeTime(importedAt, now) : relativeTime(r.imported_at_ms, now),
      })),
    };
  }, [report, now, syncPin, scope, syncDevices, serverOrigins]);

  // The tools page lists every *detected* source, not just the ones that
  // billed this window: a credits-only tool that metered nothing here, or a
  // source whose store is unreadable, would otherwise vanish for weeks. The
  // sheet's zero-session switch hides that tail again (default off).
  // Hooks rule: this must sit above the boot screen's early return.
  const tools = useMemo(() => {
    if (!report) return [];
    const w = report[period];
    const inWindow = new Set(w.breakdown.tools.map((t) => t.key));
    // Under an origin scope the tail would lie: the window lists only that
    // machine's tools, so every local-only tool would read as "zero sessions"
    // when the scope simply excludes it. The window's own rows never filter.
    const silent =
      scope.kind === "origin"
        ? []
        : report.sources
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
    return [...w.breakdown.tools, ...silent].filter((t) => !hideUnusedTools || t.sessions > 0);
  }, [report, period, hideUnusedTools, scope]);

  // The period's active tools (sessions > 0) — the same predicate the tools
  // page filters on. The quota strip hides the rest while the zero-session
  // switch is off: tools without sessions here are the "not in use" set.
  // Hooks rule: above the boot screen's early return.
  const usedTools = useMemo(() => {
    if (!report) return new Set<string>();
    const w = report[period];
    return new Set(w.breakdown.tools.filter((t) => t.sessions > 0).map((t) => t.key));
  }, [report, period]);

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
          sync={sync}
          scope={scope}
          onScope={switchScope}
          scopeMenuOpen={scopeMenuOpen}
          onScopeMenuOpen={setScopeMenuOpen}
          serverOrigins={serverOrigins}
          syncDevices={syncDevices}
          now={now}
        />

        <main className="scroll" tabIndex={-1} ref={scroll}>
          {isEmpty ? (
            loading ? (
              /* The rescan hides everything and shows just the spinner; when
                 the fresh report lands the same test renders the hero again —
                 empty or not. */
              <div className="empty">
                <div className="empty-rescan">
                  <Loading size={34} label={t("empty.scanning")} />
                </div>
              </div>
            ) : (
              <EmptyState onRefresh={() => void refresh()} />
            )
          ) : (
            /* `key` remounts the page, which is what the entry animation needs. */
            <div className="page" key={page}>
              {page === "overview" ? (
                <>
                  <Heatmap cells={report.heatmap} today={localDate(report.generated_at_ms)} hours={report.hourly} hoursPrev={report.hourly_prev} period={period} />
                  <QuotaStrip quotas={report.quotas} now={now} pending={report.quotas_pending} scoped={scope.kind !== "all"} polling={quotaPolling} displayMode={quotaDisplayMode} showCycleTokens={showCycleTokens} usedTools={usedTools} hideUnusedQuota={hideUnusedQuota} hideUnusedEstimation={hideUnusedEstimation} />
                </>
              ) : null}
              {page === "tools" ? <ToolsSection tools={tools} /> : null}
              {page === "ranks" ? (
                <>
                  <RankedList label={t("sec.tools")} items={win.breakdown.tools} limit={8} weight="tokens" />
                  <RankedList label={t("sec.models")} items={win.breakdown.models} limit={8} unpriced={win.summary.unpriced} />
                  <RankedList label={t("sec.projects")} items={win.breakdown.projects} limit={8} />
                  <CallTabs mcps={win.breakdown.mcps} skills={win.breakdown.skills} />
                </>
              ) : null}
              {page === "detail" ? (
                <>
                  <Sessions rows={report.recent_sessions} now={now} />
                  <Sources sources={report.sources} allTime={report.all_time} scope={scope} />
                </>
              ) : null}
              {page === "speed" ? <SpeedPage snapshot={ttftSnapshot} now={now} scope={scope} /> : null}
            </div>
          )}
        </main>

        <StatusBar
          pricing={report.pricing}
          loading={loading}
          onRefresh={() => void refresh()}
          onOpenSettings={() => setSettingsOpen(true)}
          onOpenServers={() => setServersOpen(true)}
          onOpenSync={() => setSyncOpen(true)}
          servers={servers}
          tray={inTauri ? { mode: tray?.mode ?? "tray_tokens", onCycle: () => void cycleTrayMode() } : null}
          update={update}
          restored={restored}
          publishedAt={report.generated_at_ms}
          refreshSecs={refreshSecs}
          pendingSince={refreshPending}
          frozen={frozen}
        />

        {settingsOpen ? (
          <SettingsSheet
            onClose={() => setSettingsOpen(false)}
            onEmptyTools={setHideUnusedTools}
            onRefreshSecs={setRefreshSecs}
            onMoney={setShowMoney}
            onPolling={setQuotaPolling}
            onQuotaDisplayMode={setQuotaDisplayMode}
            onShowCycleTokens={setShowCycleTokens}
            onQuotaHideUnused={setHideUnusedQuota}
            onCycleHideUnusedEstimation={setHideUnusedEstimation}
            displays={Object.fromEntries(report.sources.map((s) => [s.id, s.display]))}
          />
        ) : null}

        {serversOpen ? (
          <ServerSheet
            servers={servers}
            onServers={setServers}
            onClose={() => setServersOpen(false)}
            closeRef={serverCloseRef}
          />
        ) : null}

        {syncOpen ? (
          <SyncSheet
            snapshot={syncDevices}
            onSnapshot={setSyncDevices}
            onClose={() => setSyncOpen(false)}
          />
        ) : null}
      </div>
    </IconProvider>
    </DisplayCtx.Provider>
  );
}

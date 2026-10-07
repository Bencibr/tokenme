import { useMemo } from "react";
import type { PricingMeta, ServerStatus, ServerView, TrayMode } from "../types";
import { bridge } from "../lib/bridge";
import { clockStamp, freshness, relativeTime, TRAY_MODE_LABEL } from "../lib/format";
import { useTicker } from "../lib/hooks";
import { t } from "../lib/i18n";
import type { UpdateInfo } from "../lib/update";
import { IconRefresh, IconServer, IconSettings, IconWarn } from "./Icons";

/** One dot for the whole fleet: the most actionable state wins. */
function aggServerStatus(servers: ServerView[]): ServerStatus {
  for (const s of ["syncing", "error", "warn", "ok"] as const) {
    if (servers.some((v) => v.status === s)) return s;
  }
  return "none";
}

/**
 * The footer corner that answers "when are these numbers from" — one slot, and
 * the state that owns it says which. It keeps its own 1 s tick — the app's 30 s
 * one is sized for quota countdowns — so a manual refresh visibly restarts the
 * clock at "just now". Precedence, highest first: a restored first report (the
 * accent chip; those figures are older than the moment they are being read as),
 * a manual pass that has not published yet (the busy label rather than the old
 * age), an engine that has stopped publishing (the amber chip with the last
 * pass's age), and finally the live clock. The last two are the same fact
 * pointing opposite ways and never show together.
 */
function Freshness({
  publishedAt,
  restored,
  pendingSince,
  refreshSecs,
  frozen,
}: {
  publishedAt: number;
  restored?: { at: number } | null;
  pendingSince: number | null;
  refreshSecs: number;
  frozen?: { ago: string; afterSecs: number } | null;
}) {
  const tick = useTicker(1000);
  const now = useMemo(() => Date.now(), [tick]);
  if (restored) {
    return (
      <span className="restored" title={t("status.restored.tip", { a: relativeTime(restored.at, now) })}>
        <IconRefresh size={11} />
        {t("status.restored")}
        <span className="dot-sep" aria-hidden="true" />
        <span className="num">{relativeTime(restored.at, now)}</span>
      </span>
    );
  }
  // A manual pass runs for tens of seconds and the invoke returns the old
  // report at once, so without this the age would sit there with no sign the
  // click registered. The 120 s cap is a backstop: a pass that never publishes
  // (engine restarting) must not pin the busy label forever — the frozen line
  // is the honest signal for that, and it must be able to reappear.
  if (pendingSince !== null && now - pendingSince < 120_000) {
    return (
      <span className="fresh busy">
        <IconRefresh size={11} />
        {t("status.refreshing")}
      </span>
    );
  }
  // The engine stopped publishing: the age stops being a freshness and becomes
  // the size of the lie the figures would be telling, so it wears the warning
  // hue instead of the muted clock. Same slot as the clock because both answer
  // "when were these from" — the answer here is "19 min ago and nothing since".
  if (frozen) {
    return (
      <span className="frozen" title={t("status.frozen.tip", { s: frozen.afterSecs })}>
        <IconWarn size={11} />
        {t("status.frozen")}
        <span className="dot-sep" aria-hidden="true" />
        <span className="num">{frozen.ago}</span>
      </span>
    );
  }
  return (
    <span className="fresh" title={t("status.fresh.tip", { time: clockStamp(publishedAt, now), secs: refreshSecs })}>
      {t("status.fresh")}
      <span className="dot-sep" aria-hidden="true" />
      <span className="num">{freshness(publishedAt, now)}</span>
    </span>
  );
}

/** Persistent status line: what the numbers on screen are, and the refresh control. */
export function StatusBar({
  pricing,
  loading,
  onRefresh,
  onOpenSettings,
  onOpenServers,
  servers,
  tray,
  update,
  restored,
  publishedAt,
  pendingSince,
  refreshSecs,
  frozen,
}: {
  pricing: PricingMeta;
  loading: boolean;
  onRefresh: () => void;
  onOpenSettings: () => void;
  onOpenServers: () => void;
  /** The fleet behind the server icon; the dot aggregates one state for all. */
  servers: ServerView[];
  /** Present only inside Tauri: a browser has no menu-bar title to configure. */
  tray?: { mode: TrayMode; onCycle: () => void } | null;
  /** A newer release answered the manifest; the chip opens its download page. */
  update?: UpdateInfo | null;
  /** The figures above are last run's, restored off disk while this one scans. */
  restored?: { at: number } | null;
  /** `generated_at_ms` of the report on screen: the footer clock's source. */
  publishedAt: number;
  /** When the in-flight manual refresh was asked for, if one is. */
  pendingSince: number | null;
  /** The engine's idle cadence, quoted in the readout's tooltip. */
  refreshSecs: number;
  /** The engine stopped publishing: set once the last report ages past the
   *  cadence line `App.tsx` draws. Null while the numbers are still live. */
  frozen?: { ago: string; afterSecs: number } | null;
}) {
  return (
    <footer className="status">
      <span className="status-left">
        {/* The corner that carries what the reader must know about the numbers, and
            nothing else. The price table's provenance used to live here — models.dev
            with its key count — which is a fact about the cost column's source, not
            about the panel's state, and it read the same on almost every machine.
            One slot answers "when are these from": the live clock, which is also how
            a manual refresh shows that it landed; the busy label while that pass is
            in flight; a restored first report, because the figures are older than
            the moment they are being read as; and the amber stopped-publishing chip
            when the engine has gone silent — the clock and the chip are the same
            fact pointing opposite ways, so they never show together. A stale price
            table is a different fact and keeps its own chip beside them. */}
        <Freshness
          publishedAt={publishedAt}
          restored={restored}
          pendingSince={pendingSince}
          refreshSecs={refreshSecs}
          frozen={frozen}
        />
        {pricing.stale ? (
          <span className="stale" title={t("status.stale.tip")}>
            <IconWarn size={11} />
            {t("status.stale")}
          </span>
        ) : null}
      </span>
      <span className="status-right">
        {update ? (
          <button
            type="button"
            className="update-chip num"
            title={t("status.update.go", { v: update.latest })}
            onClick={() => void bridge.openExternal(update.url)}
          >
            {t("status.update.chip", { v: update.latest })}
          </button>
        ) : null}
        {tray ? (
          <button type="button" className="tray-mode" onClick={tray.onCycle} title={t("status.tray.tip")}>
            {TRAY_MODE_LABEL[tray.mode]}
          </button>
        ) : null}
        <button
          type="button"
          className="icon-btn"
          data-status={aggServerStatus(servers)}
          onClick={onOpenServers}
          title={servers.length ? t("srv.title.count", { n: servers.length }) : t("srv.title")}
          aria-label={t("srv.title")}
        >
          <IconServer size={15} />
          <span className="srv-dot" aria-hidden="true" />
        </button>
        <button type="button" className="settings-btn" onClick={onOpenSettings} title={t("status.settings")} aria-label={t("status.settings")}>
          <IconSettings size={15} />
        </button>
        <button type="button" className="refresh" onClick={onRefresh} disabled={loading}>
          <IconRefresh size={12} />
          {loading ? t("status.refreshing") : t("status.refresh")}
        </button>
      </span>
    </footer>
  );
}

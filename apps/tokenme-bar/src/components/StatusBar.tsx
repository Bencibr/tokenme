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
 * The footer's clock: when the numbers on screen were last recomputed. It keeps
 * its own 1 s tick — the app's 30 s one is sized for quota countdowns — so a
 * manual refresh visibly restarts it at "just now", and the seconds tier stays
 * honest. A restored first report keeps the accent chip instead (those figures
 * are older than the moment they are being read as); a manual pass that has not
 * published yet shows the busy label rather than the old age; the tooltip
 * always carries the absolute stamp plus the engine's cadence.
 */
function Freshness({
  publishedAt,
  restored,
  pendingSince,
  refreshSecs,
}: {
  publishedAt: number;
  restored?: { at: number } | null;
  pendingSince: number | null;
  refreshSecs: number;
}) {
  const tick = useTicker(1000);
  const now = useMemo(() => Date.now(), [tick]);
  if (restored) {
    return (
      <span className="restored" title={t("hdr.restored.tip", { a: relativeTime(restored.at, now) })}>
        <IconRefresh size={11} />
        {t("hdr.restored")}
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
}) {
  return (
    <footer className="status">
      <span className="status-left">
        {/* The corner that carries what the reader must know about the numbers, and
            nothing else. The price table's provenance used to live here — models.dev
            with its key count — which is a fact about the cost column's source, not
            about the panel's state, and it read the same on almost every machine.
            Three things earn this space: the freshness clock, which is also how a
            manual refresh shows that it landed; a restored first report, because the
            figures are older than the moment they are being read as; and a price
            table that went stale because no fetch succeeded. */}
        <Freshness
          publishedAt={publishedAt}
          restored={restored}
          pendingSince={pendingSince}
          refreshSecs={refreshSecs}
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

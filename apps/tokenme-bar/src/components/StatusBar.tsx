import type { PricingMeta, ServerStatus, ServerView, TrayMode } from "../types";
import { bridge } from "../lib/bridge";
import { TRAY_MODE_LABEL, count, relativeTime } from "../lib/format";
import { t } from "../lib/i18n";
import type { UpdateInfo } from "../lib/update";
import { IconRefresh, IconServer, IconSettings, IconWarn } from "./Icons";

const SOURCE_KEY: Record<PricingMeta["source"], Parameters<typeof t>[0] | "models_dev"> = {
  models_dev: "models_dev",
  cache: "models_dev",
  bundled: "status.source.bundled",
  unavailable: "status.source.none",
};
// A fresh fetch and a snapshot read are the same thing to the reader: models.dev
// prices, TTL-cached like any other price table. Only the two states that change
// what a cost number *means* — the snapshot baked into the bundle, and no prices
// at all — get their own word. The provenance that used to sit here went to the
// tooltip, because the footer is the panel's scarcest horizontal space.
const sourceLabel = (k: PricingMeta["source"]): string => (SOURCE_KEY[k] === "models_dev" ? "models.dev" : t(SOURCE_KEY[k]));
const sourceTitle = (p: PricingMeta): string | undefined =>
  p.source === "cache"
    ? t("status.source.cache.tip", { ago: relativeTime(p.fetched_at_ms, Date.now()) })
    : p.source === "models_dev"
      ? t("status.source.fresh.tip")
      : undefined;

/** One dot for the whole fleet: the most actionable state wins. */
function aggServerStatus(servers: ServerView[]): ServerStatus {
  for (const s of ["syncing", "error", "warn", "ok"] as const) {
    if (servers.some((v) => v.status === s)) return s;
  }
  return "none";
}

/** Persistent status line: where the prices came from, and the refresh control. */
export function StatusBar({
  pricing,
  loading,
  onRefresh,
  onOpenSettings,
  onOpenServers,
  servers,
  tray,
  update,
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
}) {
  return (
    <footer className="status">
      <span className="status-left">
        <span className="badge" data-source={pricing.source} title={sourceTitle(pricing)}>
          {sourceLabel(pricing.source)}
        </span>
        <span className="num">{count(pricing.key_count)}</span>
        <span>{t("status.prices")}</span>
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

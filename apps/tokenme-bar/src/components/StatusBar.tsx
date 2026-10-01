import type { PricingMeta, TrayMode } from "../types";
import { bridge } from "../lib/bridge";
import { TRAY_MODE_LABEL, count } from "../lib/format";
import { t } from "../lib/i18n";
import type { UpdateInfo } from "../lib/update";
import { IconRefresh, IconSettings, IconWarn } from "./Icons";

const SOURCE_KEY: Record<PricingMeta["source"], Parameters<typeof t>[0] | "models_dev"> = {
  models_dev: "models_dev",
  cache: "status.source.cache",
  bundled: "status.source.bundled",
  unavailable: "status.source.none",
};
const sourceLabel = (k: PricingMeta["source"]): string => (SOURCE_KEY[k] === "models_dev" ? "models.dev" : t(SOURCE_KEY[k]));

/** Persistent status line: where the prices came from, how much is indexed, refresh. */
export function StatusBar({
  pricing,
  events,
  loading,
  onRefresh,
  onOpenSettings,
  tray,
  update,
}: {
  pricing: PricingMeta;
  events: number;
  loading: boolean;
  onRefresh: () => void;
  onOpenSettings: () => void;
  /** Present only inside Tauri: a browser has no menu-bar title to configure. */
  tray?: { mode: TrayMode; onCycle: () => void } | null;
  /** A newer release answered the manifest; the chip opens its download page. */
  update?: UpdateInfo | null;
}) {
  return (
    <footer className="status">
      <span className="status-left">
        <span className="badge" data-source={pricing.source}>
          {sourceLabel(pricing.source)}
        </span>
        <span className="num">{count(pricing.key_count)}</span>
        <span>{t("status.prices")}</span>
        <span className="dot-sep" aria-hidden="true" />
        <span className="num">{count(events)}</span>
        <span>{t("status.events")}</span>
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

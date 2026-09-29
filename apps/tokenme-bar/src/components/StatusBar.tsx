import type { PricingMeta, TrayMode } from "../types";
import { bridge } from "../lib/bridge";
import { TRAY_MODE_LABEL, count } from "../lib/format";
import type { UpdateInfo } from "../lib/update";
import { IconRefresh, IconSettings, IconWarn } from "./Icons";

const SOURCE_LABEL: Record<PricingMeta["source"], string> = {
  models_dev: "models.dev",
  cache: "缓存",
  bundled: "内置快照",
  unavailable: "无价格源",
};

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
          {SOURCE_LABEL[pricing.source]}
        </span>
        <span className="num">{count(pricing.key_count)}</span>
        <span>价格</span>
        <span className="dot-sep" aria-hidden="true" />
        <span className="num">{count(events)}</span>
        <span>事件</span>
        {pricing.stale ? (
          <span className="stale" title="价格快照超过 24 小时未更新，成本为估算值">
            <IconWarn size={11} />
            价格已过期
          </span>
        ) : null}
      </span>
      <span className="status-right">
        {update ? (
          <button
            type="button"
            className="update-chip num"
            title={`前往下载 ${update.latest}`}
            onClick={() => void bridge.openExternal(update.url)}
          >
            新版本 v{update.latest}
          </button>
        ) : null}
        {tray ? (
          <button type="button" className="tray-mode" onClick={tray.onCycle} title="菜单栏显示内容">
            {TRAY_MODE_LABEL[tray.mode]}
          </button>
        ) : null}
        <button type="button" className="settings-btn" onClick={onOpenSettings} title="设置" aria-label="设置">
          <IconSettings size={15} />
        </button>
        <button type="button" className="refresh" onClick={onRefresh} disabled={loading}>
          <IconRefresh size={12} />
          {loading ? "刷新中" : "刷新"}
        </button>
      </span>
    </footer>
  );
}

import type { SourceStatus } from "../types";
import { toolDisplay } from "../lib/format";
import { t } from "../lib/i18n";
import { IconRefresh } from "./Icons";

/** Nothing on this machine is readable yet — say exactly what to install. */
export function EmptyState({ sources, onRefresh }: { sources: SourceStatus[]; onRefresh: () => void }) {
  return (
    <div className="empty">
      <p className="empty-title">{t("empty.title")}</p>
      <p className="empty-body">{t("empty.body")}</p>
      <ul className="empty-list">
        {sources.map((s) => (
          <li key={s.id}>
            <span className="empty-name">{toolDisplay(s.id)}</span>
            <span className="empty-path">{s.roots[0] ?? "~"}</span>
          </li>
        ))}
      </ul>
      <button type="button" className="empty-cta" onClick={onRefresh}>
        <IconRefresh size={12} />
        {t("empty.cta")}
      </button>
    </div>
  );
}

import { t } from "../lib/i18n";
import { IconSortAsc, IconSortDefault, IconSortDesc } from "./Icons";

export type SortState = "default" | "asc" | "desc";

const META: Record<SortState, { title: () => string; label: () => string }> = {
  default: { title: () => t("sort.title.default"), label: () => t("sort.default") },
  asc: { title: () => t("sort.title.asc"), label: () => t("sort.asc") },
  desc: { title: () => t("sort.title.desc"), label: () => t("sort.desc") },
};

/** 用量排序三态开关:默认 ⇄ 升序 ⇄ 降序,点击循环。 */
export function SortToggle({ state, onCycle }: { state: SortState; onCycle: () => void }) {
  const Icon = state === "asc" ? IconSortAsc : state === "desc" ? IconSortDesc : IconSortDefault;
  const { title, label } = META[state];
  return (
    <button type="button" className="sort-toggle" title={title()} aria-label={t("sort.a11y", { l: label() })} onClick={onCycle}>
      <Icon size={12} />
    </button>
  );
}

/** 点击推进三态循环:默认 → 升序 → 降序 → 默认。 */
export function nextSortState(state: SortState): SortState {
  return state === "default" ? "asc" : state === "asc" ? "desc" : "default";
}

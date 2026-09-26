import { IconSortAsc, IconSortDefault, IconSortDesc } from "./Icons";

export type SortState = "default" | "asc" | "desc";

const META: Record<SortState, { title: string; label: string }> = {
  default: { title: "默认排序", label: "默认" },
  asc: { title: "按用量升序", label: "升序" },
  desc: { title: "按用量降序", label: "降序" },
};

/** 用量排序三态开关:默认 ⇄ 升序 ⇄ 降序,点击循环。 */
export function SortToggle({ state, onCycle }: { state: SortState; onCycle: () => void }) {
  const Icon = state === "asc" ? IconSortAsc : state === "desc" ? IconSortDesc : IconSortDefault;
  const { title, label } = META[state];
  return (
    <button type="button" className="sort-toggle" title={title} aria-label={`排序：${label}`} onClick={onCycle}>
      <Icon size={12} />
    </button>
  );
}

/** 点击推进三态循环:默认 → 升序 → 降序 → 默认。 */
export function nextSortState(state: SortState): SortState {
  return state === "default" ? "asc" : state === "asc" ? "desc" : "default";
}

import { IconArrowDown, IconArrowUp } from "./Icons";

/** 用量排序开关:降序(默认)⇄ 升序。区头右侧的小图标按钮。 */
export function SortToggle({ asc, onToggle }: { asc: boolean; onToggle: () => void }) {
  const Icon = asc ? IconArrowUp : IconArrowDown;
  return (
    <button
      type="button"
      className="sort-toggle"
      title={asc ? "按用量升序" : "按用量降序"}
      aria-label={`按用量${asc ? "升" : "降"}序`}
      onClick={onToggle}
    >
      <Icon size={12} />
    </button>
  );
}

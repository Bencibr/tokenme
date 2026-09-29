import type { SourceStatus } from "../types";
import { toolDisplay } from "../lib/format";
import { IconRefresh } from "./Icons";

/** Nothing on this machine is readable yet — say exactly what to install. */
export function EmptyState({ sources, onRefresh }: { sources: SourceStatus[]; onRefresh: () => void }) {
  return (
    <div className="empty">
      <p className="empty-title">未检测到任何数据源</p>
      <p className="empty-body">
        tokenme 只读本机已有的人工智能命令行日志并统计用量计数。安装下列任意一个并运行一次会话后，点右下角刷新即可看到数据。
      </p>
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
        重新扫描
      </button>
    </div>
  );
}

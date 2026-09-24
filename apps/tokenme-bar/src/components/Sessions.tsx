import { useState } from "react";
import type { SessionRow } from "../types";
import { compactTokens, count, money, relativeTime } from "../lib/format";
import { MoreRow, Section } from "./Section";
import { ToolIcon } from "./ToolIcon";

const BASENAME = (p: string) => p.replace(/\/+$/, "").split("/").pop() || p;

/** Six rows by default — "what did I just spend on", not a session browser — with
 *  the tail reachable rather than silently cut off. */
export function Sessions({ rows, now }: { rows: SessionRow[]; now: number }) {
  const [all, setAll] = useState(false);
  const shown = all ? rows : rows.slice(0, 6);
  return (
    <Section label="最近会话" meta={rows.length > shown.length ? `显示 ${shown.length} / ${rows.length}` : undefined}>
      {shown.length === 0 ? (
        <p className="empty-row">还没有会话记录</p>
      ) : (
        <ol className="sess-list">
          {shown.map((s) => {
            const project = s.project ? BASENAME(s.project) : s.session.slice(0, 8);
            const billed = s.cost > 0 ? money(s.cost) : s.total_tokens === 0 ? null : "无价格";
            return (
              <li className="sess" key={`${s.tool}/${s.session}`}>
                <ToolIcon tool={s.tool} size={20} />
                <div className="sess-main">
                  <div className="sess-line">
                    <span className="sess-name" title={s.project ?? s.session}>
                      {project}
                    </span>
                    <span
                      className="sess-cost num"
                      data-unpriced={billed === "无价格" || undefined}
                      data-credit={billed === null || undefined}
                      title={billed === null ? "credits 计量，不计入美元" : undefined}
                    >
                      {billed ?? "credits"}
                    </span>
                  </div>
                  <div className="sess-meta">
                    <span className="sess-model" title={s.model}>
                      {s.model}
                    </span>
                    <span className="dot-sep" aria-hidden="true" />
                    <span className="num">{count(s.requests)}</span>
                    <span>次</span>
                    <span className="dot-sep" aria-hidden="true" />
                    <span className="num">{compactTokens(s.total_tokens)}</span>
                    <span className="grow" />
                    <span className="sess-when">{relativeTime(s.last_ms, now)}</span>
                  </div>
                </div>
              </li>
            );
          })}
        </ol>
      )}
      <MoreRow
        open={all}
        total={rows.length}
        preview={6}
        onToggle={() => setAll((v) => !v)}
        closedLabel={`查看全部 ${rows.length} 个会话`}
        openLabel="只看最近 6 个会话"
      />
    </Section>
  );
}

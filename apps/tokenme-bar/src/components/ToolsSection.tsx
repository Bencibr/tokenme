import type { Item } from "../types";
import {
  cachedOf,
  compactTokens,
  count,
  credits as creditText,
  money,
  percent,
  toolColor,
  toolDisplay,
} from "../lib/format";
import { Section } from "./Section";
import { ToolIcon } from "./ToolIcon";

/**
 * The product's core differentiator: one row per tool, the same three numerals
 * in the same columns every time, so cross-tool comparison is a single glance.
 * The badge is the tool's own app icon where one is installed, which is what
 * lets twelve rows be scanned by shape instead of read one name at a time.
 * The badge is the tool's own app icon where one is installed, which is what
 * lets twelve rows be scanned by shape instead of read one name at a time.
 */
export function ToolsSection({ tools }: { tools: Item[] }) {
  const costMax = tools.reduce((acc, t) => Math.max(acc, t.cost), 0);
  const totalCost = tools.reduce((acc, t) => acc + t.cost, 0);

  if (tools.length === 0) {
    return (
      <Section label="工具">
        <p className="empty-row">本期没有用量记录</p>
      </Section>
    );
  }

  return (
    <Section label="工具" meta={`${tools.length} 个工具`}>
      <ol className="tool-list">
        {tools.map((t) => {
          const credit = t.counts.credits > 0 && t.total_tokens === 0;
          const share = costMax > 0 ? (t.cost / costMax) * 100 : 0;
          return (
            <li className="tool" key={t.key} style={{ "--c": toolColor(t.key) } as React.CSSProperties}>
              <div className="tool-top">
                <ToolIcon tool={t.key} size={22} />
                <span className="tool-name">{toolDisplay(t.key)}</span>
                <span className="tool-tokens num">{credit ? "—" : compactTokens(t.total_tokens)}</span>
                <span className="tool-cost num" data-credit={credit || undefined} data-unpriced={!t.priced || undefined}>
                  {credit
                  ? t.cost > 0
                    ? `≈${money(t.cost)}`
                    : creditText(t.counts.credits)
                  : t.priced
                    ? money(t.cost)
                    : "无价格"}
                </span>
              </div>
              {credit ? (
                <div className="tool-foot">
                  <span className="num">{count(t.requests)}</span>
                  <span>
                    {t.cost > 0
                      ? `次 · ${creditText(t.counts.credits)}，按方案价折算`
                      : "次 · credits 计量，不计入美元"}
                  </span>
                </div>
              ) : (
                <>
                  <div className="track tool-track" aria-hidden="true">
                    <i style={{ width: `${Math.max(share, t.cost > 0 ? 2 : 0)}%` }} />
                  </div>
                  <div className="tool-foot">
                    <span className="num">{count(t.requests)}</span>
                    <span>次</span>
                    <span className="dot-sep" aria-hidden="true" />
                    <span className="num">{count(t.sessions)}</span>
                    <span>会话</span>
                    <span className="grow" />
                    <span>
                      缓存 {percent(cachedOf(t.counts), 0)} · 占 {percent(totalCost > 0 ? (t.cost / totalCost) * 100 : 0, 0)}
                    </span>
                  </div>
                </>
              )}
            </li>
          );
        })}
      </ol>
    </Section>
  );
}

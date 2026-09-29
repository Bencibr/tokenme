import { useMemo, useState } from "react";
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
import { nextSortState, SortToggle, type SortState } from "./SortToggle";
import { ToolIcon } from "./ToolIcon";
import { useMoney } from "../lib/display";

/**
 * The product's core differentiator: one row per tool, the same three numerals
 * in the same columns every time, so cross-tool comparison is a single glance.
 * The badge is the tool's own app icon where one is installed, which is what
 * lets twelve rows be scanned by shape instead of read one name at a time.
 * The cost share is a wash of the tool's own colour behind the row — the same
 * language as the ranks page, so the eye learns it once.
 */
export function ToolsSection({ tools }: { tools: Item[] }) {
  const showMoney = useMoney();
  const [sort, setSort] = useState<SortState>("default");
  const ordered = useMemo(() => {
    if (sort === "default") return tools;
    const dir = sort === "asc" ? 1 : -1;
    return [...tools].sort((a, b) => (a.total_tokens - b.total_tokens) * dir);
  }, [tools, sort]);
  // With dollars off, the wash ranks by tokens instead of cost — the row has
  // to keep saying something about relative size.
  const keyOf = (t: Item) => (showMoney ? t.cost : t.total_tokens);
  const costMax = ordered.reduce((acc, t) => Math.max(acc, t.priced ? t.cost : 0), 0);
  const tokenMax = ordered.reduce((acc, t) => Math.max(acc, t.total_tokens), 0);
  const totalCost = ordered.reduce((acc, t) => acc + t.cost, 0);
  const totalTokens = ordered.reduce((acc, t) => acc + t.total_tokens, 0);
  // Each row washes on a scale that is true for it: priced tools on cost,
  // everything else on tokens. A tool the price list cannot name (DSH's
  // deepseek-flash, say) must still show its relative size — hiding the wash
  // read as "this tool did nothing", which was never the meaning.
  const washOf = (t: Item) => {
    const max = showMoney && t.priced ? costMax : tokenMax;
    const value = showMoney && t.priced ? t.cost : t.total_tokens;
    return max > 0 ? (value / max) * 100 : 0;
  };
  // A real share rounds to 0% too easily at these magnitudes (DSH's 0.05%);
  // "<1%" keeps the number honest about being small instead of empty.
  const shareText = (share: number) => (share > 0 && share < 1 ? "<1%" : percent(share, 0));

  if (tools.length === 0) {
    return (
      <Section label="工具">
        <p className="empty-row">本期没有用量记录</p>
      </Section>
    );
  }

  return (
    <Section
      label="工具"
      meta={`${tools.length} 个工具`}
      trail={<SortToggle state={sort} onCycle={() => setSort((s) => nextSortState(s))} />}
    >
      <ol className="tool-list">
        {ordered.map((t) => {
          const credit = t.counts.credits > 0 && t.total_tokens === 0;
          const wash = washOf(t);
          return (
            <li
              className="tool"
              key={t.key}
              style={
                {
                  "--c": toolColor(t.key),
                  backgroundImage: "linear-gradient(90deg, color-mix(in srgb, var(--c) 9%, transparent) 0 0)",
                  backgroundSize: `${Math.max(wash, keyOf(t) > 0 ? 2 : 0)}% 100%`,
                } as React.CSSProperties
              }
            >
              <div className="tool-top">
                <ToolIcon tool={t.key} size={24} />
                <span className="tool-name">{toolDisplay(t.key)}</span>
                <span className="tool-tokens num">{credit ? "—" : compactTokens(t.total_tokens)}</span>
                <span className="tool-cost num" data-credit={credit || undefined} data-unpriced={!t.priced || undefined}>
                  {credit
                  ? showMoney && t.cost > 0
                    ? `≈${money(t.cost)}`
                    : creditText(t.counts.credits)
                  : showMoney
                    ? t.priced
                      ? money(t.cost)
                      : "无价格"
                    : ""}
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
                <div className="tool-foot">
                  <span className="num">{count(t.requests)}</span>
                  <span>次</span>
                  <span className="dot-sep" aria-hidden="true" />
                  <span className="num">{count(t.sessions)}</span>
                  <span>会话</span>
                  <span className="grow" />
                  <span>
                    缓存 {percent(cachedOf(t.counts), 0)}
                    {showMoney ? (
                      <>
                        {" · 占 "}
                        {t.priced ? (
                          <span className="num">{shareText(totalCost > 0 ? (t.cost / totalCost) * 100 : 0)}</span>
                        ) : t.total_tokens > 0 ? (
                          <>
                            <span className="num">{shareText(totalTokens > 0 ? (t.total_tokens / totalTokens) * 100 : 0)}</span>
                            <span className="dim">（按 tokens）</span>
                          </>
                        ) : (
                          <span className="num">{percent(0, 0)}</span>
                        )}
                      </>
                    ) : null}
                  </span>
                </div>
              )}
            </li>
          );
        })}
      </ol>
      {showMoney && totalCost > 0 ? (
        <div className="share">
          <div className="share-bar" aria-hidden="true">
            {ordered
              .filter((t) => t.cost > 0)
              .map((t) => (
                <span
                  key={t.key}
                  style={
                    {
                      "--c": toolColor(t.key),
                      width: `${(t.cost / totalCost) * 100}%`,
                    } as React.CSSProperties
                  }
                />
              ))}
          </div>
          <div className="share-legend">
            {ordered
              .filter((t) => t.cost > 0)
              .map((t) => (
                <span key={t.key} className="share-item">
                  <i style={{ background: toolColor(t.key) }} />
                  {toolDisplay(t.key)} <b className="num">{percent(totalCost > 0 ? (t.cost / totalCost) * 100 : 0, 0)}</b>
                </span>
              ))}
          </div>
        </div>
      ) : null}
    </Section>
  );
}

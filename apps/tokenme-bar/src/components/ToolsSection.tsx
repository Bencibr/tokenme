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
import { t as tr } from "../lib/i18n";

/**
 * The product's core differentiator: one row per tool, the same three numerals
 * in the same columns every time, so cross-tool comparison is a single glance.
 * The badge is the tool's own app icon where one is installed, which is what
 * lets twelve rows be scanned by shape instead of read one name at a time.
 * The size share is the tool's own colour washed across the full row — small
 * shares ride a 2% floor so a quiet tool still reads as present.
 */
export function ToolsSection({ tools }: { tools: Item[] }) {
  const showMoney = useMoney();
  const [sort, setSort] = useState<SortState>("default");
  const ordered = useMemo(() => {
    if (sort === "default") return tools;
    const dir = sort === "asc" ? 1 : -1;
    return [...tools].sort((a, b) => (a.total_tokens - b.total_tokens) * dir);
  }, [tools, sort]);
  // The wash follows each row's "size language": priced tools on cost, and
  // everything else on tokens — money off, or a tool the price list cannot
  // name (dsh's deepseek-flash). A row whose colour variable points past the
  // palette (--cat-18 for dsh once) rendered the wash transparent, which read
  // as "did nothing"; the palette now outgrows TOOL_ORDER with it.
  const washOf = (t: Item) => {
    const max = showMoney && t.priced ? costMax : tokenMax;
    const value = showMoney && t.priced ? t.cost : t.total_tokens;
    const share = max > 0 ? (value / max) * 100 : 0;
    // 3% floor: a 0.05% share is a sub-pixel sliver, and an invisible bar is
    // the same lie as no bar. The floor keys on activity, not the money value:
    // a tool whose cost computes to $0.00 (opencode's free tier, 9.8M tokens
    // in a day) must still show its band, or silence reads as "did nothing".
    return Math.max(share, t.total_tokens > 0 || t.cost > 0 ? 3 : 0);
  };
  const costMax = ordered.reduce((acc, t) => Math.max(acc, t.priced ? t.cost : 0), 0);
  const tokenMax = ordered.reduce((acc, t) => Math.max(acc, t.total_tokens), 0);
  const totalCost = ordered.reduce((acc, t) => acc + t.cost, 0);

  if (tools.length === 0) {
    return (
      <Section label={tr("tools.section")}>
        <p className="empty-row">{tr("tools.empty")}</p>
      </Section>
    );
  }

  return (
    <Section
      label={tr("tools.section")}
      meta={tr("tools.meta", { n: tools.length })}
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
                  backgroundSize: `${wash}% 100%`,
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
                      : tr("tools.unpriced")
                    : ""}
                </span>
              </div>
              {credit ? (
                <div className="tool-foot">
                  <span className="num">{count(t.requests)}</span>
                  <span>
                    {t.cost > 0
                      ? tr("tools.credit.priced", { c: creditText(t.counts.credits) })
                      : tr("tools.credit.free")}
                  </span>
                </div>
              ) : (
                <div className="tool-foot">
                  <span className="num">{count(t.requests)}</span>
                  <span>{tr("hdr.reqs")}</span>
                  <span className="dot-sep" aria-hidden="true" />
                  <span className="num">{count(t.sessions)}</span>
                  <span>{tr("tools.sessions")}</span>
                  <span className="grow" />
                  <span>{tr("hdr.cache", { p: percent(cachedOf(t.counts), 0) })}</span>
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

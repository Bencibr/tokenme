import { useMemo, useState } from "react";
import type { Item, UnpricedModel } from "../types";
import { compactTokens, credits as creditText, money } from "../lib/format";
import { useMoney } from "../lib/display";
import { MoreRow, Section } from "./Section";
import { nextSortState, SortToggle, type SortState } from "./SortToggle";

/**
 * One row = name, tokens, cost, with the ranking carried by a proportional
 * wash behind the row instead of a separate hairline bar: the two-line rows
 * read as a ladder of stripes, and the 2 px tracks were too thin to compare.
 * The fill is accent at 10%, so the text keeps full ink over it.
 */
export function RankRows({
  items,
  limit = 5,
  weight = "cost",
}: {
  items: Item[];
  limit?: number;
  weight?: "cost" | "tokens";
}) {
  const shown = items.slice(0, limit);
  // Dollars off: rank and wash by tokens — the list keeps its shape without
  // pretending to know what a model costs.
  const rankingByCost = weight === "cost" && useMoney();
  const max = shown.reduce((acc, i) => Math.max(acc, rankingByCost ? i.cost : i.total_tokens), 0);

  return (
    <ol className="rank">
      {shown.map((i) => {
        const credit = i.counts.credits > 0 && i.total_tokens === 0;
        const measure = rankingByCost ? i.cost : i.total_tokens;
        const share = max > 0 ? Math.max((measure / max) * 100, measure > 0 ? 2 : 0) : 0;
        const [head, tail] = splitPath(i.label);
        return (
          <li
            className="rank-row"
            key={i.key}
            style={{
              // a fading tail: the wash reads as a bar, not a stained block
              backgroundImage:
                "linear-gradient(90deg, var(--page-accent-soft), transparent 92%)",
              backgroundSize: `${share}% 100%`,
            }}
          >
            <div className="rank-line">
              <span className="rank-name" title={i.key}>
                {head ? <span className="rank-dir">{head}</span> : null}
                {tail}
              </span>
              <span className="rank-tokens num">{credit ? "—" : compactTokens(i.total_tokens)}</span>
              <span className="rank-cost num" data-credit={credit || undefined} data-unpriced={!i.priced || undefined}>
                {credit
                  ? creditText(i.counts.credits)
                  : !rankingByCost
                    ? ""
                    : i.priced
                      ? money(i.cost)
                      : "无价格"}
              </span>
            </div>
          </li>
        );
      })}
    </ol>
  );
}

/** `/Users/me/work/tokenme` → dim the directory, keep the basename at full ink. */
function splitPath(label: string): [string, string] {
  const i = label.lastIndexOf("/");
  return i > 0 ? [label.slice(0, i + 1), label.slice(i + 1)] : ["", label];
}

interface RankedProps {
  label: string;
  items: Item[];
  limit?: number;
  weight?: "cost" | "tokens";
  unpriced?: UnpricedModel[];
  emptyText?: string;
}

export function RankedList({ label, items, limit = 5, weight = "cost", unpriced, emptyText }: RankedProps) {
  const [all, setAll] = useState(false);
  const [sort, setSort] = useState<SortState>("default");
  const showMoney = useMoney();
  const hidden = items.length - Math.min(items.length, limit);
  const shown = all ? items.length : Math.min(items.length, limit);
  const listed = (unpriced ?? []).filter((u) => items.some((i) => i.key === u.model && !i.priced));
  // The toggle re-orders by the same measure the wash paints with, so the
  // stripes stay aligned with the ranking in both directions.
  const ordered = useMemo(() => {
    if (sort === "default") return items;
    const byCost = weight === "cost" && showMoney;
    const dir = sort === "asc" ? 1 : -1;
    return [...items].sort((a, b) => {
      const ma = byCost ? a.cost : a.total_tokens;
      const mb = byCost ? b.cost : b.total_tokens;
      return (ma - mb) * dir;
    });
  }, [items, sort, weight, showMoney]);

  return (
    <Section
      label={all ? `${label} · 全部` : `${label} Top ${Math.max(shown, 1)}`}
      meta={all ? `${items.length} 项` : hidden > 0 ? `另 ${hidden} 项` : undefined}
      trail={items.length > 1 ? <SortToggle state={sort} onCycle={() => setSort((v) => nextSortState(v))} /> : undefined}
    >
      {items.length === 0 ? (
        <p className="empty-row">{emptyText ?? "本期无数据"}</p>
      ) : (
        <RankRows items={ordered} limit={shown} weight={weight} />
      )}
      <MoreRow open={all} total={items.length} preview={limit} onToggle={() => setAll((v) => !v)} />
      {listed.length > 0 ? (
        <p className="foot-note">
          {unpriced?.length ?? 0} 个模型暂无价格：
          {listed
            .slice(0, 2)
            .map((u) => `${u.model} ${compactTokens(u.total_tokens)}`)
            .join(" · ")}
        </p>
      ) : null}
    </Section>
  );
}

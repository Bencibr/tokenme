import { useState } from "react";
import type { Item } from "../types";
import { count } from "../lib/format";
import { RankRows } from "./RankedList";
import { MoreRow, Section } from "./Section";

type TabKey = "mcp" | "skill";

/**
 * MCP / Skill attribution — where the tokens actually went. One tab strip and
 * one list, so the two never compete for vertical space.
 */
export function CallTabs({ mcps, skills }: { mcps: Item[]; skills: Item[] }) {
  const [tab, setTab] = useState<TabKey>(mcps.length >= skills.length ? "mcp" : "skill");
  const [all, setAll] = useState(false);
  if (mcps.length === 0 && skills.length === 0) return null;

  const items = tab === "mcp" ? mcps : skills;

  return (
    <Section label="调用来源" meta={`${count(mcps.length + skills.length)} 项`}>
      <div className="tabs" role="tablist" aria-label="调用类型">
        {(
          [
            ["mcp", `MCP`, mcps.length],
            ["skill", `Skill`, skills.length],
          ] as const
        ).map(([key, label, n]) => (
          <button
            key={key}
            type="button"
            role="tab"
            className="tab"
            aria-selected={tab === key}
            tabIndex={tab === key ? 0 : -1}
            onClick={() => setTab(key)}
            onKeyDown={(e) => {
              if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
              e.preventDefault();
              setTab(key === "mcp" ? "skill" : "mcp");
            }}
          >
            {label} <span className="tab-n num">{n}</span>
          </button>
        ))}
      </div>
      {items.length === 0 ? (
        <p className="empty-row">本期没有{tab === "mcp" ? " MCP" : " Skill"}调用</p>
      ) : (
        <div role="tabpanel" aria-label={tab === "mcp" ? "MCP 调用" : "Skill 调用"}>
          <RankRows items={items} limit={all ? items.length : 5} weight="tokens" />
          <MoreRow open={all} total={items.length} preview={5} onToggle={() => setAll((v) => !v)} />
        </div>
      )}
    </Section>
  );
}

import { useState } from "react";
import type { SourceStatus, Summary } from "../types";
import { compactTokens, count, money } from "../lib/format";
import { IconMissing } from "./Icons";
import { MoreRow, Section } from "./Section";
import { ToolIcon } from "./ToolIcon";

/**
 * Which logs were actually read. Undetected sources stay reachable rather than
 * deleted — hiding them would hide the only hint about how to make them appear —
 * but they move behind a toggle, because on a machine with nine of fourteen tools
 * installed the dead rows were the majority of the section's height.
 */
export function Sources({ sources, allTime }: { sources: SourceStatus[]; allTime: Summary }) {
  const [showMissing, setShowMissing] = useState(false);
  const detected = sources.filter((s) => s.detected);
  const missing = sources.filter((s) => !s.detected);
  const events = sources.reduce((acc, s) => acc + s.events_ingested, 0);
  const listed = showMissing ? [...detected, ...missing] : detected;

  return (
    <Section label="数据源" meta={`${detected.length}/${sources.length} 已检测 · ${count(events)} 事件`}>
      <ol className="src-list">
        {listed.map((s) => (
          <li className="src" key={s.id} data-off={!s.detected || undefined}>
            {s.detected ? <ToolIcon tool={s.id} size={20} /> : <IconMissing size={12} className="src-icon" />}
            <span className="src-name">{s.display}</span>
            <span className="src-events num">{s.detected ? `${count(s.events_ingested)} 条` : "未检测到"}</span>
            <div className="src-meta">
              <span className="src-root" title={s.roots.join("\n")}>
                {s.roots[0] ?? "—"}
                {s.roots.length > 1 ? ` +${s.roots.length - 1}` : ""}
              </span>
              {s.hint ? (
                <>
                  <span className="dot-sep" aria-hidden="true" />
                  <span>{s.hint}</span>
                </>
              ) : null}
            </div>
          </li>
        ))}
      </ol>
      <MoreRow
        open={showMissing}
        total={sources.length}
        preview={detected.length}
        onToggle={() => setShowMissing((v) => !v)}
        closedLabel={`展开 ${missing.length} 个未检测到的源`}
        openLabel="收起未检测到的源"
      />
      <p className="src-total">
        全部历史
        <span className="num"> {compactTokens(allTime.total_tokens)}</span>
        <span> tokens ·</span>
        <span className="num"> {money(allTime.cost)}</span>
        {allTime.credits > 0 ? (
          <>
            <span> · </span>
            <span className="num">{allTime.credits.toFixed(2)}</span>
            <span> credits</span>
          </>
        ) : null}
      </p>
    </Section>
  );
}

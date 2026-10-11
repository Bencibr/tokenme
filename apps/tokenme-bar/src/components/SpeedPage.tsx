import { useMemo, useState } from "react";
import type { TtftSample, TtftSnapshot } from "../types";
import { relativeTime } from "../lib/format";
import { t } from "../lib/i18n";
import { Section } from "./Section";
import { ToolIcon } from "./ToolIcon";

/** The window the average covers: the old table's median window, kept — a
 *  provider's older entries say more about yesterday's network than about the
 *  access speed the user is reading. */
const AVG_WINDOW = 20;

const PROVIDERS: { id: TtftSample["provider"]; tool: string; nameKey: "ttft.codex" | "ttft.claude"; granularityKey: "ttft.turn" | "ttft.request" }[] = [
  { id: "codex", tool: "codex", nameKey: "ttft.codex", granularityKey: "ttft.turn" },
  { id: "claude_code", tool: "claude", nameKey: "ttft.claude", granularityKey: "ttft.request" },
];

function average(values: number[]): number | null {
  if (!values.length) return null;
  return values.reduce((a, b) => a + b, 0) / values.length;
}

function metricLabel(metric: TtftSample["metric"]): string {
  return metric === "first_content" ? t("ttft.first_content") : t("ttft.ttft");
}

function SpeedRow({ nameKey, granularityKey, tool, samples, now }: {
  nameKey: "ttft.codex" | "ttft.claude";
  granularityKey: "ttft.turn" | "ttft.request";
  tool: string;
  samples: TtftSample[];
  now: number;
}) {
  const latest = samples[0];
  // Only like compares with like: the two metrics answer different questions
  // (first token vs first content), and one provider may emit both.
  const comparable = latest ? samples.filter((sample) => sample.metric === latest.metric) : [];
  const avg = average(comparable.slice(0, AVG_WINDOW).map((sample) => sample.ttft_ms));
  const sub = [t(granularityKey), latest?.model ?? null].filter(Boolean).join(" · ");
  return (
    <li className="speed">
      <ToolIcon tool={tool} size={20} />
      <span className="speed-name">
        <span className="speed-provider">{t(nameKey)}</span>
        <span className="ttft-granularity">{sub}</span>
      </span>
      <span className="speed-fig">
        <span className="speed-last num">{latest ? `${Math.round(latest.ttft_ms)} ms` : "—"}</span>
        <span className="ttft-granularity">{latest ? metricLabel(latest.metric) : t("ttft.no_data")}</span>
      </span>
      <span className="speed-meta">
        <span className="num">{t("ttft.avg", { ms: Math.round(avg ?? latest?.ttft_ms ?? 0) })}</span>
        <span>{t("ttft.samples", { n: comparable.length })}</span>
        <span>{latest ? relativeTime(latest.sampled_at_ms, now) : "—"}</span>
      </span>
    </li>
  );
}

export function SpeedPage({ snapshot, now }: { snapshot: TtftSnapshot; now: number }) {
  const [aboutOpen, setAboutOpen] = useState(false);
  const byProvider = useMemo(() => ({
    codex: snapshot.samples.filter((sample) => sample.provider === "codex").sort((a, b) => b.sampled_at_ms - a.sampled_at_ms),
    claude_code: snapshot.samples.filter((sample) => sample.provider === "claude_code").sort((a, b) => b.sampled_at_ms - a.sampled_at_ms),
  }), [snapshot.samples]);
  return (
    <Section
      label={t("ttft.title")}
      trail={
        <span className="ttft-status">
          <span className="ttft-local">{t("ttft.local")}</span>
          <span className={`ttft-state${snapshot.enabled ? " is-on" : ""}`}>{snapshot.enabled ? t("ttft.passive") : t("ttft.disabled")}</span>
          <button
            type="button"
            className="ttft-about-button"
            aria-label={t("ttft.about.label")}
            aria-expanded={aboutOpen}
            aria-controls="ttft-about-note"
            onClick={() => setAboutOpen((open) => !open)}
          >?</button>
        </span>
      }
    >
      {aboutOpen ? <p className="ttft-about-note" id="ttft-about-note" role="note">{t("ttft.about")}</p> : null}
      <ol className="speed-list">
        {PROVIDERS.map((p) => (
          <SpeedRow key={p.id} nameKey={p.nameKey} granularityKey={p.granularityKey} tool={p.tool} samples={byProvider[p.id]} now={now} />
        ))}
      </ol>
      {snapshot.claude_config_status === "error" ? <p className="ttft-note" role="status">{t("ttft.claude_config_error")}</p> : null}
      {snapshot.enabled && snapshot.claude_receiver === "unavailable" ? <p className="ttft-note" role="status">{t("ttft.receiver_unavailable")}</p> : null}
      {snapshot.enabled ? <p className="ttft-note">{t("ttft.passive_hint")}</p> : null}
    </Section>
  );
}

import { useMemo, useState } from "react";
import type { TtftSample, TtftSnapshot } from "../types";
import { relativeTime } from "../lib/format";
import { t } from "../lib/i18n";
import { Section } from "./Section";
import { ToolIcon } from "./ToolIcon";

/** The window the averages cover: the old table's median window, kept — a
 *  provider's older entries say more about yesterday's network than about the
 *  access speed the user is reading. */
const AVG_WINDOW = 20;

/** 快中慢 bands, the quota gauge's colour language: TTFT lower is faster
 *  (≤1 s green, ≤2.5 s amber, past that red); TPS is the inverse (≥50 green,
 *  ≥20 amber, below red). */
function stageOfMs(ms: number): "ok" | "mid" | "bad" {
  return ms <= 1000 ? "ok" : ms <= 2500 ? "mid" : "bad";
}
function stageOfTps(tps: number): "ok" | "mid" | "bad" {
  return tps >= 50 ? "ok" : tps >= 20 ? "mid" : "bad";
}

const PROVIDERS: { id: "codex" | "claude_code"; tool: string; nameKey: "ttft.codex" | "ttft.claude"; granularityKey: "ttft.turn" | "ttft.request" }[] = [
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

function SpeedCell({ value, cap, cap2, stage }: { value: string; cap: string; cap2?: string; stage: "ok" | "mid" | "bad" | null }) {
  return (
    <span className="speed-cell">
      <span className="speed-num num" data-stage={stage ?? undefined}>{value}</span>
      <span className="speed-cap">{cap}</span>
      {cap2 ? <span className="speed-cap">{cap2}</span> : null}
    </span>
  );
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
  const recent = comparable.slice(0, AVG_WINDOW);
  const avgMs = average(recent.map((sample) => sample.ttft_ms));
  const avgTps = average(recent.map((sample) => sample.tps).filter((tps): tps is number => typeof tps === "number" && Number.isFinite(tps)));
  const sub = [latest?.model ?? null,
    comparable.length > 0 ? t("ttft.samples", { n: comparable.length }) : null].filter(Boolean).join(" · ");
  return (
    <li className="speed" title={t(nameKey)}>
      <span className="speed-ic"><ToolIcon tool={tool} size={20} /></span>
      <span className="speed-sub ttft-granularity">{sub}</span>
      <SpeedCell
        value={latest ? `${Math.round(latest.ttft_ms)} ms` : "—"}
        cap={latest ? metricLabel(latest.metric) : t("ttft.no_data")}
        cap2={latest ? `${t(granularityKey)} · ${relativeTime(latest.sampled_at_ms, now)}` : undefined}
        stage={latest ? stageOfMs(latest.ttft_ms) : null}
      />
      <SpeedCell
        value={avgMs != null ? `${Math.round(avgMs)} ms` : "—"}
        cap={t("ttft.head.avg")}
        stage={avgMs != null ? stageOfMs(avgMs) : null}
      />
      <SpeedCell
        value={avgTps != null ? avgTps.toFixed(1) : "—"}
        cap={t("ttft.head.tps")}
        stage={avgTps != null ? stageOfTps(avgTps) : null}
      />
    </li>
  );
}

export function SpeedPage({ snapshot, now }: { snapshot: TtftSnapshot; now: number }) {
  const [aboutOpen, setAboutOpen] = useState(false);
  const byProvider = useMemo(() => ({
    codex: snapshot.samples.filter((sample) => sample.provider === "codex").sort((a, b) => b.sampled_at_ms - a.sampled_at_ms),
    claude_code: snapshot.samples.filter((sample) => sample.provider === "claude_code").sort((a, b) => b.sampled_at_ms - a.sampled_at_ms),
  }), [snapshot.samples]);
  // Only an open IDE earns a row: the snapshot's host flags are the same
  // running-process signal the quota strip's gate reads; engines older than
  // the field omit it and the page then shows every row.
  const visible = PROVIDERS.filter((p) => snapshot.hosts?.[p.id] !== false);
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
      {visible.length === 0 ? <p className="ttft-note">{t("ttft.no_host")}</p> : (
        <ol className="speed-list">
          {visible.map((p) => (
            <SpeedRow key={p.id} nameKey={p.nameKey} granularityKey={p.granularityKey} tool={p.tool} samples={byProvider[p.id]} now={now} />
          ))}
        </ol>
      )}
      {snapshot.claude_config_status === "error" ? <p className="ttft-note" role="status">{t("ttft.claude_config_error")}</p> : null}
      {snapshot.enabled && snapshot.claude_receiver === "unavailable" ? <p className="ttft-note" role="status">{t("ttft.receiver_unavailable")}</p> : null}
      {snapshot.enabled ? <p className="ttft-note">{t("ttft.passive_hint")}</p> : null}
    </Section>
  );
}

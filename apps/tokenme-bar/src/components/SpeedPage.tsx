import { useMemo, useState } from "react";
import type { ActiveSpeedTest, MachineScope, TtftSample, TtftSnapshot } from "../types";
import { duration, relativeTime } from "../lib/format";
import { t } from "../lib/i18n";
import { bridge } from "../lib/bridge";
import { IconInfo } from "./Icons";
import { Section } from "./Section";
import { ToolIcon } from "./ToolIcon";

/** Non-DSH providers use a rolling window; DSH's reference UI aggregates the
 *  complete active session, so its window is selected by session_id below. */
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

const PROVIDERS: { id: "codex" | "claude_code" | "dsh"; tool: string; nameKey: "ttft.codex" | "ttft.claude" | "ttft.dsh"; granularityKey: "ttft.turn" | "ttft.request" }[] = [
  { id: "codex", tool: "codex", nameKey: "ttft.codex", granularityKey: "ttft.turn" },
  { id: "claude_code", tool: "claude", nameKey: "ttft.claude", granularityKey: "ttft.request" },
  { id: "dsh", tool: "dsh", nameKey: "ttft.dsh", granularityKey: "ttft.request" },
];

function average(values: number[]): number | null {
  if (!values.length) return null;
  return values.reduce((a, b) => a + b, 0) / values.length;
}

function metricLabel(metric: TtftSample["metric"]): string {
  return metric === "first_content" ? t("ttft.first_content") : t("ttft.ttft");
}

function SpeedCell({ value, cap, stage }: { value: string; cap: string; stage: "ok" | "mid" | "bad" | null }) {
  return (
    <span className="speed-cell">
      <span className="speed-num num" data-stage={stage ?? undefined}>{value}</span>
      <span className="speed-cap">{cap}</span>
    </span>
  );
}

function SpeedRow({ nameKey, granularityKey, provider, tool, samples, now, canTest, testing, onTest, activeTest }: {
  nameKey: "ttft.codex" | "ttft.claude" | "ttft.dsh";
  granularityKey: "ttft.turn" | "ttft.request";
  provider: "codex" | "claude_code" | "dsh";
  tool: string;
  samples: TtftSample[];
  now: number;
  canTest: boolean;
  testing: boolean;
  onTest: () => void;
  activeTest: ActiveSpeedTest | null;
}) {
  const latest = samples[0];
  // Only like compares with like: the two metrics answer different questions
  // (first token vs first content), and one provider may emit both.
  const comparable = latest ? samples.filter((sample) => sample.metric === latest.metric) : [];
  const recent = tool === "dsh" && latest?.session_id
    ? comparable.filter((sample) => sample.session_id === latest.session_id)
    : comparable.slice(0, AVG_WINDOW);
  const avgMs = average(recent.map((sample) => sample.ttft_ms));
  const dshWeighted = recent.filter((sample) => typeof sample.output_tokens === "number" && typeof sample.decode_ms === "number" && sample.decode_ms > 0);
  const dshOutput = dshWeighted.reduce((sum, sample) => sum + sample.output_tokens!, 0);
  const dshDecodeMs = dshWeighted.reduce((sum, sample) => sum + sample.decode_ms!, 0);
  const avgTps = tool === "dsh" && dshDecodeMs > 0
    ? dshOutput / (dshDecodeMs / 1000)
    : average(recent.map((sample) => sample.tps).filter((tps): tps is number => typeof tps === "number" && Number.isFinite(tps)));
  // Provider-constant facts (name, granularity, sample count) are metadata,
  // not per-sample data: they live in the hover title, and the row face
  // carries only the model and the three figures.
  const title = [t(nameKey), t(granularityKey),
    recent.length > 0 ? t("ttft.samples", { n: recent.length }) : null].filter(Boolean).join(" · ");
  const activeModel = activeTest?.provider === provider ? activeTest.model?.trim() : undefined;
  const sub = latest?.model?.trim() || activeModel || "—";
  return (
    <li className="speed" title={title}>
      <span className="speed-ic"><ToolIcon tool={tool} size={20} /></span>
      <span className="speed-meta">
        <span className="speed-sub ttft-granularity" title={sub === "—" ? undefined : sub}>{sub}</span>
        {canTest ? <button type="button" className="ttft-test-button" onClick={onTest} disabled={testing}>
          {testing ? t("ttft.active.running") : t("ttft.active.button")}
        </button> : null}
      </span>
      <SpeedCell
        value={latest ? duration(latest.ttft_ms) : "—"}
        cap={latest ? `${metricLabel(latest.metric)} · ${relativeTime(latest.sampled_at_ms, now)}` : t("ttft.no_data")}
        stage={latest ? stageOfMs(latest.ttft_ms) : null}
      />
      <SpeedCell
        value={avgMs != null ? duration(avgMs) : "—"}
        cap={t("ttft.head.avg")}
        stage={avgMs != null ? stageOfMs(avgMs) : null}
      />
      <SpeedCell
        value={avgTps != null ? avgTps.toFixed(1) : "—"}
        cap={t("ttft.head.tps")}
        stage={avgTps != null ? stageOfTps(avgTps) : null}
      />
      {activeTest?.provider === provider ? <div className="ttft-row-active" role="status">
        <span className="ttft-row-active-label" title={activeTest.model ?? undefined}>{t("ttft.active.result", { provider: t(nameKey), model: activeTest.model ?? "—", age: relativeTime(activeTest.sampledAtMs, now) })}</span>
        <SpeedCell
          value={duration(activeTest.ttftMs)}
          cap={activeTest.measurement === "claude_otel" ? t("ttft.first_content") : t("ttft.ttft")}
          stage={stageOfMs(activeTest.ttftMs)}
        />
        <SpeedCell
          value={activeTest.durationMs != null ? duration(activeTest.durationMs) : "—"}
          cap={t("ttft.active.duration")}
          stage={null}
        />
        <SpeedCell
          value={activeTest.tps != null ? activeTest.tps.toFixed(1) : "—"}
          cap="TPS"
          stage={activeTest.tps != null ? stageOfTps(activeTest.tps) : null}
        />
      </div> : null}
    </li>
  );
}

export function SpeedPage({ snapshot, now, scope }: { snapshot: TtftSnapshot; now: number; scope: MachineScope }) {
  const [aboutOpen, setAboutOpen] = useState(false);
  const [activeTest, setActiveTest] = useState<ActiveSpeedTest | null>(null);
  const [activeError, setActiveError] = useState<string | null>(null);
  const [testing, setTesting] = useState<ActiveSpeedTest["provider"] | null>(null);
  const byProvider = useMemo(() => ({
    codex: snapshot.samples.filter((sample) => sample.provider === "codex").sort((a, b) => b.sampled_at_ms - a.sampled_at_ms),
    claude_code: snapshot.samples.filter((sample) => sample.provider === "claude_code").sort((a, b) => b.sampled_at_ms - a.sampled_at_ms),
    dsh: snapshot.samples.filter((sample) => sample.provider === "dsh").sort((a, b) => b.sampled_at_ms - a.sampled_at_ms),
  }), [snapshot.samples]);
  // TTFT is collected on this machine only — the OTLP receiver and the
  // Codex rollouts are local files, and no sync bundle carries samples.
  // Scoped to a remote machine, the local rows would silently contradict
  // every other page's numbers, so the page says so instead (the same
  // honesty the quota strip's 本机·与筛选无关 note uses).
  const scopedAway = scope.kind === "origin";
  // Only an open IDE earns a row: the snapshot's host flags are the same
  // running-process signal the quota strip's gate reads; engines older than
  // the field omit it and the page then shows every row.
  const visible = PROVIDERS.filter((p) => p.id === "codex" || p.id === "claude_code" || snapshot.hosts?.[p.id] !== false);
  const runTest = async (provider: ActiveSpeedTest["provider"]) => {
    if (testing) return;
    setTesting(provider);
    setActiveError(null);
    try { setActiveTest(await bridge.runActiveSpeedTest(provider)); }
    catch (error) {
      const detail = error instanceof Error ? error.message : typeof error === "string" ? error : "";
      setActiveError(detail ? `${t("ttft.active.error")} ${detail}` : t("ttft.active.error"));
    }
    finally { setTesting(null); }
  };
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
          ><IconInfo size={7} /></button>
        </span>
      }
    >
      {aboutOpen ? <p className="ttft-about-note" id="ttft-about-note" role="note">{t("ttft.about")}</p> : null}
      {scopedAway ? <p className="ttft-note">{t("ttft.scoped", { name: scope.name })}</p> : null}
      {!scopedAway && visible.length === 0 ? <p className="ttft-note">{t("ttft.no_host")}</p> : null}
      {!scopedAway && visible.length > 0 ? (
        <ol className="speed-list">
          {visible.map((p) => (
            <SpeedRow key={p.id} nameKey={p.nameKey} granularityKey={p.granularityKey} provider={p.id} tool={p.tool} samples={byProvider[p.id]} now={now}
              canTest={p.id !== "dsh"} testing={testing === p.id} onTest={() => p.id !== "dsh" && void runTest(p.id)}
              activeTest={p.id === "dsh" ? null : activeTest} />
          ))}
        </ol>
      ) : null}
      {!scopedAway && activeError ? <p className="ttft-note" role="alert">{activeError}</p> : null}
      {!scopedAway ? <p className="ttft-note">{t("ttft.active.hint")}</p> : null}
      {snapshot.claude_config_status === "error" ? <p className="ttft-note" role="status">{t("ttft.claude_config_error")}</p> : null}
      {snapshot.enabled && snapshot.claude_receiver === "unavailable" ? <p className="ttft-note" role="status">{t("ttft.receiver_unavailable")}</p> : null}
      {snapshot.enabled ? <p className="ttft-note">{t("ttft.passive_hint")}</p> : null}
    </Section>
  );
}

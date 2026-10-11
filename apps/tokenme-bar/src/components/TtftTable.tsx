import { useMemo, useState } from "react";
import type { TtftSample, TtftSnapshot } from "../types";
import { relativeTime } from "../lib/format";
import { t } from "../lib/i18n";

function median(values: number[]): number | null {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

function metricLabel(sample: TtftSample): string {
  return sample.metric === "first_content" ? t("ttft.first_content") : t("ttft.ttft");
}

function ProviderRow({ provider, samples, now }: { provider: "codex" | "claude_code"; samples: TtftSample[]; now: number }) {
  const latest = samples[0];
  const comparable = latest ? samples.filter((sample) => sample.metric === latest.metric) : [];
  const medianMs = median(comparable.slice(0, 20).map((sample) => sample.ttft_ms));
  return (
    <tr>
      <th scope="row">
        <span className="ttft-name">{t(provider === "codex" ? "ttft.codex" : "ttft.claude")}</span>
        <span className="ttft-granularity">{t(provider === "codex" ? "ttft.turn" : "ttft.request")}</span>
      </th>
      <td>
        {latest ? <>
          <span className="ttft-last num">{Math.round(latest.ttft_ms)} ms</span>
          <span className="ttft-meta">{metricLabel(latest)}</span>
        </> : <span className="ttft-empty">{t("ttft.no_data")}</span>}
      </td>
      <td className="ttft-model" title={latest?.model ?? undefined}>{latest?.model ?? "—"}</td>
      <td>
        {latest ? <>
          <span className="ttft-meta">{t("ttft.median", { ms: Math.round(medianMs ?? latest.ttft_ms) })}</span>
          <span className="ttft-meta">{t("ttft.samples", { n: comparable.length })}</span>
          <span className="ttft-time">{relativeTime(latest.sampled_at_ms, now)}</span>
        </> : <span className="ttft-empty">—</span>}
      </td>
    </tr>
  );
}

export function TtftTable({ snapshot, now }: { snapshot: TtftSnapshot; now: number }) {
  const [aboutOpen, setAboutOpen] = useState(false);
  const byProvider = useMemo(() => ({
    codex: snapshot.samples.filter((sample) => sample.provider === "codex").sort((a, b) => b.sampled_at_ms - a.sampled_at_ms),
    claude_code: snapshot.samples.filter((sample) => sample.provider === "claude_code").sort((a, b) => b.sampled_at_ms - a.sampled_at_ms),
  }), [snapshot.samples]);
  return (
    <section className="ttft-section" aria-label={t("ttft.title")}>
      <div className="ttft-heading">
        <div className="ttft-title-group">
          <h2>{t("ttft.title")}</h2>
          <button
            type="button"
            className="ttft-about-button"
            aria-label={t("ttft.about.label")}
            aria-expanded={aboutOpen}
            aria-controls="ttft-about-note"
            onClick={() => setAboutOpen((open) => !open)}
          >?</button>
        </div>
        <span className="ttft-status">
          <span className="ttft-local">{t("ttft.local")}</span>
          <span className={`ttft-state${snapshot.enabled ? " is-on" : ""}`}>{snapshot.enabled ? t("ttft.passive") : t("ttft.disabled")}</span>
        </span>
      </div>
      {aboutOpen ? <p className="ttft-about-note" id="ttft-about-note" role="note">{t("ttft.about")}</p> : null}
      <table className="ttft-table">
        <thead>
          <tr>
            <th scope="col">{t("ttft.col.source")}</th>
            <th scope="col">{t("ttft.col.latest")}</th>
            <th scope="col">{t("ttft.col.model")}</th>
            <th scope="col">{t("ttft.col.summary")}</th>
          </tr>
        </thead>
        <tbody>
          <ProviderRow provider="codex" samples={byProvider.codex} now={now} />
          <ProviderRow provider="claude_code" samples={byProvider.claude_code} now={now} />
        </tbody>
      </table>
      {snapshot.claude_config_status === "error" ? <p className="ttft-note" role="status">{t("ttft.claude_config_error")}</p> : null}
      {snapshot.enabled && snapshot.claude_receiver === "unavailable" ? <p className="ttft-note" role="status">{t("ttft.receiver_unavailable")}</p> : null}
      {snapshot.enabled ? <p className="ttft-note">{t("ttft.passive_hint")}</p> : null}
    </section>
  );
}

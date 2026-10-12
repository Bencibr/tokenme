import { useEffect, useState } from "react";
import { applyTheme, bridge, inTauri } from "../lib/bridge";
import { t } from "../lib/i18n";
import type { PetNotice, PetNoticeKind, PetResponse } from "../lib/petCompanion";

export function petNoticeCopy(kind: PetNoticeKind, minutes: number) {
  switch (kind) {
    case "morning": return { title:t("pet.morning"), body:t("pet.morning.body"), icon:"☀" };
    case "noon": return { title:t("pet.noon"), body:t("pet.noon.body"), icon:"🍵" };
    case "evening": return { title:t("pet.evening"), body:t("pet.evening.body"), icon:"✦" };
    case "night": return { title:t("pet.night"), body:t("pet.night.body"), icon:"☾" };
    case "rest": return { title:t("pet.rest"), body:t("pet.rest.body", { minutes }), icon:"🌿" };
    case "hello": return { title:t("pet.hello"), body:t("pet.hello.body"), icon:"♡" };
  }
}

export function PetNoticeCard({ notice, onResponse }: { notice:PetNotice; onResponse:(action:PetResponse["action"]) => void }) {
  const copy = petNoticeCopy(notice.kind, notice.minutes);
  return <section className="pet-notice-card" data-kind={notice.kind} role="status" aria-live="polite">
    <button type="button" className="pet-notice-close" aria-label={t("pet.dismiss")} onClick={() => onResponse("dismiss")}>×</button>
    <div className="pet-notice-icon" aria-hidden="true">{copy.icon}</div>
    <div className="pet-notice-copy"><strong>{copy.title}</strong><p>{copy.body}</p></div>
    <div className="pet-notice-actions">
      {notice.kind === "rest" ? <>
        <button type="button" className="pet-notice-primary" onClick={() => onResponse("break")}>{t("pet.break")}</button>
        <button type="button" onClick={() => onResponse("snooze")}>{t("pet.snooze")}</button>
      </> : <button type="button" className="pet-notice-primary" onClick={() => onResponse("hello")}>{t("pet.reply")}</button>}
    </div>
  </section>;
}

export function PetNoticeApp() {
  const pin = new URLSearchParams(location.search).get("pet-notice") as PetNoticeKind | null;
  const [notice, setNotice] = useState<PetNotice | null>(!inTauri && pin && ["morning","noon","evening","night","rest","hello"].includes(pin)
    ? { id:1, kind:pin, minutes:60 } : null);
  useEffect(() => {
    let alive = true; let changed = false;
    const off = bridge.onPetNotice((next) => { changed = true; if (alive) setNotice(next); });
    void bridge.getPetNotice().then((next) => { if (alive && !changed && inTauri) setNotice(next); }).catch(() => {});
    return () => { alive = false; off(); };
  }, []);
  useEffect(() => {
    document.body.dataset.surface = "pet-notice";
    if (!notice) return;
    let alive = true;
    // Apply the saved palette before reveal; a hidden webview's rAF may pause.
    void bridge.panelSettings().then((settings) => {
      if (!alive) return;
      applyTheme(settings.theme);
      return bridge.petNoticeReady(notice.id);
    }).catch(() => { if (alive) void bridge.petNoticeReady(notice.id).catch(() => {}); });
    return () => { alive = false; };
  }, [notice]);
  return <div className="pet-notice-app">{notice && <PetNoticeCard notice={notice} onResponse={(action) => {
    void bridge.respondPetNotice(notice.id, action).catch(() => {});
    if (!inTauri) setNotice(null);
  }} />}</div>;
}

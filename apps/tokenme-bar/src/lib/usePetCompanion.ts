import { useEffect, useRef, useState } from "react";
import { bridge } from "./bridge";
import { DEFAULT_PET_PREFERENCES, PetCompanion, noticeMotion } from "./petCompanion";
import type { PetMotion, PetPreferences } from "./petCompanion";

const LEDGER = "tokenme.pet.companion.v1";

function restore() {
  try {
    const value = JSON.parse(localStorage.getItem(LEDGER) ?? "{}");
    const seen = value.seen && typeof value.seen === "object" && !Array.isArray(value.seen) ? value.seen : {};
    return new PetCompanion({ seen, snooze_until: Number(value.snooze_until) || 0, break_until: Number(value.break_until) || 0 });
  } catch { return new PetCompanion(); }
}

export function usePetCompanion() {
  const [prefs, setPrefs] = useState<PetPreferences>(DEFAULT_PET_PREFERENCES);
  const [motion, setMotion] = useState<PetMotion>("idle");
  const [emotion, setEmotion] = useState<PetMotion>("idle");
  const noticeVisible = useRef(false);
  const clock = useRef<PetCompanion>();
  if (!clock.current) clock.current = restore();
  const motionTimer = useRef<ReturnType<typeof setTimeout>>();
  const prefsRef = useRef(prefs);
  prefsRef.current = prefs;

  const save = () => { try { localStorage.setItem(LEDGER, JSON.stringify(clock.current!.ledger)); } catch { /* memory still dedupes this session */ } };
  const play = (next: PetMotion) => {
    if (motionTimer.current) clearTimeout(motionTimer.current);
    setMotion(prefsRef.current.motions ? next : "idle");
    motionTimer.current = setTimeout(() => setMotion("idle"), next === "sleep" ? 8000 : 2200);
  };

  useEffect(() => {
    let alive = true;
    let changed = false;
    const offPrefs = bridge.onPetPreferences((next) => {
      changed = true; if (alive) setPrefs(next);
      if (!next.motions) setMotion("idle");
    });
    void bridge.panelSettings().then((settings) => { if (alive && !changed) setPrefs(settings.pet_companion); }).catch(() => {});
    const offNotice = bridge.onPetNotice((notice) => {
      noticeVisible.current = Boolean(notice);
      setEmotion(notice ? noticeMotion(notice.kind) : "idle");
      if (notice) play(noticeMotion(notice.kind));
    });
    const offResponse = bridge.onPetResponse(({ action }) => {
      clock.current!.respond(action, Date.now()); save();
      if (action === "hello") play("cheer");
      if (action === "break") play("sleep");
    });
    let timer: ReturnType<typeof setTimeout>;
    const tick = async () => {
      try {
        const activity = await bridge.petActivity();
        if (!alive) return;
        const candidate = clock.current!.tick(Date.now(), prefsRef.current, activity);
        if (candidate) {
          const notice = await bridge.showPetNotice(candidate.kind, candidate.minutes);
          if (notice) { clock.current!.delivered(candidate, Date.now()); save(); }
        }
      } catch { /* an unavailable activity sample never accrues work */
        clock.current!.tick(Date.now(), prefsRef.current, { visible:false, locked:true, fullscreen:false, idle_ms:null });
      }
      if (alive) timer = setTimeout(tick, 5000);
    };
    timer = setTimeout(tick, 5000);
    return () => {
      alive = false; clearTimeout(timer); if (motionTimer.current) clearTimeout(motionTimer.current);
      offPrefs(); offNotice(); offResponse();
    };
    // Event callbacks read the current preferences through prefsRef.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return { motion, emotion, noticeVisible, motions: prefs.motions, play };
}

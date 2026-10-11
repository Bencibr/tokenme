export type PetNoticeKind = "morning" | "noon" | "evening" | "night" | "rest" | "hello";
export type PetMotion = "idle" | "hello" | "care" | "cheer" | "stretch" | "sleep" | "settle";

export interface PetPreferences {
  greetings: boolean;
  rest_reminders: boolean;
  motions: boolean;
  rest_interval_minutes: number;
}

export const DEFAULT_PET_PREFERENCES: PetPreferences = {
  greetings: true, rest_reminders: true, motions: true, rest_interval_minutes: 60,
};

export interface PetActivity {
  idle_ms: number | null;
  locked: boolean;
  fullscreen: boolean;
  visible: boolean;
}

export interface PetNotice { id: number; kind: PetNoticeKind; minutes: number }
export interface PetResponse { id: number; action: "dismiss" | "break" | "snooze" | "hello" }
export interface PetCandidate { kind: PetNoticeKind; key?: string; minutes: number }
export interface PetLedger { seen: Record<string, number>; snooze_until: number; break_until: number }

export function greetingSlot(now: Date): { kind: PetNoticeKind; key: string } | null {
  const hour = now.getHours();
  const kind = hour < 6 || hour >= 22 ? "night" : hour < 11 ? "morning" : hour < 14 ? "noon" : hour >= 18 ? "evening" : null;
  if (!kind) return null;
  const day = new Date(now);
  // 23:00 and 01:00 are the same night, including month/year boundaries.
  if (kind === "night" && hour < 6) day.setDate(day.getDate() - 1);
  return { kind, key: `${day.getFullYear()}-${day.getMonth() + 1}-${day.getDate()}/${kind}` };
}

/** Local input activity only. A sleeping/hidden window never accrues work. */
export class PetCompanion {
  activeMs = 0;
  private lastTick = 0;
  private cooldownUntil = 0;
  ledger: PetLedger;

  constructor(ledger?: Partial<PetLedger>) {
    this.ledger = { seen: ledger?.seen ?? {}, snooze_until: ledger?.snooze_until ?? 0, break_until: ledger?.break_until ?? 0 };
  }

  tick(now: number, prefs: PetPreferences, activity: PetActivity): PetCandidate | null {
    const elapsed = this.lastTick ? now - this.lastTick : 0;
    this.lastTick = now;
    if (elapsed < 0 || elapsed > 20_000) {
      this.activeMs = 0;
      this.ledger.snooze_until = 0;
      this.ledger.break_until = Math.min(this.ledger.break_until, now + 5 * 60_000);
    }
    if (!activity.visible || activity.locked || activity.idle_ms === null || activity.idle_ms >= 180_000) {
      this.activeMs = 0;
      this.ledger.snooze_until = 0;
      return null;
    }
    if (now < this.ledger.break_until) return null;
    this.activeMs += elapsed >= 0 && elapsed <= 20_000 ? elapsed : 0;
    if (!prefs.rest_reminders) this.ledger.snooze_until = 0;
    // Work can continue in a presentation; delivery waits until it ends.
    if (activity.fullscreen || now < this.cooldownUntil) return null;
    const slot = greetingSlot(new Date(now));
    if (prefs.greetings && slot && !this.ledger.seen[slot.key]) return { ...slot, minutes: 0 };
    const hour = new Date(now).getHours();
    const quiet = hour >= 22 || hour < 6;
    const interval = Math.max(15, Math.min(180, prefs.rest_interval_minutes)) * 60_000;
    const snoozed = this.ledger.snooze_until > 0;
    if (prefs.rest_reminders && !quiet && (snoozed ? now >= this.ledger.snooze_until : this.activeMs >= interval)) {
      return { kind: "rest", minutes: Math.max(1, Math.round(this.activeMs / 60_000)) };
    }
    return null;
  }

  delivered(candidate: PetCandidate, now: number) {
    if (candidate.key) this.ledger.seen[candidate.key] = now;
    if (candidate.kind === "rest") { this.activeMs = 0; this.ledger.snooze_until = 0; }
    this.cooldownUntil = now + 60_000;
    for (const [key, at] of Object.entries(this.ledger.seen)) {
      if (!Number.isFinite(at) || now - at > 8 * 86_400_000) delete this.ledger.seen[key];
    }
  }

  respond(action: PetResponse["action"], now: number) {
    if (action === "break") {
      this.activeMs = 0; this.ledger.snooze_until = 0; this.ledger.break_until = now + 5 * 60_000;
    } else if (action === "snooze") {
      this.ledger.snooze_until = now + 10 * 60_000;
    }
  }
}

export function noticeMotion(kind: PetNoticeKind): PetMotion {
  return kind === "night" ? "sleep" : kind === "rest" ? "stretch" : kind === "noon" ? "care" : kind === "hello" ? "cheer" : "hello";
}

// Run with node --experimental-strip-types scripts/test-pet-companion.mjs.
import assert from "node:assert/strict";
import { PetCompanion, DEFAULT_PET_PREFERENCES, greetingSlot, noticeMotion } from "../apps/tokenme-bar/src/lib/petCompanion.ts";

const active = { idle_ms:0, locked:false, fullscreen:false, visible:true };
const prefs = { ...DEFAULT_PET_PREFERENCES, greetings:false, rest_interval_minutes:15 };
const at = (day, hour, minute = 0) => new Date(2026, 9, day, hour, minute).getTime();
const advance = (clock, begin, minutes, state = active, settings = prefs) => {
  let result = null;
  for (let now = begin; now <= begin + minutes * 60_000; now += 5000) result = clock.tick(now, settings, state);
  return result;
};

assert.equal(greetingSlot(new Date(at(11,6))).kind, "morning");
assert.equal(greetingSlot(new Date(at(11,11))).kind, "noon");
assert.equal(greetingSlot(new Date(at(11,15))), null);
assert.equal(greetingSlot(new Date(at(11,18))).kind, "evening");
assert.equal(greetingSlot(new Date(at(11,23))).key, greetingSlot(new Date(at(12,1))).key);
assert.notEqual(greetingSlot(new Date(at(12,23))).key, greetingSlot(new Date(at(12,1))).key);
assert.equal(greetingSlot(new Date(2027,0,1,1)).key, greetingSlot(new Date(2026,11,31,23)).key);

const morning = new PetCompanion();
const candidate = morning.tick(at(11,8), DEFAULT_PET_PREFERENCES, active);
assert.equal(candidate.kind, "morning");
// Failed or suppressed delivery must not consume the greeting.
assert.equal(morning.tick(at(11,8)+5000, DEFAULT_PET_PREFERENCES, active).kind, "morning");
morning.delivered(candidate, at(11,8)+5000);
const restarted = new PetCompanion(morning.ledger);
assert.equal(restarted.tick(at(11,8)+70_000, DEFAULT_PET_PREFERENCES, active), null);
assert.equal(restarted.tick(at(12,8), DEFAULT_PET_PREFERENCES, active).kind, "morning");

for (const blocker of [{locked:true},{visible:false},{idle_ms:null},{idle_ms:180_000}]) {
  const clock = new PetCompanion(); advance(clock, at(11,15), 14);
  assert.equal(clock.tick(at(11,15,14)+5000, prefs, {...active,...blocker}), null);
  assert.equal(clock.activeMs, 0);
}
const clock = new PetCompanion();
assert.equal(advance(clock, at(11,15),15).kind, "rest");
clock.delivered({kind:"rest",minutes:15},at(11,15,15));
clock.respond("snooze",at(11,15,15));
assert.equal(advance(clock,at(11,15,15),9),null);
assert.equal(advance(clock,at(11,15,24),1).kind,"rest");
clock.respond("break",at(11,15,25));
assert.equal(advance(clock,at(11,15,25),4),null);
assert.equal(clock.activeMs,0);
assert.equal(advance(new PetCompanion(),at(11,23),20),null,"quiet hours do not generate a break reminder");
const fullscreen = new PetCompanion();
assert.equal(advance(fullscreen,at(11,15),15,{...active,fullscreen:true}),null);
assert.equal(fullscreen.tick(at(11,15,15)+5000,prefs,active).kind,"rest","fullscreen postpones, not consumes, delivery");
const slept = new PetCompanion(); advance(slept,at(11,15),14);
assert.equal(slept.tick(at(11,16),prefs,active),null); assert.equal(slept.activeMs,0);
assert.equal(advance(new PetCompanion(),at(11,15),20,active,{...prefs,rest_reminders:false}),null);
assert.equal(noticeMotion("noon"),"care"); assert.equal(noticeMotion("night"),"sleep");
console.log("pet companion: OK — time boundaries, delivery dedupe, idle/lock/sleep, fullscreen, quiet hours, snooze and break");

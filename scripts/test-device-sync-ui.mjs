#!/usr/bin/env node
// Start `pnpm exec vite preview --host 127.0.0.1 --port 4175` after pnpm build.
// Install playwright-core in a QA tools directory and set
// TOKENME_PLAYWRIGHT_MODULE to its absolute module entry path when necessary.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.TOKENME_PLAYWRIGHT_MODULE || "playwright-core");
const baseUrl = process.env.TOKENME_QA_URL || "http://127.0.0.1:4175";
const browser = await chromium.launch({ channel: "msedge", headless: true });
let passed = 0;

async function scenario(name, options, verify) {
  const context = await browser.newContext({ viewport: { width: 430, height: 720 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on("pageerror", error => pageErrors.push(error.message));
  await page.addInitScript(options => {
    const state = {
      primary_id: "primary-fixture", role: options.role || "secondary", service_running: true,
      service_health: "running", endpoint: "https://192.0.2.1:47821", certificate_pin: "11".repeat(32),
      stream_id: options.authError ? null : "stream-fixture", data_epoch: 1, pending: 0, in_flight: 0,
      pending_bytes: 0, last_error: options.authError ? "HTTP status 401: device is not authorized" : null,
      discovery_error: null, last_connected_at_ms: null, operation_state: "success", last_checked_at_ms: 1,
      invite_url: null, invite_expires_at_ms: null, pairing_pending: false, discovered_primaries: [],
      devices: [{ device_id: "secondary-fixture", role: "secondary", origin: "fixture", display_name: "Fixture", os: "test", status: "paired", data_epoch: 1, active_stream_id: "stream-fixture", last_generation: 1, last_merged_at_ms: null, snapshot_complete: true }],
      pairing_requests: [],
    };
    const qa = window.__QA__ = { state, calls: [], pollStarted: false, reads: 0 };
    const empty = () => ({ ...state, role: "unconfigured", service_running: false, operation_state: "idle", last_error: null, devices: [] });
    window.__TOKENME_SYNC_STUB__ = async request => {
      qa.calls.push(request?.action || "snapshot");
      if (!request) {
        qa.reads++;
        if (options.restoreDelay && qa.reads <= 3) return empty();
      }
      if (request?.action === "status" && options.delayPoll && !qa.pollStarted) {
        qa.pollStarted = true;
        const previous = structuredClone(state);
        await new Promise(resolve => { qa.releasePoll = resolve; });
        return previous;
      }
      if (request?.action === "stop" || request?.action === "unpair") {
        state.service_running = false;
        state.operation_state = state.role === "primary" ? "offline" : "idle";
        if (state.role !== "primary") state.role = "unconfigured";
        state.last_error = null;
      }
      if (request?.action === "start_primary") Object.assign(state, { role: "primary", service_running: true, operation_state: "success" });
      if (request?.action === "pause") state.devices[0].status = "paused";
      if (request?.action === "resume") state.devices[0].status = "paired";
      return structuredClone(state);
    };
  }, options);
  try {
    await page.goto(`${baseUrl}/?lang=zh`);
    await page.getByRole("button", { name: "设备同步", exact: true }).click();
    await verify(page);
    assert.deepEqual(pageErrors, []);
    console.log(`PASS ${name}`);
    passed++;
  } finally { await context.close(); }
}

try {
  await scenario("stop preempts polling and ignores its stale result", { delayPoll: true }, async page => {
    await page.waitForFunction(() => window.__QA__.pollStarted, null, { timeout: 12_000 });
    const stop = page.getByRole("button", { name: "停止并断开" });
    assert.equal(await stop.isEnabled(), true);
    await stop.click();
    await page.locator(".sync-choice-grid").waitFor();
    await page.evaluate(() => window.__QA__.releasePoll());
    await page.waitForTimeout(150);
    assert.equal(await page.locator(".sync-choice-grid").count(), 1);
    assert.equal(await page.locator(".rolecard").count(), 0);
    assert.equal(await page.evaluate(() => window.__QA__.calls.filter(a => a === "stop").length), 1);
    assert.equal(await page.evaluate(() => window.__QA__.calls.filter(a => a === "status").length), 1);
  });
  await scenario("primary stop returns to setup and can restart", { role: "primary" }, async page => {
    await page.getByRole("button", { name: "停止并断开" }).click();
    await page.locator(".sync-choice-grid").waitFor();
    await page.locator(".sync-choice-card").first().click();
    await page.getByRole("button", { name: "下一步", exact: true }).click();
    await page.locator(".rolecard").waitFor();
    assert.equal(await page.locator(".role-state").innerText(), "同步中");
  });
  await scenario("secondary auth failure clears on stop", { authError: true }, async page => {
    await page.locator(".sheet-notice").filter({ hasText: "主端尚未批准" }).waitFor();
    await page.getByRole("button", { name: "停止并断开" }).click();
    await page.locator(".sync-choice-grid").waitFor();
    assert.equal(await page.locator('.sheet-notice[role="alert"]').count(), 0);
  });
  await scenario("primary pause and resume reach backend actions", { role: "primary" }, async page => {
    await page.getByRole("button", { name: "暂停", exact: true }).click();
    await page.getByRole("button", { name: "恢复", exact: true }).click();
    assert.deepEqual(await page.evaluate(() => window.__QA__.calls.filter(a => a === "pause" || a === "resume")), ["pause", "resume"]);
  });
  await scenario("persisted secondary loads without offering role selection", { restoreDelay: true }, async page => {
    await page.locator(".rolecard").waitFor();
    assert.equal(await page.locator(".sync-choice-grid").count(), 0);
  });
  console.log(`device-sync-ui: ${passed} scenarios passed`);
} finally { await browser.close(); }

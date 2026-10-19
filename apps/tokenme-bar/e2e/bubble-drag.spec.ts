import { test, expect, type Page } from "@playwright/test";
import { FIXTURE } from "../src/fixture";
import { DEFAULT_PET_PREFERENCES } from "../src/lib/petCompanion";
import { installTauriMock, type MockCall, type MockHandle } from "./tauri-mock";

/**
 * The bubble drag bug, pinned end to end.
 *
 * On Windows the pet window is dragged by a Rust loop that moves the HWND with
 * the cursor, so the webview sees pointer events whose **client** coordinates
 * never change while the pointer physically travels. The old code decided
 * "click vs drag" from those client deltas, read every drag as a fresh click,
 * and popped the panel open on release. These specs install a fake Tauri IPC
 * backend, render the real `BubbleApp`, and dispatch exactly that pointer
 * shape (fixed client point, moving screen point) plus the `bubble-drag-ended`
 * verdict the Rust loop now emits.
 */

const SETTINGS = {
  autostart: false,
  refresh_secs: 60,
  theme: "light",
  show_money: true,
  quota_display_mode: "used",
  ttft_enabled: false,
  show_empty_tools: false,
  bubble_enabled: true,
  bubble_skin: "waterdrop",
  pet_companion: DEFAULT_PET_PREFERENCES,
  host_exit_pause: false,
  auto_checkin: false,
  quota_polling: true,
  quota_probes_off: [],
  notify_tiers: "both",
  notify_muted: [],
  auto_update_check: false,
  version: "0.1.8",
  build: "e2e",
};

declare global {
  interface Window {
    __tauriMock: MockHandle;
  }
}

test.beforeEach(async ({ context, page }) => {
  await context.addInitScript(installTauriMock, { report: FIXTURE, settings: SETTINGS });
  await page.goto("/");
  await page.waitForSelector(".bubble-app");
  // The browser keeps a real cursor at the viewport corner; when the dock
  // commits and the element changes size under it, Chromium re-evaluates
  // hover and fires a synthetic mouseover the OS would never send for a
  // moving window. Cut real hit-testing so only the pointer events this spec
  // dispatches reach the component (programmatic events still do).
  await page.evaluate(() => {
    document.documentElement.style.pointerEvents = "none";
  });
});

const calls = (page: Page) => page.evaluate(() => window.__tauriMock.calls as MockCall[]);
const commands = async (page: Page) => (await calls(page)).map((call) => call.cmd);
const emit = (page: Page, event: string, payload?: unknown) =>
  page.evaluate(([name, data]) => window.__tauriMock.emit(name, data), [event, payload] as const);

type PointerOpts = {
  clientX: number;
  clientY: number;
  screenX: number;
  screenY: number;
  button?: number;
  buttons?: number;
};

/** Dispatches a pointer event the way WebView2 delivers it mid-drag. */
async function pointer(page: Page, type: string, opts: PointerOpts) {
  await page.evaluate(
    ([type, opts]) => {
      const el = document.querySelector(".bubble-app");
      if (!el) throw new Error("bubble not mounted");
      el.dispatchEvent(new PointerEvent(type, {
        bubbles: true,
        cancelable: true,
        pointerId: 1,
        pointerType: "mouse",
        isPrimary: true,
        button: opts.button ?? 0,
        buttons: opts.buttons ?? 1,
        clientX: opts.clientX,
        clientY: opts.clientY,
        screenX: opts.screenX,
        screenY: opts.screenY,
      }));
    },
    [type, opts] as const,
  );
}

/** Presses the pet and waits until the native drag session has armed. */
async function press(page: Page) {
  await pointer(page, "pointerdown", { clientX: 200, clientY: 200, screenX: 200, screenY: 200 });
  await expect.poll(async () => (await commands(page)).includes("begin_bubble_drag")).toBe(true);
}

test("dragging the pet never opens the panel", async ({ page }) => {
  await press(page);
  // The native loop moves the window with the cursor: client stays put, screen
  // travels. Then the drop lands near the left edge.
  await page.evaluate(() => window.__tauriMock.setWindow(4, 300));
  await pointer(page, "pointermove", { clientX: 200, clientY: 200, screenX: 520, screenY: 360 });
  await pointer(page, "pointerup", { clientX: 200, clientY: 200, screenX: 520, screenY: 360, buttons: 0 });
  await emit(page, "bubble-drag-ended", true);

  await page.waitForTimeout(300);
  expect(await commands(page)).not.toContain("show_panel");
  // It did the drag's real work instead: docked back to the nearest edge.
  await expect.poll(async () => {
    const positions = await page.evaluate(() =>
      window.__tauriMock.calls
        .filter((call) => call.cmd === "plugin:window|set_position")
        .map((call) => {
          const value = (call.args as { value: { x: number; Physical?: { x: number } } }).value;
          return value.Physical?.x ?? value.x;
        }),
    );
    return positions.some((x) => x < 0);
  }).toBe(true);
  await expect(page.locator(".bubble-app")).toHaveAttribute("data-dock", "left");
});

test("a drag whose pointerup is swallowed by the native loop still only docks", async ({ page }) => {
  await press(page);
  await page.evaluate(() => window.__tauriMock.setWindow(4, 300));
  await pointer(page, "pointermove", { clientX: 200, clientY: 200, screenX: 520, screenY: 360 });
  // No pointerup reaches the webview; the Rust loop reports the drop alone.
  await emit(page, "bubble-drag-ended", true);

  await page.waitForTimeout(200);
  expect(await commands(page)).not.toContain("show_panel");
  await expect(page.locator(".bubble-app")).toHaveAttribute("data-dock", "left");
});

test("a plain click still opens the panel exactly once", async ({ page }) => {
  await press(page);
  // No travel: same client AND same screen point. The native loop reports an
  // unmoved session.
  await pointer(page, "pointerup", { clientX: 200, clientY: 200, screenX: 200, screenY: 200, buttons: 0 });
  await emit(page, "bubble-drag-ended", false);

  await expect.poll(async () => (await commands(page)).filter((cmd) => cmd === "show_panel").length).toBe(1);
  // The silent-session verdict must not add a second toggle or a stray dock.
  await page.waitForTimeout(250);
  expect((await commands(page)).filter((cmd) => cmd === "show_panel").length).toBe(1);
  expect(await commands(page)).not.toContain("plugin:window|set_position");
});

test("a cancelled pointer is never a click", async ({ page }) => {
  await press(page);
  // The browser can cancel the stream instead of delivering a release (the OS
  // taking the pointer, capture lost). A cancellation is not a click, and an
  // unmoved one is not a drop either: no panel, no dock.
  await pointer(page, "pointercancel", { clientX: 200, clientY: 200, screenX: 200, screenY: 200, buttons: 0 });
  await emit(page, "bubble-drag-ended", false);
  await page.waitForTimeout(250);
  expect(await commands(page)).not.toContain("show_panel");
  expect(await commands(page)).not.toContain("plugin:window|set_position");
});

test("a cancelled drag still settles on its edge", async ({ page }) => {
  await press(page);
  await page.evaluate(() => window.__tauriMock.setWindow(4, 300));
  await pointer(page, "pointermove", { clientX: 200, clientY: 200, screenX: 520, screenY: 360 });
  await pointer(page, "pointercancel", { clientX: 200, clientY: 200, screenX: 520, screenY: 360, buttons: 0 });
  await page.waitForTimeout(300);
  expect(await commands(page)).not.toContain("show_panel");
  await expect(page.locator(".bubble-app")).toHaveAttribute("data-dock", "left");
});

test("an unmoved native session leaves hover expand unlatched", async ({ page }) => {
  await press(page);
  // The Rust loop ends without moving the window (the silent-session case the
  // always-emit change targets). Before the fix this left `dragging` latched,
  // so hover could never expand the pet again.
  await emit(page, "bubble-drag-ended", false);
  await page.waitForTimeout(50);

  await page.evaluate(() => {
    const el = document.querySelector(".bubble-app");
    el?.dispatchEvent(new MouseEvent("mouseover", { bubbles: true }));
  });
  await expect(page.locator(".bubble-app")).toHaveAttribute("data-expanded", "true");
});

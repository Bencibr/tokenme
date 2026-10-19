import { defineConfig } from "@playwright/test";

/**
 * Browser E2E for the Windows utility surface (the pet bubble).
 *
 * The bubble's native window is Windows-only, so the harness renders the real
 * `BubbleApp` with a fake Tauri IPC backend (`e2e/tauri-mock.ts`) and drives
 * it with the pointer-event shape WebView2 produces mid-drag. The dev server
 * is Vite's, the same entrypoint the bundle ships.
 */
export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  use: {
    baseURL: "http://127.0.0.1:1420",
    // Windows is the platform whose drag loop the specs model: it selects the
    // non-mac pointer path and the Windows click contract in BubbleApp.
    userAgent:
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
    viewport: { width: 800, height: 600 },
    headless: true,
  },
  webServer: {
    command: "node ./node_modules/vite/bin/vite.js --port 1420 --strictPort",
    url: "http://127.0.0.1:1420",
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
  },
});

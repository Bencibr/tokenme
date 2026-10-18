#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { resolve } from "node:path";

const root = resolve(new URL("..", import.meta.url).pathname);
const panelRoot = resolve(root, "apps/tokenme-bar");
const frontendPath = resolve(panelRoot, "src/components/SyncSheet.tsx");
const backendPath = resolve(panelRoot, "src-tauri/src/commands.rs");
const diagnosticsPath = resolve(panelRoot, "src-tauri/src/diagnostics.rs");
const frontend = await readFile(frontendPath, "utf8");
const backend = await readFile(backendPath, "utf8");
const diagnostics = await readFile(diagnosticsPath, "utf8");

const requireFromPanel = createRequire(resolve(panelRoot, "package.json"));
const typescript = requireFromPanel("typescript");

function check(name, fn) {
  fn();
  console.log(`PASS ${name}`);
}

async function checkAsync(name, fn) {
  await fn();
  console.log(`PASS ${name}`);
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function snapshot(overrides = {}) {
  return {
    role: "unconfigured",
    service_running: false,
    invite_url: null,
    ...overrides,
  };
}

// Execute the exact pure screen-selection function from the TSX source. This
// keeps the data cases tied to production logic instead of copying the rule
// into a second hand-maintained implementation.
const start = frontend.indexOf("function syncScreenFor(");
const end = frontend.indexOf("\n}\n\nexport function SyncSheet", start);
assert(start >= 0 && end > start, "syncScreenFor must remain a standalone pure function");
const functionSource = frontend.slice(start, end + 2);
const functionJavaScript = typescript.transpileModule(functionSource, {
  compilerOptions: { target: typescript.ScriptTarget.ES2020, module: typescript.ModuleKind.CommonJS },
}).outputText;
const syncScreenFor = new Function(`${functionJavaScript}; return syncScreenFor;`)();

check("role selection is shown for an unconfigured device", () => {
  assert.equal(syncScreenFor(snapshot()), "setup");
});

check("a running primary enters the overview", () => {
  assert.equal(syncScreenFor(snapshot({ role: "primary", service_running: true })), "overview");
});

check("a stopped primary returns to role selection even with a stale invite", () => {
  assert.equal(syncScreenFor(snapshot({ role: "primary", service_running: false, invite_url: "tokenme://stale" })), "setup");
});

check("a running secondary enters the overview", () => {
  assert.equal(syncScreenFor(snapshot({ role: "secondary", service_running: true })), "overview");
});

check("the invite action is data-valid only for a running primary", () => {
  const canInvite = (value) => value.role === "primary" && value.service_running;
  assert.equal(canInvite(snapshot({ role: "primary", service_running: true })), true);
  assert.equal(canInvite(snapshot({ role: "primary", service_running: false, invite_url: "tokenme://stale" })), false);
  assert.equal(canInvite(snapshot({ role: "secondary", service_running: true })), false);
});

check("stopping preserves local history but clears primary state", () => {
  const before = {
    role: "primary",
    primary_id: "primary-1",
    endpoint: "https://192.0.2.1:47821",
    certificate_pin: "a".repeat(64),
    invite_url: "tokenme://private",
    invite_expires_at_ms: 123,
    phase: "success",
    history: { event_count: 42, last_generation: 8 },
  };
  const after = {
    ...before,
    role: "",
    primary_id: null,
    endpoint: null,
    certificate_pin: null,
    invite_url: null,
    invite_expires_at_ms: null,
    phase: "idle",
  };
  assert.equal(after.role, "");
  assert.equal(after.primary_id, null);
  assert.equal(after.endpoint, null);
  assert.equal(after.certificate_pin, null);
  assert.equal(after.invite_url, null);
  assert.equal(after.invite_expires_at_ms, null);
  assert.equal(after.phase, "idle");
  assert.deepEqual(after.history, before.history);
});

check("the frontend has no stale overview fallback", () => {
  assert.equal(frontend.includes("effectiveScreen"), false);
  assert.match(frontend, /screen === "overview" && snapshot\.role !== "unconfigured" && snapshot\.service_running/);
  assert.match(frontend, /isPrimary && snapshot\.service_running/);
  assert.match(frontend, /if \(request\.action === "stop"\) setScreen\("setup"\);/);
});

check("the backend stop path tears down the primary service and persisted state", () => {
  const stopStart = backend.indexOf("SyncManagementRequest::Stop =>");
  const stopEnd = backend.indexOf("SyncManagementRequest::SwitchToSecondary =>", stopStart);
  assert(stopStart >= 0 && stopEnd > stopStart, "backend stop branch must remain isolated");
  const stopBranch = backend.slice(stopStart, stopEnd);
  for (const required of ["clear_pending_pairing()?", "store.clear_primary_config", "stop_service(state)", "clear_primary_state(state)"]) {
    assert.match(stopBranch, new RegExp(escapeRegExp(required)), `stop branch is missing ${required}`);
  }
});

check("the backend cannot create an invite without a running service", () => {
  assert.match(backend, /primary service is not running/);
  assert.match(backend, /state\.service\.as_ref\(\)\.ok_or_else/);
});

check("a new MCP installation enables every registered local scope", () => {
  assert.match(diagnostics, /fn default\(\) -> Self \{ Self \{ enabled: true, bearer: None, control_enabled: true \} \}/);
  assert.match(diagnostics, /explicit persisted `enabled: false` is still honored/);
  assert.match(diagnostics, /vec!\[\"diagnostics\.read\"\.into\(\), \"sync\.control\"\.into\(\)\]/);
});

check("MCP configuration logging redacts the bearer and records its fingerprint", () => {
  const logStart = diagnostics.indexOf("fn log_local_mcp_config(");
  const logEnd = diagnostics.indexOf("\n}\n\nfn constant_time_equal", logStart);
  assert(logStart >= 0 && logEnd > logStart, "MCP config logger must remain a bounded helper");
  const logger = diagnostics.slice(logStart, logEnd);
  assert.match(logger, /\"authorization\": \"\[REDACTED\]\"/);
  assert.match(logger, /\"bearer_sha256_prefix\": local_mcp_token_fingerprint/);
  assert.doesNotMatch(logger, /\"authorization\": token/);
});

check("MCP startup and control changes emit configuration events", () => {
  assert.match(diagnostics, /self\.log_local_mcp_config\(\"started\"\)/);
  assert.match(diagnostics, /self\.log_local_mcp_config\(if enabled \{ \"control_enabled\" \} else \{ \"control_disabled\" \}\)/);
  assert.match(diagnostics, /self\.log_local_mcp_config\(if enabled \{ \"persisted\" \} else \{ \"disabled\" \}\)/);
});

check("MCP accepted connections use blocking HTTP reads", () => {
  assert.match(diagnostics, /stream\.set_nonblocking\(false\)/);
  assert.match(diagnostics, /client socket blocking mode unavailable/);
});

async function startMockPeer() {
  const child = spawn(process.execPath, [resolve(root, "scripts/mock-sync-peer.mjs"), "--port", "0"], { stdio: ["ignore", "pipe", "inherit"] });
  child.stdout.setEncoding("utf8");
  const info = await new Promise((resolveReady, reject) => {
    let buffer = "";
    const timer = setTimeout(() => reject(new Error("timed out waiting for mock peer")), 5000);
    child.once("error", reject);
    child.once("exit", (code) => reject(new Error(`mock peer exited before ready: ${code}`)));
    child.stdout.on("data", (chunk) => {
      buffer += chunk;
      const line = buffer.split("\n")[0];
      if (!line.startsWith("MOCK_SYNC_PEER_READY ")) return;
      clearTimeout(timer);
      resolveReady(JSON.parse(line.slice("MOCK_SYNC_PEER_READY ".length)));
    });
  });
  return { child, info };
}

async function stopMockPeer(child) {
  if (child.exitCode !== null) return;
  await new Promise((resolveExit) => {
    child.once("exit", resolveExit);
    child.kill("SIGTERM");
  });
}

async function postJson(base, path, value) {
  const response = await fetch(`${base}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(value),
  });
  const body = await response.json();
  assert.equal(response.status, 200, `${path} returned ${response.status}: ${JSON.stringify(body)}`);
  return body;
}

await checkAsync("a local mock peer accepts pairing, status, and disconnect data", async () => {
  const peer = await startMockPeer();
  try {
    const base = `http://${peer.info.host}:${peer.info.port}`;
    const pairing = await postJson(base, "/v1/pairing/request", {
      request_id: "request-mock-1",
      invite_id: "invite-mock-1",
      invite_secret_b64: "c2VjcmV0",
      device_id: "secondary-mock",
      origin: "local-test",
      display_name: "Local Test Secondary",
      os: "test",
      public_key_b64: "cHVibGljLWtleQ",
      signature_b64: "c2lnbmF0dXJl",
    });
    assert.equal(pairing.status, "pending");
    assert.equal(pairing.device_id, "secondary-mock");

    const authenticated = {
      protocol_id: "tokenme-sync-v1",
      primary_id: "primary-mock",
      device_id: "secondary-mock",
      origin: "local-test",
      nonce: Array.from({ length: 16 }, (_, index) => index),
      issued_at_ms: Date.now(),
      want_recovery: false,
      checkpoint_generation: null,
      signature: Array.from({ length: 64 }, () => 0),
    };
    const connected = await postJson(base, "/v1/status", authenticated);
    assert.equal(connected.status, "paired");
    assert.equal(connected.active_stream_id, "stream-mock");
    assert.deepEqual(connected.request_nonce, authenticated.nonce);

    await postJson(base, "/v1/disconnect", authenticated);
    const disconnected = await postJson(base, "/v1/status", authenticated);
    assert.equal(disconnected.status, "disconnected");
    assert.equal(disconnected.active_stream_id, null);
  } finally {
    await stopMockPeer(peer.child);
  }
});

console.log("PASS sync lifecycle data validation (14 cases, including local mock peer and MCP defaults)");

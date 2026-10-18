#!/usr/bin/env node

import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";

const root = resolve(new URL("..", import.meta.url).pathname);
const primaryScript = resolve(root, "scripts/mock-tokenme-primary.mjs");
const mcpConfigCandidates = process.platform === "darwin"
  ? [join(homedir(), "Library", "Application Support", "tokenme", "mcp.json"), join(homedir(), "Library", "Preferences", "tokenme", "mcp.json")]
  : [join(homedir(), ".config", "tokenme", "mcp.json")];
const logPath = process.platform === "darwin"
  ? join(homedir(), "Library", "Logs", "tokenme", "panel.log")
  : join(homedir(), ".local", "state", "tokenme", "logs", "panel.log");
const keep = process.argv.includes("--keep");
const verifyStop = process.argv.includes("--verify-stop");
const requireRows = process.argv.includes("--require-rows");
const verifyDiscovery = process.argv.includes("--verify-discovery");
const pauseAfterRecovery = process.argv.includes("--pause-after-recovery");
assert(!(keep && verifyStop), "--keep and --verify-stop cannot be combined");

async function readMcpConfig() {
  const path = mcpConfigCandidates.find(existsSync);
  assert(path, `MCP state file was not created; checked ${mcpConfigCandidates.join(", ")}`);
  const config = JSON.parse(await readFile(path, "utf8"));
  assert.equal(config.enabled, true, "MCP must be enabled");
  assert.equal(config.control_enabled, true, "MCP sync control must be enabled");
  assert.match(config.bearer, /^tm_[A-Za-z0-9_-]{40,}$/);
  const log = await readFile(logPath, "utf8");
  const endpoint = log.split("\n").reverse()
    .filter((line) => line.includes("mcp.config"))
    .map((line) => line.slice(line.indexOf("mcp.config ") + "mcp.config ".length))
    .map((line) => { try { return JSON.parse(line); } catch { return null; } })
    .find((value) => value?.enabled && typeof value.endpoint === "string")?.endpoint;
  assert.match(endpoint, /^http:\/\/[^:]+:\d+\/mcp$/);
  return { endpoint, bearer: config.bearer };
}

function startPrimary() {
  const child = spawn(process.execPath, [primaryScript, ...(verifyDiscovery ? ["--mdns"] : [])], { stdio: ["ignore", "pipe", "pipe"] });
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  const events = new Map();
  let stdoutRemainder = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => {
    stdoutRemainder += chunk;
    const lines = stdoutRemainder.split("\n");
    stdoutRemainder = lines.pop() ?? "";
    for (const line of lines) {
      const separator = line.indexOf(" ");
      if (separator < 0) continue;
      try {
        const name = line.slice(0, separator);
        const values = events.get(name) ?? [];
        values.push(JSON.parse(line.slice(separator + 1)));
        events.set(name, values);
      } catch { /* wait for a valid event */ }
    }
  });
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  return { child, events, getStderr: () => stderr };
}

function wait(milliseconds) { return new Promise((resolveWait) => setTimeout(resolveWait, milliseconds)); }

function waitForInput() {
  return new Promise((resolveInput) => process.stdin.once("data", resolveInput));
}

async function waitFor(predicate, label, attempts = 120) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    const value = await predicate();
    if (value) return value;
    await wait(250);
  }
  throw new Error(`timed out waiting for ${label}`);
}

async function waitForEvent(primary, name, after = 0, attempts = 120) {
  return waitFor(() => {
    const values = primary.events.get(name) ?? [];
    if (values.length > after) return values[after];
    if (primary.child.exitCode !== null) throw new Error(`mock primary exited with ${primary.child.exitCode}: ${primary.getStderr()}`);
    return null;
  }, name, attempts);
}

function stopPrimary(primary) {
  if (!primary || primary.child.exitCode !== null) return Promise.resolve();
  primary.child.kill("SIGTERM");
  return new Promise((resolveExit) => primary.child.once("exit", resolveExit));
}

const config = await readMcpConfig();
let sequence = 0;
let sessionId = null;

async function rpc(method, params = {}, includeId = true) {
  const id = includeId ? `secondary-test-${++sequence}` : undefined;
  const payload = { jsonrpc: "2.0", ...(includeId ? { id } : {}), method, params };
  const response = await fetch(config.endpoint, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${config.bearer}`,
      ...(sessionId ? { "mcp-session-id": sessionId } : {}),
    },
    body: JSON.stringify(payload),
  });
  const nextSession = response.headers.get("mcp-session-id");
  if (nextSession) sessionId = nextSession;
  const raw = await response.text();
  const body = raw ? JSON.parse(raw) : null;
  assert.notEqual(response.status, 401, `${method} was unauthorized`);
  if (body?.error) throw new Error(`${method} failed: ${body.error.message}`);
  return body?.result?.structuredContent ?? body?.result ?? null;
}

async function openSession() {
  sessionId = null;
  await rpc("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "tokenme-secondary-closed-loop-test", version: "1" },
  });
  await rpc("notifications/initialized", {}, false);
}

async function callTool(name, argumentsValue = {}) {
  return rpc("tools/call", { name, arguments: argumentsValue });
}

let primary = null;
let paired = false;
try {
  await openSession();
  const tools = await rpc("tools/list");
  const names = new Set((tools.tools ?? []).map((tool) => tool.name));
  for (const required of ["sync.get_status", "sync.pair", ...(verifyDiscovery ? ["sync.discover"] : []), "ui.open_sync_sheet", ...(verifyStop ? ["sync.stop_secondary"] : [])]) {
    assert(names.has(required), `required MCP tool missing: ${required}`);
  }
  let status = await callTool("sync.get_status");
  assert.equal(status.role, "unconfigured", "secondary test must start from an unconfigured app");
  assert.equal(status.service_running, false);
  console.log("PASS MCP secondary prerequisites (unconfigured app + sync control)");

  primary = startPrimary();
  const mockPrimaryPid = primary.child.pid;
  const ready = await waitForEvent(primary, "MOCK_TOKENME_PRIMARY_READY");
  assert.match(ready.endpoint, verifyDiscovery ? /^https:\/\/[^:]+:\d+$/ : /^https:\/\/127\.0\.0\.1:\d+$/);
  assert.match(ready.invite, /^tokenme:\/\/device-pair\?/);

  if (verifyDiscovery) {
    assert.equal(ready.mdns, true, "the discovery test primary must publish mDNS");
    let lastCandidates = [];
    let discovered;
    try {
      discovered = await waitFor(async () => {
        const value = await callTool("sync.discover");
        lastCandidates = value.discovered_primaries ?? [];
        const match = lastCandidates.find((item) => item.primary_id === ready.primary_id) ?? null;
        return match ? { value, match } : null;
      }, "real mDNS discovery of the script primary", 20);
    } catch (error) {
      if (error instanceof Error) error.message += `; last_candidates=${JSON.stringify(lastCandidates)}`;
      throw error;
    }
    assert.equal(discovered.value.has_discovery_error, false);
    assert.equal(discovered.match.endpoint, ready.endpoint);
    assert.equal(discovered.match.certificate_pin, ready.certificate_pin);
    assert.equal(discovered.match.identity_conflict, false);
    console.log(`PASS real mDNS discovery (primary_id=${ready.primary_id}, endpoint=${discovered.match.endpoint})`);
    // The MCP control surface intentionally exposes the complete invite pair
    // action, not a second pairing permission. The UI uses pair_discovered
    // directly after the same discovery result has been shown to the user.
    status = await callTool("sync.pair", {
      invite: ready.invite,
      origin: "tokenme-secondary-discovery-test",
      display_name: "TokenMe Secondary Discovery Test",
      os: "macos",
    });
  } else {
    status = await callTool("sync.pair", {
      invite: ready.invite,
      origin: "tokenme-secondary-test",
      display_name: "TokenMe Secondary Test",
      os: "macos",
    });
  }
  paired = true;
  assert.equal(status.role, "secondary");
  assert.equal(status.service_running, true);
  assert.equal(status.primary_id, ready.primary_id);
  assert.equal(status.endpoint, ready.endpoint);
  console.log(`PASS MCP sync.pair (app is secondary -> ${ready.endpoint})`);

  const primaryPairing = await waitForEvent(primary, "MOCK_TOKENME_PRIMARY_PAIRING");
  assert.equal(primaryPairing.auto_approved, true);
  const firstStatus = await waitForEvent(primary, "MOCK_TOKENME_PRIMARY_STATUS");
  assert.equal(firstStatus.status, "paired");
  assert.equal(firstStatus.stream_id, "mock-stream");

  status = await waitFor(async () => {
    const value = await callTool("sync.get_status");
    if (value.role !== "secondary" || !value.service_running || value.stream_id !== "mock-stream" || value.data_epoch !== 1 || value.has_error || value.last_connected_at_ms == null) return null;
    return value;
  }, "secondary stream and connected status");
  assert.equal(status.service_health, "running");
  console.log(`PASS secondary runtime status (stream=${status.stream_id}, epoch=${status.data_epoch}, connected=${status.last_connected_at_ms})`);

  const upload = await waitForEvent(primary, "MOCK_TOKENME_PRIMARY_UPLOAD");
  assert.equal(upload.device_id, primaryPairing.device_id);
  assert.equal(upload.generation, 1);
  if (requireRows) {
    assert(upload.rows > 0, "live secondary upload must contain at least one result row");
    assert.deepEqual(upload.first_row && {
      tool: upload.first_row.tool,
      in: upload.first_row.in,
      out: upload.first_row.out,
    }, { tool: "codex", in: 13, out: 9 });
    assert.match(upload.first_row.source, /\.jsonl$/i, "real adapter source must identify the session JSONL file");
  }
  assert.equal(upload.snapshot_complete, true);
  status = await waitFor(async () => {
    const value = await callTool("sync.get_status");
    return value.role === "secondary" && value.service_running && value.pending === 0 && value.in_flight === 0 && !value.has_error ? value : null;
  }, "secondary upload ACK and drained outbox");
  console.log(`PASS secondary snapshot upload/ACK (generation=${upload.generation}, rows=${upload.rows}, pending=${status.pending}, in_flight=${status.in_flight})`);

  const checkpointStatus = await waitFor(async () => {
    const values = primary.events.get("MOCK_TOKENME_PRIMARY_STATUS") ?? [];
    return values.find((value) => value.checkpoint_generation === upload.generation) ?? null;
  }, "secondary checkpoint polling");
  assert.equal(checkpointStatus.checkpoint_generation, upload.generation);
  console.log(`PASS secondary checkpoint polling (checkpoint_generation=${checkpointStatus.checkpoint_generation ?? "null"})`);

  const sheet = await callTool("ui.open_sync_sheet");
  assert.equal(sheet.opened, true);
  console.log("PASS MCP UI control (sync sheet opened while app is secondary)");
  if (verifyStop) {
    primary.child.kill("SIGUSR1");
    await waitForEvent(primary, "MOCK_TOKENME_PRIMARY_DOWN");
    await waitFor(async () => {
      const value = await callTool("sync.get_status");
      return value.role === "secondary" && value.service_running && value.has_error ? value : null;
    }, "secondary transport error after the primary exits");
    status = await callTool("sync.get_status");
    assert.equal(status.service_running, true, "a transient primary outage must not stop the secondary runtime");
    assert.equal(status.role, "secondary", "a transient primary outage must retain the secondary binding");
    console.log("PASS secondary self-heal holding state (primary outage leaves retryable secondary running)");

    primary.child.kill("SIGUSR2");
    await waitForEvent(primary, "MOCK_TOKENME_PRIMARY_RECOVERED");
    status = await waitFor(async () => {
      const value = await callTool("sync.get_status");
      return value.role === "secondary" && value.service_running && !value.has_error && value.last_connected_at_ms != null ? value : null;
    }, "secondary automatic recovery after the primary returns");
    assert.equal(status.service_health, "running");
    console.log(`PASS secondary self-heal recovery (stream=${status.stream_id}, connected=${status.last_connected_at_ms})`);

    if (pauseAfterRecovery) {
      console.log("PAUSED_AFTER_RECOVERY press-enter-to-continue");
      await waitForInput();
    }

    primary.child.kill("SIGUSR1");
    await waitForEvent(primary, "MOCK_TOKENME_PRIMARY_DOWN", 1);
    await wait(300);
    await callTool("sync.stop_secondary");
    // Stop revokes active diagnostics sessions by design. Reconnect before
    // reading the final state so this assertion observes the real post-stop
    // state rather than treating the expected revocation as a test failure.
    await openSession();
    status = await waitFor(async () => {
      const value = await callTool("sync.get_status");
      return value.role === "unconfigured" && !value.service_running ? value : null;
    }, "local stop after the primary becomes unreachable");
    assert.equal(status.role, "unconfigured", "stop must clear the secondary binding when the primary is unreachable");
    assert.equal(status.service_running, false, "stop must leave no secondary service running");
    assert.equal(status.has_error, false, "remote disconnect failure must not become a user-facing error");
    assert.ok(!status.last_error, "remote disconnect failure must not persist as last_error");
    const stoppedSheet = await callTool("ui.open_sync_sheet");
    assert.equal(stoppedSheet.opened, true);
    const panelLog = await readFile(logPath, "utf8");
    assert.match(panelLog, /secondary disconnect notification failed after local stop; local binding cleared/);
    console.log("PASS explicit stop after outage (local binding cleared, no UI error, diagnostic logged)");
  }
  console.log(`PASS TokenMe secondary closed loop (mock_primary_pid=${mockPrimaryPid}, endpoint=${ready.endpoint})`);
  if (keep) {
    primary.child.stdout.destroy();
    primary.child.stderr.destroy();
    primary.child.unref();
    primary = null;
    console.log("KEEP secondary app + mock primary running for inspection");
  }
} finally {
  if (primary) await stopPrimary(primary);
}

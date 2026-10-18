#!/usr/bin/env node

import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";

const root = resolve(new URL("..", import.meta.url).pathname);
const peerScript = resolve(root, "scripts/mock-tokenme-peer.mjs");
const verifyDiscovery = process.argv.includes("--verify-discovery");
const mcpConfigCandidates = process.platform === "darwin"
  ? [join(homedir(), "Library", "Application Support", "tokenme", "mcp.json"), join(homedir(), "Library", "Preferences", "tokenme", "mcp.json")]
  : [join(homedir(), ".config", "tokenme", "mcp.json")];
const logPath = process.platform === "darwin"
  ? join(homedir(), "Library", "Logs", "tokenme", "panel.log")
  : join(homedir(), ".local", "state", "tokenme", "logs", "panel.log");

async function readMcpConfig() {
  const path = mcpConfigCandidates.find(existsSync);
  assert(path, `MCP state file was not created; checked ${mcpConfigCandidates.join(", ")}`);
  const config = JSON.parse(await readFile(path, "utf8"));
  assert.equal(config.enabled, true, "MCP must be enabled by default");
  assert.equal(config.control_enabled, true, "MCP LAN control must be enabled by default");
  assert.match(config.bearer, /^tm_[A-Za-z0-9_-]{40,}$/);
  const log = await readFile(logPath, "utf8");
  const endpoint = log.split("\n").reverse()
    .filter((line) => line.includes("mcp.config"))
    .map((line) => line.slice(line.indexOf("mcp.config ") + "mcp.config ".length))
    .map((line) => { try { return JSON.parse(line); } catch { return null; } })
    .find((value) => value?.enabled && typeof value.endpoint === "string")?.endpoint;
  assert.match(endpoint, /^http:\/\/[^:]+:\d+\/mcp$/);
  return { path, endpoint, bearer: config.bearer };
}

const config = await readMcpConfig();
let sequence = 0;
let sessionId = null;

async function rpc(method, params = {}, includeId = true) {
  const id = includeId ? `mcp-test-${++sequence}` : undefined;
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
    clientInfo: { name: "tokenme-closed-loop-test", version: "1" },
  });
  await rpc("notifications/initialized", {}, false);
}

async function callTool(name, argumentsValue = {}) {
  return rpc("tools/call", { name, arguments: argumentsValue });
}

function wait(milliseconds) {
  return new Promise((resolveWait) => setTimeout(resolveWait, milliseconds));
}

async function waitFor(predicate, label, attempts = 60) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    const value = await predicate();
    if (value) return value;
    await wait(250);
  }
  throw new Error(`timed out waiting for ${label}`);
}

function startPeer(endpoint, primaryId) {
  const child = spawn(process.execPath, [peerScript, "--endpoint", endpoint, "--primary-id", primaryId], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  let output = "";
  let errors = "";
  const events = new Map();
  child.stdout.on("data", (chunk) => {
    output += chunk;
    for (const line of output.split("\n").slice(0, -1)) {
      const separator = line.indexOf(" ");
      if (separator < 0) continue;
      const name = line.slice(0, separator);
      try { events.set(name, JSON.parse(line.slice(separator + 1))); } catch { /* wait for a complete line */ }
    }
    output = output.slice(output.lastIndexOf("\n") + 1);
  });
  child.stderr.on("data", (chunk) => { errors += chunk; });
  return { child, events, getErrors: () => errors };
}

async function waitForPeerEvent(peer, name, attempts = 100) {
  return waitFor(() => {
    if (peer.events.has(name)) return peer.events.get(name);
    if (peer.child.exitCode !== null) throw new Error(`mock peer exited with ${peer.child.exitCode}: ${peer.getErrors()}`);
    return null;
  }, name, attempts);
}

async function stopPeer(peer) {
  if (!peer || peer.child.exitCode !== null) return;
  peer.child.kill("SIGTERM");
  await new Promise((resolveExit) => peer.child.once("exit", resolveExit));
}

let peer = null;
let primaryRunning = false;
try {
  await openSession();
  const tools = await rpc("tools/list");
  const toolNames = new Set((tools.tools ?? []).map((tool) => tool.name));
  for (const required of ["sync.get_status", "sync.start_primary", "sync.approve_pairing", "sync.stop_primary", "logs.tail", ...(verifyDiscovery ? ["sync.discover"] : [])]) {
    assert(toolNames.has(required), `full-control MCP tool missing: ${required}`);
  }
  console.log(`PASS MCP initialize/tools-list (${tools.tools.length} tools, full control scope)`);

  let status = await callTool("sync.get_status");
  if (status.role === "primary" && status.service_running) {
    await callTool("sync.stop_primary");
    await openSession();
    status = await callTool("sync.get_status");
  }
  assert.notEqual(status.role, "secondary", "test requires an unpaired app; a persisted secondary must be stopped manually");

  const startPrimaryParams = {
    bind: verifyDiscovery ? "0.0.0.0:0" : "127.0.0.1:0",
    allow_lan: verifyDiscovery,
  };
  if (!verifyDiscovery) startPrimaryParams.advertise_host = "127.0.0.1";
  status = await callTool("sync.start_primary", startPrimaryParams);
  assert.equal(status.role, "primary");
  assert.equal(status.service_running, true);
  assert.match(status.endpoint, verifyDiscovery ? /^https:\/\/[^:]+:\d+$/ : /^https:\/\/127\.0\.0\.1:\d+$/);
  primaryRunning = true;
  console.log(`PASS MCP sync.start_primary (${status.endpoint})`);

  if (verifyDiscovery) {
    const primaryId = status.primary_id;
    const discovered = await waitFor(async () => {
      const value = await callTool("sync.discover");
      const match = value.discovered_primaries?.find((item) => item.primary_id === primaryId) ?? null;
      return match ? { value, match } : null;
    }, "real mDNS discovery of the running app primary", 20);
    assert.equal(discovered.value.has_discovery_error, false);
    assert.equal(discovered.match.primary_id, primaryId);
    assert.match(discovered.match.certificate_pin, /^[a-f0-9]{64}$/);
    assert.equal(discovered.match.endpoint, status.endpoint);
    assert.equal(discovered.match.identity_conflict, false);
    console.log(`PASS real mDNS discovery (primary_id=${primaryId}, endpoint=${discovered.match.endpoint})`);
  }

  peer = startPeer(status.endpoint, status.primary_id);
  const pairingEvent = await waitForPeerEvent(peer, "MOCK_TOKENME_PEER_PAIRING_REQUEST");
  const request = await waitFor(async () => {
    const value = await callTool("sync.get_requests");
    return value.requests.find((item) => item.device_id === pairingEvent.device_id) ?? null;
  }, "MCP-visible pairing request");
  assert.equal(request.status, "pending");
  await callTool("sync.approve_pairing", { request_id: request.request_id });
  console.log(`PASS MCP sync.approve_pairing (${request.device_id})`);

  const peerStatus = await waitForPeerEvent(peer, "MOCK_TOKENME_PEER_STATUS");
  assert.equal(peerStatus.status, "paired");
  const peerUpload = await waitForPeerEvent(peer, "MOCK_TOKENME_PEER_UPLOAD");
  assert.equal(peerUpload.rows, 1);
  assert.equal(peerUpload.inserted, 1);
  assert.equal(peerUpload.snapshot_complete, true);
  console.log(`PASS real primary merged remote result row (generation=${peerUpload.generation}, rows=${peerUpload.rows}, inserted=${peerUpload.inserted})`);
  await waitForPeerEvent(peer, "MOCK_TOKENME_PEER_DISCONNECTED");
  console.log(`PASS mock peer HTTPS pairing/status/disconnect (${peerStatus.stream_id ?? "no-stream"})`);

  status = await callTool("sync.get_status");
  const device = status.devices.find((item) => item.device_id === pairingEvent.device_id);
  assert(device, "MCP status did not expose the mock peer device");
  assert.equal(device.status, "revoked");
  console.log("PASS MCP sync.get_status reflects the peer disconnect");

  status = await callTool("sync.stop_primary");
  primaryRunning = false;
  assert.equal(status.service_running, false);
  await openSession();
  status = await callTool("sync.get_status");
  assert.equal(status.role, "unconfigured");
  assert.equal(status.service_running, false);
  console.log("PASS MCP sync.stop_primary clears the running primary and returns unconfigured state");

  const log = await readFile(logPath, "utf8");
  const configLines = log.split("\n").filter((line) => line.includes("mcp.config"));
  assert(configLines.length >= 2, "MCP configuration events were not written to panel.log");
  assert(configLines.some((line) => line.includes('"control_enabled":true') && line.includes('"sync.control"')));
  assert(configLines.every((line) => line.includes('"authorization":"[REDACTED]"')));
  assert(configLines.every((line) => !line.includes("tm_")), "MCP bearer appeared in a config log line");
  console.log(`PASS panel.log MCP config evidence (${configLines.length} events, bearer redacted)`);
} finally {
  await stopPeer(peer);
  if (primaryRunning) {
    try {
      await callTool("sync.stop_primary");
    } catch {
      // Preserve the original failure; the app can still be stopped by the
      // caller's process cleanup if the MCP session was revoked.
    }
  }
}

console.log("PASS TokenMe MCP closed loop (real app + local mock peer)");

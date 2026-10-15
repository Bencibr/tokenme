#!/usr/bin/env node

// Focused local acceptance gate for diagnostics MCP.  It reports protocol and
// static evidence separately from host/platform work that is not implemented
// or cannot be proved on this machine.  An UNVERIFIED line is intentional; it
// must not be converted into a PASS by a loopback or cross-compile result.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const read = (path) => readFileSync(join(root, path), "utf8");
const tauriDiagnostics = read("apps/tokenme-bar/src-tauri/src/diagnostics.rs");
const tauriLib = read("apps/tokenme-bar/src-tauri/src/lib.rs");
const tauriCommands = read("apps/tokenme-bar/src-tauri/src/commands.rs");
const transport = read("crates/usage-device-sync/src/transport.rs");
const syncSheet = read("apps/tokenme-bar/src/components/SyncSheet.tsx");
const cliArgs = read("crates/usage-cli/src/args.rs");
const cliMain = read("crates/usage-cli/src/main.rs");
const cliCommands = read("crates/usage-cli/src/commands.rs");
const diagnosticsCore = read("crates/usage-device-sync/src/diagnostics.rs");

let passed = 0;
let failed = 0;
let unverified = 0;

function result(status, name, detail = "") {
  if (status === "PASS") passed += 1;
  if (status === "FAIL") failed += 1;
  if (status === "UNVERIFIED") unverified += 1;
  console.log(`${status} ${name}${detail ? ` — ${detail}` : ""}`);
}

function cargoTest(name, args) {
  const run = spawnSync("cargo", args, { cwd: root, encoding: "utf8" });
  if (run.status === 0) {
    result("PASS", name);
    return;
  }
  const output = `${run.stdout ?? ""}\n${run.stderr ?? ""}`.trim().split("\n").slice(-12).join(" | ");
  result("FAIL", name, `exit ${run.status ?? "unknown"}; ${output}`);
}

function staticProbe() {
  try {
    assert.match(transport, /DIAGNOSTICS_MCP_PATH/);
    assert.match(transport, /diagnostics\.handle_json/);
    assert.match(tauriDiagnostics, /pub async fn call_tool/);
    assert.match(tauriLib, /diagnostics::DiagnosticsState/);
    result("PASS", "protocol route and Tauri handler primitives are present", "route-level evidence only");

    assert.match(tauriDiagnostics, /logs\.tail/);
    assert.match(tauriDiagnostics, /logs\.search/);
    assert.match(tauriDiagnostics, /handler_error/);
    result("PASS", "Tauri diagnostics tool dispatch has fixed names and typed failure path");

    if (!/stdin|stdout|serve_stdio/.test(tauriDiagnostics)) {
      result("UNVERIFIED", "Tauri stdio bridge", "no Tauri-owned stdin/stdout bridge is present");
    } else {
      result("PASS", "Tauri stdio bridge source is present");
    }

    assert.match(cliArgs, /DiagnosticsStdio/);
    assert.match(cliMain, /Cmd::DiagnosticsStdio/);
    assert.match(cliCommands, /pub fn diagnostics_stdio/);
    assert.match(cliCommands, /serve_stdio/);
    assert.match(cliCommands, /diagnostics_bridge_stdio/);
    assert.match(cliCommands, /DiagnosticsRequestAuth::new_signed/);
    assert.match(cliCommands, /Tls13HttpsTransport::new/);
    assert.match(cliCommands, /server_name: Option<String>/);
    assert.match(cliCommands, /alias = "serverName"/);
    assert.match(cliCommands, /MCP session is not initialized/);
    assert.match(cliCommands, /write_diagnostics_bridge_error/);
    result("PASS", "TokenMe CLI stdio host source is present", "--db-scoped SyncStore and JSON-RPC-only stdio path");

    assert.match(diagnosticsCore, /pub type DiagnosticsControlHandler/);
    assert.match(diagnosticsCore, /pub fn set_control_handler/);
    assert.match(diagnosticsCore, /handler\(spec\.name, arguments\)/);
    assert.match(tauriCommands, /dispatch_diagnostics_control/);
    assert.match(tauriCommands, /spawn_diagnostics_sync_action/);
    result("PASS", "remote control tools are attached to the bounded host callback", "fixed sync/ui allowlist with async Tauri routing");

    assert.match(tauriCommands, /fn sync_snapshot\(state: &mut SyncControlState, diagnostics_app: Option<AppHandle>\)/);
    assert.match(tauriCommands, /start_primary\(state, Some\(config\.bind_addr\.clone\(\)\), config\.advertise_host\.clone\(\), config\.allow_lan, diagnostics_app\.clone\(\)\)/);
    assert.match(tauriCommands, /sync_snapshot\(&mut state, Some\(app\.clone\(\)\)\)/);
    result("PASS", "primary restore keeps diagnostics control wiring", "launch restore and first status pass AppHandle into start_primary");

    assert.match(tauriDiagnostics, /pub fn diagnostics_session/);
    assert.match(tauriDiagnostics, /commands::primary_diagnostics_context\(&control\)/);
    assert.match(tauriDiagnostics, /context\.handle\.issue_diagnostics_grant/);
    assert.match(tauriCommands, /pub\(crate\) fn primary_diagnostics_context/);
    assert.match(tauriCommands, /pub handle: usage_device_sync::HttpsSyncServerHandle/);
    assert.match(tauriCommands, /HttpsSyncServer::bind_with_host/);
    assert.match(tauriCommands, /let diagnostics = server\.diagnostics_handle\(\)/);
    assert.match(transport, /pub struct HttpsSyncServerHandle/);
    assert.match(transport, /pub fn issue_diagnostics_grant/);
    assert.match(transport, /DiagnosticsService::new_with_host/);
    result("PASS", "Tauri diagnostics grant is attached to the running primary", "diagnostics_session → primary_diagnostics_context → HttpsSyncServerHandle → DiagnosticsService");

    assert.match(syncSheet, /tokenme:\/\/device-pair\?/);
    if (/runDiagnosticsPlaceholder/.test(syncSheet)) {
      result("FAIL", "diagnostics authorization QR/copy actions", "the UI still calls an undefined placeholder action");
    } else if (!/<img[^>]+diagnosticsQrDataUrl/.test(syncSheet)) {
      result("UNVERIFIED", "diagnostics authorization QR payload", "a config is generated but no rendered/downloadable QR evidence is present");
    } else {
      result("PASS", "diagnostics authorization QR payload is rendered by the UI");
    }

    result("UNVERIFIED", "Tauri → usage-device-sync end-to-end diagnostics call", "route and local handler exist, but no single host-to-transport acceptance path is wired");
    result("UNVERIFIED", "macOS/Ubuntu/Windows native diagnostics interop", "this probe does not turn loopback or cross-compilation into real-device evidence");
  } catch (error) {
    result("FAIL", "static diagnostics MCP route probe", error.message);
  }
}

console.log("Diagnostics MCP focused acceptance probe");
cargoTest("usage-device-sync diagnostics acceptance", ["test", "-p", "usage-device-sync", "--test", "diagnostics_mcp_acceptance", "--", "--test-threads=1"]);
cargoTest("usage-device-sync diagnostics transport primitives", ["test", "-p", "usage-device-sync", "--test", "diagnostics_mcp_primitives", "--", "--test-threads=1"]);
cargoTest("usage-device-sync diagnostics unit compile", ["test", "-p", "usage-device-sync", "diagnostics::", "--lib", "--", "--test-threads=1"]);
cargoTest("usage-cli diagnostics stdio host", ["test", "-p", "usage-cli", "--test", "cli", "diagnostics_stdio_is_a_jsonrpc_only_cli_host", "--", "--test-threads=1"]);
cargoTest("usage-cli diagnostics remote config rejection", ["test", "-p", "usage-cli", "--test", "cli", "diagnostics_stdio_rejects_invalid_remote_config_without_stdout", "--", "--test-threads=1"]);
cargoTest("usage-cli diagnostics config signing", ["test", "-p", "usage-cli", "diagnostics_config_deserializes_and_request_auth_is_signed", "--", "--test-threads=1"]);
const staticRun = spawnSync(process.execPath, [join(root, "scripts/test-diagnostics-mcp.mjs")], { cwd: root, encoding: "utf8" });
if (staticRun.status === 0) result("PASS", "static design and implementation audit probe");
else result("FAIL", "static design and implementation audit probe", `exit ${staticRun.status ?? "unknown"}`);
staticProbe();

console.log(`Diagnostics MCP acceptance summary: ${passed} passed, ${failed} failed, ${unverified} unverified`);
if (failed) process.exitCode = 1;

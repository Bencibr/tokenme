import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const read = (path) => readFileSync(join(root, path), "utf8");
const design = read("docs/internal/DIAGNOSTICS_MCP_DESIGN_V1.md");
const audit = read("docs/internal/DIAGNOSTICS_MCP_AUDIT_R1.md");
const transport = read("crates/usage-device-sync/src/transport.rs");
const quotaDiagnostics = read("crates/usage-quota/src/diagnostics.rs");
const commands = read("apps/tokenme-bar/src-tauri/src/commands.rs");
const bridge = read("apps/tokenme-bar/src/lib/bridge.ts");
const syncSheet = read("apps/tokenme-bar/src/components/SyncSheet.tsx");
const production = [
  read("crates/usage-device-sync/src/lib.rs"),
  read("crates/usage-device-sync/src/protocol.rs"),
  transport,
  read("crates/usage-device-sync/src/store.rs"),
  read("apps/tokenme-bar/src-tauri/src/commands.rs"),
  bridge,
  syncSheet,
].join("\n");

const checks = [
  ["design keeps the MCP implementation boundary explicit", () => {
    assert.match(design, /实现状态：`PARTIAL`/);
    assert.match(design, /未接通的工具必须返回 `tool_unavailable`/);
  }],
  ["design fixes the supported MCP methods and endpoint", () => {
    for (const method of ["initialize", "notifications/initialized", "tools/list", "tools/call"]) assert.ok(design.includes(`\`${method}\``), method);
    assert.match(design, /\/diagnostics\/mcp/);
  }],
  ["design fixes the read-only and control tool allowlists", () => {
    for (const tool of [
      "device.get_status", "device.get_build", "logs.tail", "logs.search", "sync.get_status",
      "sync.get_requests", "sync.get_trace", "diagnostics.run_check", "app.get_panel_state",
      "sync.start_primary", "sync.stop_primary", "sync.discover", "sync.approve_pairing",
      "sync.reject_pairing", "sync.clear_expired_request", "sync.pause", "sync.resume",
      "sync.revoke_device", "sync.refresh", "ui.open_sync_sheet", "ui.focus_device",
    ]) assert.ok(design.includes(`\`${tool}\``), tool);
    assert.match(design, /只能接受固定日志名，不能接受任意路径/);
  }],
  ["design separates pairing from diagnostics authorization", () => {
    assert.match(design, /授权默认关闭/);
    assert.match(design, /同步配对本身不授予诊断权限/);
    assert.match(design, /diagnostics\.read/);
    assert.match(design, /sync\.control/);
  }],
  ["design specifies expiry, revocation, and replay controls", () => {
    for (const term of ["expires_at_ms", "主动撤销", "replay cache", "nonce", "request_id"]) assert.match(design, new RegExp(term.replace(".", "\\.")));
  }],
  ["design specifies redaction and bounded output controls", () => {
    for (const term of ["Authorization", "API Key", "Prompt", "行数、字节数、时间范围和超时限制"]) assert.match(design, new RegExp(term.replace(" ", "\\s+")));
  }],
  ["design specifies stop lifecycle and UI semantic reuse", () => {
    assert.match(design, /停止主服务后立即失效/);
    assert.match(design, /所有 UI 意图调用与 Tauri UI 按钮复用同一个内部处理函数/);
  }],
  ["audit matrix covers every requested attack surface", () => {
    for (const term of ["未开启诊断", "重放", "中间人", "任意 Shell", "凭据泄露", "输出膨胀", "主端停止", "UI \/ MCP 分叉"]) assert.match(audit, new RegExp(term));
    assert.match(audit, /三端真实互通.*`UNVERIFIED`/s);
  }],
  ["audit preserves the not-yet-implemented evidence boundary", () => {
    assert.match(audit, /实现结论：`PARTIAL`/);
    assert.match(audit, /stdio bridge、Tauri grant\/session 签发到远端/);
    assert.match(audit, /远端 UI 语义操作的跨设备调用尚未接入/);
  }],
  ["existing TLS path verifies the leaf pin before HTTP bytes", () => {
    const connect = transport.slice(transport.indexOf("fn connect("), transport.indexOf("fn register_socket("));
    assert.match(connect, /complete_io[\s\S]*verify_peer_pin/);
    assert.match(transport, /struct PinOnlyVerifier/);
  }],
  ["existing transport has an explicit bounded response body", () => {
    assert.match(transport, /const MAX_RESPONSE_BYTES: usize = 256 \* 1024/);
    assert.match(transport, /if length > max_response \{ return Err\(TransportError::Limit\); \}/);
    assert.match(transport, /max_response_bytes == 0 \|\| self\.max_response_bytes > MAX_RESPONSE_BYTES/);
  }],
  ["existing stop path cancels active sockets", () => {
    assert.match(transport, /for stream in active\.values\(\) \{ let _ = stream\.shutdown\(Shutdown::Both\); \}/);
    assert.match(transport, /while !stop\.load\(AtomicOrdering::Acquire\)/);
    assert.match(commands, /stop_service\(state\)/);
  }],
  ["existing metadata diagnostics omit request and response contents", () => {
    const event = quotaDiagnostics.slice(quotaDiagnostics.indexOf("pub struct Event"), quotaDiagnostics.indexOf("pub type Sink"));
    assert.doesNotMatch(event, /\b(?:body|headers?|url|path|query|fragment)\b/);
    assert.match(quotaDiagnostics, /split\(\['\/', '\?', '#']\)\.next\(\)\?\.rsplit\('@'\)/);
  }],
  ["existing UI and Tauri sync action vocabulary stays aligned", () => {
    assert.match(bridge, /invoke<SyncDevicesSnapshot>\("sync_devices"/);
    assert.match(syncSheet, /bridge\.syncDevices/);
    for (const action of ["status", "discover", "start_primary", "start_secondary", "stop", "unpair", "approve", "reject", "pause", "resume", "revoke"]) {
      assert.match(syncSheet, new RegExp(`action: "${action}"`));
      assert.match(commands, new RegExp(`\\b${action.replace("_", "_")}\\b`));
    }
  }],
  ["production exposes only the bounded diagnostics MCP endpoint", () => {
    assert.match(production, /\/diagnostics\/mcp|DiagnosticsMcp|diagnostics_mcp/);
    assert.match(transport, /DIAGNOSTICS_MCP_PATH/);
    assert.match(transport, /MAX_DIAGNOSTICS_BODY_BYTES/);
  }],
];

let passed = 0;
let failed = 0;
for (const [name, check] of checks) {
  try {
    check();
    passed += 1;
    console.log(`PASS ${name}`);
  } catch (error) {
    failed += 1;
    console.error(`FAIL ${name}: ${error.message}`);
  }
}
console.log(`Diagnostics MCP audit probes: ${passed} passed, ${failed} failed`);
if (failed) process.exitCode = 1;

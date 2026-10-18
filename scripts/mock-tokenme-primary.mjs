#!/usr/bin/env node

// A local HTTPS primary used by the secondary-side closed-loop test.  It is
// intentionally small, but speaks the real invite, pairing, status, recovery,
// disconnect, and upload wire shapes used by usage-device-sync.

import { execFileSync, spawn } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { createServer } from "node:https";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { networkInterfaces, tmpdir } from "node:os";
import { join } from "node:path";
import { gunzipSync } from "node:zlib";

const PROTOCOL_ID = "tokenme-device-sync/1";
const RECEIPT_DOMAIN = "tokenme-device-sync/receipt/v1";
const MDNS_SERVICE_TYPE = "_tokenme-sync._tcp";
const publishMdns = process.argv.includes("--mdns");
const primaryId = `mock-primary-${randomBytes(6).toString("hex")}`;
const certDir = mkdtempSync(join(tmpdir(), "tokenme-mock-primary-"));
const keyPath = join(certDir, "key.pem");
const certPath = join(certDir, "cert.pem");

function fail(message) {
  throw new Error(message);
}

function makeCertificate() {
  if (!existsSync("/usr/bin/openssl")) fail("openssl is required to create the local mock certificate");
  execFileSync("/usr/bin/openssl", [
    "req", "-x509", "-newkey", "rsa:2048", "-nodes",
    "-keyout", keyPath, "-out", certPath, "-subj", "/CN=localhost",
    "-days", "1", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
  ], { stdio: "ignore" });
  const certificatePem = readFileSync(certPath);
  const certificateDer = certificatePemToDer(certificatePem);
  return { certificatePem, certificateDer, keyPem: readFileSync(keyPath) };
}

function certificatePemToDer(pem) {
  const body = pem.toString("ascii")
    .replace("-----BEGIN CERTIFICATE-----", "")
    .replace("-----END CERTIFICATE-----", "")
    .replace(/\s+/g, "");
  return Buffer.from(body, "base64");
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest();
}

function hex(bytes) {
  return Buffer.from(bytes).toString("hex");
}

function base64(bytes) {
  return Buffer.from(bytes).toString("base64");
}

function u64(value) {
  const result = Buffer.alloc(8);
  result.writeBigUInt64BE(BigInt(value));
  return result;
}

function i64(value) {
  const result = Buffer.alloc(8);
  result.writeBigInt64BE(BigInt(value));
  return result;
}

function identityBytes(domain, fields) {
  const parts = [domain, ...fields].map((field) => {
    const bytes = Buffer.isBuffer(field) ? field : Buffer.from(field);
    return Buffer.concat([u64(bytes.length), bytes]);
  });
  return Buffer.concat(parts);
}

function readU32(body, offset) {
  if (offset + 4 > body.length) fail("upload ended before a field length");
  return { value: body.readUInt32BE(offset), offset: offset + 4 };
}

function readField(body, offset, label) {
  const length = readU32(body, offset);
  const end = length.offset + length.value;
  if (end > body.length) fail(`upload ended inside ${label}`);
  return { value: body.subarray(length.offset, end), offset: end };
}

function readU64(body, offset, label) {
  if (offset + 8 > body.length) fail(`upload ended before ${label}`);
  return { value: Number(body.readBigUInt64BE(offset)), offset: offset + 8 };
}

function readUpload(body) {
  if (body.length < 9 || body.subarray(0, 8).toString("ascii") !== "TMSYNC01") fail("invalid upload magic");
  if (body[8] !== 1) fail("invalid upload wire version");
  let offset = 9;
  const protocol = readField(body, offset, "protocol_id"); offset = protocol.offset;
  const primary = readField(body, offset, "primary_id"); offset = primary.offset;
  const device = readField(body, offset, "device_id"); offset = device.offset;
  const origin = readField(body, offset, "origin"); offset = origin.offset;
  const stream = readField(body, offset, "stream_id"); offset = stream.offset;
  const manifest = readField(body, offset, "manifest"); offset = manifest.offset;
  const epoch = readU64(body, offset, "data_epoch"); offset = epoch.offset;
  const generation = readU64(body, offset, "generation"); offset = generation.offset;
  if (offset + 32 + 64 + 8 > body.length) fail("upload ended before hashes or signature");
  const gzipHash = body.subarray(offset, offset + 32); offset += 32;
  const signature = body.subarray(offset, offset + 64); offset += 64;
  const gzipLength = readU64(body, offset, "gzip length"); offset = gzipLength.offset;
  if (offset + gzipLength.value !== body.length) fail("upload gzip length does not match HTTP body");
  const gzip = body.subarray(offset);
  if (!sha256(gzip).equals(gzipHash)) fail("upload gzip hash does not match the envelope");
  const manifestHash = sha256(manifest.value);
  let manifestJson;
  try { manifestJson = JSON.parse(manifest.value.toString("utf8")); } catch { fail("upload manifest is not JSON"); }
  if (protocol.value.toString("utf8") !== PROTOCOL_ID) fail("unsupported upload protocol");
  let rows;
  try {
    const text = gunzipSync(gzip).toString("utf8");
    rows = text.split("\n").filter((line) => line.length > 0).map((line) => JSON.parse(line));
  } catch { fail("upload gzip is not JSONL"); }
  if (manifestJson.rows !== rows.length) fail("upload manifest row count does not match JSONL");
  return {
    primaryId: primary.value.toString("utf8"),
    deviceId: device.value.toString("utf8"),
    origin: origin.value.toString("utf8"),
    streamId: stream.value.toString("utf8"),
    dataEpoch: epoch.value,
    generation: generation.value,
    manifestHash,
    gzipHash: Buffer.from(gzipHash),
    signature: Buffer.from(signature),
    manifest: manifestJson,
    rows,
  };
}

function makeReceipt(upload, mergedAtMs) {
  const rows = upload.rows.length;
  const inserted = rows;
  const updated = 0;
  const deduped = 0;
  const receiptHash = sha256(identityBytes(RECEIPT_DOMAIN, [
    upload.primaryId,
    upload.deviceId,
    upload.streamId,
    u64(upload.dataEpoch),
    u64(upload.generation),
    upload.manifestHash,
    upload.gzipHash,
    u64(rows),
    u64(inserted),
    u64(updated),
    u64(deduped),
    i64(mergedAtMs),
  ]));
  return {
    primary_id: upload.primaryId,
    device_id: upload.deviceId,
    origin: upload.origin,
    stream_id: upload.streamId,
    data_epoch: upload.dataEpoch,
    generation: upload.generation,
    manifest_hash: [...upload.manifestHash],
    gzip_hash: [...upload.gzipHash],
    rows,
    inserted,
    updated,
    deduped,
    merged_at_ms: mergedAtMs,
    snapshot_complete: upload.manifest.snapshot_complete === true,
    receipt_hash: [...receiptHash],
  };
}

function readJson(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let bytes = 0;
    req.on("data", (chunk) => {
      bytes += chunk.length;
      if (bytes > 16 * 1024 * 1024) {
        reject(new Error("request body exceeds mock limit"));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => {
      try { resolve(JSON.parse(Buffer.concat(chunks).toString("utf8"))); }
      catch (error) { reject(error); }
    });
    req.on("error", reject);
  });
}

function readBytes(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let bytes = 0;
    req.on("data", (chunk) => {
      bytes += chunk.length;
      if (bytes > 128 * 1024 * 1024) {
        reject(new Error("upload body exceeds mock limit"));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

function respond(response, status, value) {
  const body = Buffer.from(value == null ? "" : JSON.stringify(value));
  response.writeHead(status, {
    "content-type": "application/json",
    "content-length": body.length,
    connection: "close",
  });
  response.end(body);
}

const { certificateDer, keyPem } = makeCertificate();
const certificatePin = sha256(certificateDer);
const state = {
  inviteId: `inv-${randomBytes(8).toString("hex")}`,
  inviteSecret: randomBytes(32),
  endpoint: null,
  certificateDer,
  certificatePin,
  deviceId: null,
  origin: null,
  displayName: null,
  os: null,
  approved: false,
  disconnected: false,
  statusRequests: 0,
  uploads: 0,
  lastAck: null,
};

let mdnsProcess = null;

function localAdvertiseHost() {
  for (const addresses of Object.values(networkInterfaces())) {
    for (const address of addresses ?? []) {
      if (address.family === "IPv4" && !address.internal && !address.address.startsWith("169.254.")) return address.address;
    }
  }
  fail("no non-loopback IPv4 address is available for the local mDNS test");
}

function startMdns(port) {
  if (!existsSync("/usr/bin/dns-sd")) fail("/usr/bin/dns-sd is required for the real mDNS test");
  const serviceName = `TokenMe-${primaryId.slice(-8)}`;
  mdnsProcess = spawn("/usr/bin/dns-sd", [
    "-R", serviceName, MDNS_SERVICE_TYPE, "local", String(port),
    "v=1", `proto=${PROTOCOL_ID}`, "role=primary", `id=${primaryId}`,
    "name=TokenMe local test primary", `pin=sha256:${hex(state.certificatePin)}`, "caps=pair,upload",
  ], { stdio: ["ignore", "pipe", "pipe"] });
  mdnsProcess.stdout.resume();
  mdnsProcess.stderr.resume();
  mdnsProcess.once("error", (error) => console.error(`MOCK_TOKENME_PRIMARY_MDNS_ERROR ${error.message}`));
  mdnsProcess.once("exit", (code, signal) => {
    if (mdnsProcess) console.log(`MOCK_TOKENME_PRIMARY_MDNS_STOPPED ${JSON.stringify({ code, signal })}`);
    mdnsProcess = null;
  });
}

function stopMdns() {
  if (!mdnsProcess) return;
  mdnsProcess.kill("SIGTERM");
  mdnsProcess = null;
}

function inviteUrl() {
  return [
    `tokenme://device-pair?invite=${state.inviteId}`,
    `endpoint=${state.endpoint}`,
    `pin=${hex(state.certificatePin)}`,
    `secret=${hex(state.inviteSecret)}`,
    `expires=${Date.now() + 15 * 60 * 1000}`,
    `primary=${primaryId}`,
    "name=localhost",
    `cert=${hex(state.certificateDer)}`,
  ].join("&");
}

function statusFor(request) {
  const lastAck = state.lastAck;
  return {
    primary_id: primaryId,
    device_id: request.device_id,
    origin: state.origin ?? request.origin,
    request_nonce: request.nonce,
    status: state.disconnected ? "revoked" : state.approved ? "paired" : "pending",
    data_epoch: 1,
    active_stream_id: state.disconnected ? null : "mock-stream",
    expected_generation: state.disconnected ? null : (lastAck ? lastAck.generation + 1 : 1),
    last_generation: lastAck?.generation ?? null,
    highwater_generation: lastAck?.generation ?? null,
    highwater_receipt_hash: lastAck?.receipt_hash ?? null,
    checkpoint_generation: request.checkpoint_generation ?? null,
    checkpoint_receipt_hash: lastAck?.receipt_hash ?? null,
    coverage_lo_ms: null,
    coverage_hi_ms: null,
    observed_at_ms: Date.now(),
  };
}

async function handle(req, response) {
  try {
    const path = new URL(req.url ?? "/", "https://localhost").pathname;
    const body = path === "/v1/upload" ? await readBytes(req) : await readJson(req);
    if (path === "/v1/pairing/invite") {
      respond(response, 200, {
        invite_id: state.inviteId,
        endpoint: state.endpoint,
        certificate_pin: hex(state.certificatePin),
        expires_at_ms: Date.now() + 15 * 60 * 1000,
        secret_b64: base64(state.inviteSecret),
        certificate_der_b64: base64(state.certificateDer),
      });
      return;
    }
    if (path === "/v1/pairing/request") {
      state.deviceId = body.device_id;
      state.origin = body.origin;
      state.displayName = body.display_name;
      state.os = body.os;
      state.approved = true;
      console.log(`MOCK_TOKENME_PRIMARY_PAIRING ${JSON.stringify({ device_id: state.deviceId, origin: state.origin, auto_approved: true })}`);
      respond(response, 200, {
        request_id: body.request_id,
        invite_id: body.invite_id,
        device_id: body.device_id,
        origin: body.origin,
        display_name: body.display_name,
        os: body.os,
        status: "pending",
      });
      return;
    }
    if (path === "/v1/status") {
      state.statusRequests += 1;
      const result = statusFor(body);
      console.log(`MOCK_TOKENME_PRIMARY_STATUS ${JSON.stringify({ device_id: body.device_id, status: result.status, stream_id: result.active_stream_id, expected_generation: result.expected_generation, checkpoint_generation: result.checkpoint_generation })}`);
      respond(response, result.status === "revoked" ? 403 : 200, result);
      return;
    }
    if (path === "/v1/recovery") {
      respond(response, 200, {
        primary_id: primaryId,
        device_id: body.device_id,
        stream_id: "mock-stream",
        data_epoch: 1,
        expected_generation: state.lastAck ? state.lastAck.generation + 1 : 1,
      });
      return;
    }
    if (path === "/v1/upload") {
      const upload = readUpload(body);
      if (upload.primaryId !== primaryId || upload.deviceId !== state.deviceId || upload.streamId !== "mock-stream" || upload.dataEpoch !== 1) {
        fail("upload identity does not match the mock primary state");
      }
      const receipt = makeReceipt(upload, Date.now());
      state.lastAck = { generation: receipt.generation, receipt_hash: receipt.receipt_hash };
      state.uploads += 1;
      const first = upload.rows[0];
      console.log(`MOCK_TOKENME_PRIMARY_UPLOAD ${JSON.stringify({ device_id: upload.deviceId, generation: upload.generation, rows: receipt.rows, inserted: receipt.inserted, snapshot_complete: receipt.snapshot_complete, gzip_bytes: body.length, first_row: first ? { key: first.key, tool: first.tool, source: first.source, in: first.counts?.in, out: first.counts?.out } : null })}`);
      respond(response, 200, receipt);
      return;
    }
    if (path === "/v1/disconnect") {
      state.disconnected = true;
      console.log(`MOCK_TOKENME_PRIMARY_DISCONNECT ${JSON.stringify({ device_id: body.device_id })}`);
      respond(response, 200, {});
      return;
    }
    respond(response, 404, { message: "unknown mock primary endpoint" });
  } catch (error) {
    console.error(`MOCK_TOKENME_PRIMARY_ERROR ${error instanceof Error ? error.message : String(error)}`);
    if (!response.headersSent) respond(response, 400, { message: "mock primary rejected request" });
    else response.destroy();
  }
}

const server = createServer({ key: keyPem, cert: readFileSync(certPath) });
server.on("request", (req, response) => { void handle(req, response); });
server.on("checkContinue", (req, response) => {
  response.writeContinue();
  void handle(req, response);
});

let listeningPort = null;
const keepAlive = setInterval(() => {}, 60_000);

function listenPrimary(eventName) {
  server.listen(listeningPort ?? 0, publishMdns ? "0.0.0.0" : "127.0.0.1", () => {
    const address = server.address();
    if (!address || typeof address === "string") fail("mock primary did not expose a TCP address");
    listeningPort = address.port;
    state.endpoint = `https://${publishMdns ? localAdvertiseHost() : "127.0.0.1"}:${listeningPort}`;
    if (publishMdns) startMdns(listeningPort);
    if (eventName === "MOCK_TOKENME_PRIMARY_READY") {
      console.log(`${eventName} ${JSON.stringify({ endpoint: state.endpoint, primary_id: primaryId, certificate_pin: hex(state.certificatePin), invite: inviteUrl(), mdns: publishMdns, pid: process.pid })}`);
    } else {
      console.log(`${eventName} ${JSON.stringify({ endpoint: state.endpoint, primary_id: primaryId, pid: process.pid })}`);
    }
  });
}

function pausePrimary() {
  if (!server.listening) return;
  stopMdns();
  server.close(() => console.log(`MOCK_TOKENME_PRIMARY_DOWN ${JSON.stringify({ endpoint: state.endpoint, pid: process.pid })}`));
}

function resumePrimary() {
  if (!server.listening) listenPrimary("MOCK_TOKENME_PRIMARY_RECOVERED");
}

// The test uses these signals to model a primary process becoming unavailable
// and then returning with the same identity, certificate, and paired state.
// They are deliberately different from SIGTERM, which ends the mock process.
process.on("SIGUSR1", pausePrimary);
process.on("SIGUSR2", resumePrimary);

function cleanup() {
  clearInterval(keepAlive);
  stopMdns();
  try { rmSync(certDir, { recursive: true, force: true }); } catch { /* best effort for a test temp dir */ }
}

function shutdown(signal) {
  const finish = () => { cleanup(); process.exit(signal === "SIGINT" ? 130 : 143); };
  if (!server.listening) { finish(); return; }
  server.close(finish);
  setTimeout(() => { cleanup(); process.exit(0); }, 1000).unref();
}

process.once("SIGINT", () => shutdown("SIGINT"));
process.once("SIGTERM", () => shutdown("SIGTERM"));
process.once("exit", cleanup);

listenPrimary("MOCK_TOKENME_PRIMARY_READY");

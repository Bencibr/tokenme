#!/usr/bin/env node

import { createHash, generateKeyPairSync, randomBytes, sign } from "node:crypto";
import { gzipSync } from "node:zlib";
import { request } from "node:https";

function argument(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : null;
}

const endpoint = argument("--endpoint");
const primaryId = argument("--primary-id");
const stayConnected = process.argv.includes("--stay-connected");
if (!endpoint || !primaryId) throw new Error("usage: mock-tokenme-peer.mjs --endpoint <https-url> --primary-id <id> [--stay-connected]");

const identity = generateKeyPairSync("ed25519");
const publicKeyDer = identity.publicKey.export({ type: "spki", format: "der" });
const publicKey = publicKeyDer.subarray(publicKeyDer.length - 32);
const deviceId = `dev-${createHash("sha256").update(publicKey).digest("hex")}`;
const origin = `mock-peer-${deviceId.slice(-8)}`;
const displayName = "Local Mock Peer";
const os = "test";
const domainPairing = Buffer.from("tokenme-device-sync/pairing/v1");
const domainStatus = Buffer.from("tokenme-device-sync/status/v1");
const domainUpload = Buffer.from("tokenme-device-sync/upload-signature/v1");

function encodePart(value) {
  const bytes = Buffer.isBuffer(value) ? value : Buffer.from(value);
  const length = Buffer.alloc(8);
  length.writeBigUInt64BE(BigInt(bytes.length));
  return Buffer.concat([length, bytes]);
}

function identityBytes(domain, fields) {
  return Buffer.concat([encodePart(domain), ...fields.map(encodePart)]);
}

function hex(value) {
  return Buffer.from(value).toString("hex");
}

function base64(value) {
  return Buffer.from(value).toString("base64");
}

function postJson(path, value, expectedStatuses = [200]) {
  const url = new URL(endpoint);
  return new Promise((resolve, reject) => {
    const body = Buffer.from(JSON.stringify(value));
    const client = request({
      protocol: url.protocol,
      hostname: url.hostname,
      port: url.port,
      path: `${url.pathname.replace(/\/$/, "")}${path}`,
      method: "POST",
      rejectUnauthorized: false,
      headers: { "content-type": "application/json", "content-length": body.length },
    }, (response) => {
      const chunks = [];
      response.on("data", (chunk) => chunks.push(chunk));
      response.on("end", () => {
        const raw = Buffer.concat(chunks).toString("utf8");
        let parsed = {};
        try { parsed = raw ? JSON.parse(raw) : {}; } catch { parsed = { raw }; }
        const status = response.statusCode ?? 0;
        if (!expectedStatuses.includes(status)) {
          const message = typeof parsed.message === "string" ? parsed.message : `HTTP ${status}`;
          resolve({ status, value: parsed, error: message });
          return;
        }
        resolve({ status, value: parsed });
      });
    });
    client.once("error", reject);
    client.write(body);
    client.end();
  });
}

function postBytes(path, body, expectedStatuses = [200]) {
  const url = new URL(endpoint);
  return new Promise((resolve, reject) => {
    const client = request({
      protocol: url.protocol,
      hostname: url.hostname,
      port: url.port,
      path: `${url.pathname.replace(/\/$/, "")}${path}`,
      method: "POST",
      rejectUnauthorized: false,
      headers: { "content-type": "application/octet-stream", "content-length": body.length },
    }, (response) => {
      const chunks = [];
      response.on("data", (chunk) => chunks.push(chunk));
      response.on("end", () => {
        const raw = Buffer.concat(chunks).toString("utf8");
        let parsed = {};
        try { parsed = raw ? JSON.parse(raw) : {}; } catch { parsed = { raw }; }
        const status = response.statusCode ?? 0;
        if (!expectedStatuses.includes(status)) {
          const message = typeof parsed.message === "string" ? parsed.message : `HTTP ${status}`;
          resolve({ status, value: parsed, error: message });
          return;
        }
        resolve({ status, value: parsed });
      });
    });
    client.once("error", reject);
    client.write(body);
    client.end();
  });
}

function makePairingRequest(invite) {
  const requestId = `req-${cryptoRandomHex(16)}`;
  const fields = [
    requestId,
    invite.invite_id,
    Buffer.from(invite.secret_b64, "base64"),
    deviceId,
    origin,
    displayName,
    os,
    publicKey,
  ];
  const signature = sign(null, identityBytes(domainPairing, fields), identity.privateKey);
  return {
    request_id: requestId,
    invite_id: invite.invite_id,
    invite_secret_b64: invite.secret_b64,
    device_id: deviceId,
    origin,
    display_name: displayName,
    os,
    public_key_b64: base64(publicKey),
    signature_b64: base64(signature),
  };
}

function makeStatusRequest(wantRecovery = false) {
  const nonce = Buffer.from(cryptoRandomHex(16), "hex");
  const issuedAt = Date.now();
  const issuedAtBytes = Buffer.alloc(8);
  issuedAtBytes.writeBigInt64BE(BigInt(issuedAt));
  const checkpoint = Buffer.alloc(9);
  const fields = [
    "tokenme-device-sync/1",
    primaryId,
    deviceId,
    origin,
    nonce,
    issuedAtBytes,
    wantRecovery ? "1" : "0",
    checkpoint,
  ];
  const signature = sign(null, identityBytes(domainStatus, fields), identity.privateKey);
  return {
    protocol_id: "tokenme-device-sync/1",
    primary_id: primaryId,
    device_id: deviceId,
    origin,
    nonce: [...nonce],
    issued_at_ms: issuedAt,
    want_recovery: wantRecovery,
    checkpoint_generation: null,
    signature: [...signature],
  };
}

function cryptoRandomHex(bytes) {
  return randomBytes(bytes).toString("hex");
}

function u32(value) {
  const result = Buffer.alloc(4);
  result.writeUInt32BE(value);
  return result;
}

function u64(value) {
  const result = Buffer.alloc(8);
  result.writeBigUInt64BE(BigInt(value));
  return result;
}

function wireField(value) {
  const bytes = Buffer.isBuffer(value) ? value : Buffer.from(value);
  return Buffer.concat([u32(bytes.length), bytes]);
}

function makeUpload(connection) {
  const now = Date.now();
  const streamId = connection.response.active_stream_id ?? connection.response.stream_id;
  const dataEpoch = Number(connection.response.data_epoch);
  const generation = Number(connection.response.expected_generation);
  if (!streamId || !Number.isInteger(dataEpoch) || !Number.isInteger(generation)) {
    throw new Error("mock peer received an incomplete upload stream status");
  }
  const row = {
    key: `mock-peer-${deviceId}-row-1`,
    occurrence_id: null,
    tool: "codex",
    ts_ms: now,
    session: "mock-peer-live-session",
    project: "/tmp/tokenme-live",
    model: "gpt-5",
    meter: "tokens",
    source: "mock-peer.jsonl",
    counts: { in: 11, cc_tok: 0, cr_tok: 0, out: 7, reason_tok: 0, credits: 0 },
    calls: [],
  };
  const manifest = {
    format: "tokenme-sync/1",
    schema_version: "4",
    origin,
    window_lo_ms: now - 60_000,
    window_hi_ms: now + 60_000,
    rows: 1,
    sums: { in: 11, cc_tok: 0, cr_tok: 0, out: 7, reason_tok: 0, credits: 0 },
    generated_at_ms: now,
    snapshot_complete: true,
  };
  const manifestBytes = Buffer.from(JSON.stringify(manifest));
  const gzip = gzipSync(Buffer.from(`${JSON.stringify(row)}\n`));
  const gzipHash = createHash("sha256").update(gzip).digest();
  const wireFields = ["tokenme-device-sync/1", primaryId, deviceId, origin, streamId, manifestBytes];
  const signatureFields = [
    "tokenme-device-sync/1", primaryId, deviceId, origin, streamId,
    u64(dataEpoch), u64(generation), manifestBytes, gzipHash,
  ];
  const signature = sign(null, identityBytes(domainUpload, signatureFields), identity.privateKey);
  const wire = Buffer.concat([
    Buffer.from("TMSYNC01"), Buffer.from([1]),
    ...wireFields.map(wireField), u64(dataEpoch), u64(generation),
    gzipHash, signature, u64(gzip.length), gzip,
  ]);
  return { wire, generation };
}

function wait(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

const inviteResponse = await postJson("/v1/pairing/invite", { primary_id: primaryId, endpoint });
if (inviteResponse.status !== 200) throw new Error(`pairing invite failed: ${inviteResponse.error ?? inviteResponse.status}`);
const invite = inviteResponse.value;
for (const field of ["invite_id", "secret_b64", "certificate_pin", "certificate_der_b64"]) {
  if (typeof invite[field] !== "string" || invite[field].length === 0) throw new Error(`pairing invite missing ${field}`);
}

const pairing = makePairingRequest(invite);
const pairingResponse = await postJson("/v1/pairing/request", pairing);
if (pairingResponse.status !== 200 || pairingResponse.value.status !== "pending") {
  throw new Error(`pairing request failed: ${pairingResponse.error ?? pairingResponse.status}`);
}
console.log(`MOCK_TOKENME_PEER_PAIRING_REQUEST ${JSON.stringify({ request_id: pairing.request_id, device_id: deviceId, origin })}`);

let connected;
for (let attempt = 0; attempt < 80; attempt += 1) {
  const requestBody = makeStatusRequest();
  const response = await postJson("/v1/status", requestBody, [200, 401, 403]);
  if (response.status === 200 && response.value.status === "paired") {
    connected = { request: requestBody, response: response.value };
    break;
  }
  await wait(250);
}
if (!connected) throw new Error("mock peer did not observe MCP-approved pairing within 20 seconds");
if (connected.response.request_nonce.join(",") !== connected.request.nonce.join(",")) throw new Error("primary returned a mismatched status nonce");
console.log(`MOCK_TOKENME_PEER_STATUS ${JSON.stringify({ device_id: deviceId, status: connected.response.status, stream_id: connected.response.active_stream_id })}`);

let uploadConnection = connected;
if (!connected.response.active_stream_id) {
  const recoveryRequest = makeStatusRequest(true);
  const recovery = await postJson("/v1/recovery", recoveryRequest);
  if (recovery.status !== 200 || !recovery.value.stream_id || !recovery.value.expected_generation) {
    throw new Error(`stream recovery failed: ${recovery.error ?? JSON.stringify(recovery.value)}`);
  }
  uploadConnection = {
    request: recoveryRequest,
    response: { ...connected.response, active_stream_id: recovery.value.stream_id, data_epoch: recovery.value.data_epoch, expected_generation: recovery.value.expected_generation },
  };
  console.log(`MOCK_TOKENME_PEER_RECOVERY ${JSON.stringify({ device_id: deviceId, stream_id: recovery.value.stream_id, generation: recovery.value.expected_generation })}`);
}

const upload = makeUpload(uploadConnection);
const uploadResponse = await postBytes("/v1/upload", upload.wire);
if (uploadResponse.status !== 200 || uploadResponse.value.rows !== 1 || uploadResponse.value.inserted !== 1 || uploadResponse.value.snapshot_complete !== true) {
  throw new Error(`remote result upload was not merged: ${uploadResponse.error ?? JSON.stringify(uploadResponse.value)}`);
}
console.log(`MOCK_TOKENME_PEER_UPLOAD ${JSON.stringify({ device_id: deviceId, generation: upload.generation, rows: uploadResponse.value.rows, inserted: uploadResponse.value.inserted, snapshot_complete: uploadResponse.value.snapshot_complete })}`);

if (stayConnected) {
  console.log(`MOCK_TOKENME_PEER_STAY_CONNECTED ${JSON.stringify({ device_id: deviceId })}`);
  for (;;) {
    await wait(1000);
    const requestBody = makeStatusRequest();
    try {
      const response = await postJson("/v1/status", requestBody, [200, 401, 403]);
      console.log(`MOCK_TOKENME_PEER_RECONNECT_STATUS ${JSON.stringify({ device_id: deviceId, http_status: response.status, status: response.value.status ?? null })}`);
    } catch (error) {
      console.log(`MOCK_TOKENME_PEER_OFFLINE ${JSON.stringify({ device_id: deviceId, error: error instanceof Error ? error.message : String(error) })}`);
    }
  }
}

const disconnect = await postJson("/v1/disconnect", uploadConnection.request);
if (disconnect.status !== 200) throw new Error(`disconnect failed: ${disconnect.error ?? disconnect.status}`);
console.log(`MOCK_TOKENME_PEER_DISCONNECTED ${JSON.stringify({ device_id: deviceId })}`);

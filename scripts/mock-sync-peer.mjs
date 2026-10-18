#!/usr/bin/env node

import { createServer } from "node:http";

const portArg = process.argv.indexOf("--port");
const requestedPort = portArg >= 0 ? Number(process.argv[portArg + 1]) : 0;
const state = {
  connected: false,
  device_id: null,
  pairing_requests: [],
  disconnect_count: 0,
};

function reply(response, status, value) {
  const body = JSON.stringify(value);
  response.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(body) });
  response.end(body);
}

async function readJson(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  const body = Buffer.concat(chunks).toString("utf8");
  return body ? JSON.parse(body) : {};
}

function requireFields(value, fields) {
  for (const field of fields) {
    if (typeof value[field] !== "string" || value[field].length === 0) throw new Error(`missing ${field}`);
  }
}

function validateAuthenticatedRequest(value) {
  requireFields(value, ["protocol_id", "primary_id", "device_id", "origin"]);
  if (!Array.isArray(value.nonce) || value.nonce.length !== 16) throw new Error("nonce must contain 16 bytes");
  if (!Array.isArray(value.signature) || value.signature.length !== 64) throw new Error("signature must contain 64 bytes");
  if (value.primary_id !== "primary-mock") throw new Error("unexpected primary id");
}

const server = createServer(async (request, response) => {
  try {
    if (request.method !== "POST") {
      reply(response, 405, { message: "mock peer accepts POST only" });
      return;
    }
    const value = await readJson(request);
    if (request.url === "/v1/pairing/request") {
      requireFields(value, ["request_id", "invite_id", "invite_secret_b64", "device_id", "origin", "display_name", "os", "public_key_b64", "signature_b64"]);
      state.connected = true;
      state.device_id = value.device_id;
      state.pairing_requests.push({ request_id: value.request_id, invite_id: value.invite_id, device_id: value.device_id, status: "pending" });
      reply(response, 200, { request_id: value.request_id, invite_id: value.invite_id, device_id: value.device_id, origin: value.origin, display_name: value.display_name, os: value.os, status: "pending" });
      return;
    }
    if (request.url === "/v1/status") {
      validateAuthenticatedRequest(value);
      state.device_id = value.device_id;
      reply(response, 200, {
        primary_id: value.primary_id,
        device_id: value.device_id,
        origin: value.origin,
        request_nonce: value.nonce,
        status: state.connected ? "paired" : "disconnected",
        data_epoch: 1,
        active_stream_id: state.connected ? "stream-mock" : null,
        expected_generation: state.connected ? 1 : null,
        last_generation: state.connected ? 0 : null,
        highwater_generation: state.connected ? 0 : null,
        highwater_receipt_hash: null,
        checkpoint_generation: value.checkpoint_generation ?? null,
        checkpoint_receipt_hash: null,
        coverage_lo_ms: null,
        coverage_hi_ms: null,
        observed_at_ms: Date.now(),
      });
      return;
    }
    if (request.url === "/v1/disconnect") {
      validateAuthenticatedRequest(value);
      state.connected = false;
      state.disconnect_count += 1;
      reply(response, 200, {});
      return;
    }
    reply(response, 404, { message: "mock peer route not found" });
  } catch (error) {
    reply(response, 400, { message: error instanceof Error ? error.message : String(error) });
  }
});

server.listen(requestedPort, "127.0.0.1", () => {
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("mock peer did not receive a TCP address");
  console.log(`MOCK_SYNC_PEER_READY ${JSON.stringify({ host: "127.0.0.1", port: address.port })}`);
});

function close() {
  server.close(() => process.exit(0));
}

process.once("SIGTERM", close);
process.once("SIGINT", close);

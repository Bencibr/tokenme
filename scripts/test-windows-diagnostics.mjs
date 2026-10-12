import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";
import { prepare, stagedDirectory } from "./prepare-windows-diagnostics.mjs";
import { archive, symbolIdentity } from "./archive-windows-build.mjs";

const root = mkdtempSync(join(tmpdir(), "tokenme-diagnostics-test-"));
function put(path, bytes) { mkdirSync(dirname(join(root, path)), { recursive: true }); writeFileSync(join(root, path), bytes); }
function git(args) { const result = spawnSync("git", args, { cwd: root, encoding: "utf8", windowsHide: true }); assert.equal(result.status, 0, result.stderr); }

function pair(info, marker = 1) {
  const exe = Buffer.alloc(1024), pdb = Buffer.alloc(2048), guid = Buffer.alloc(16, marker);
  exe.write("MZ"); exe.writeUInt32LE(0x80, 0x3c); exe.write("PE\0\0", 0x80);
  exe.writeUInt16LE(240, 0x80 + 20); exe.writeUInt16LE(0x20b, 0x98); exe.writeUInt32LE(1024, 0x98 + 60);
  exe.writeUInt32LE(0x200, 0x98 + 112 + 48); exe.writeUInt32LE(28, 0x98 + 112 + 52);
  exe.writeUInt32LE(2, 0x200 + 12); exe.writeUInt32LE(0x300, 0x200 + 24);
  exe.write("RSDS", 0x300); guid.copy(exe, 0x304); exe.writeUInt32LE(1, 0x314); exe.write(info.source_sha256, 0x380);
  pdb.write("Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0", 0, "ascii");
  pdb.writeUInt32LE(512, 32); pdb.writeUInt32LE(16, 44); pdb.writeUInt32LE(1, 52);
  pdb.writeUInt32LE(2, 512); pdb.writeUInt32LE(2, 1024); pdb.writeUInt32LE(0, 1028); pdb.writeUInt32LE(28, 1032); pdb.writeUInt32LE(3, 1036);
  pdb.writeUInt32LE(1, 1536 + 8); guid.copy(pdb, 1536 + 12);
  return { exe, pdb };
}

try {
  put("Cargo.toml", "[workspace]\n"); put("Cargo.lock", "# fixture\n");
  put(".gitignore", "apps/tokenme-bar/src-tauri/resources/diagnostics/\n");
  put("apps/tokenme-bar/src-tauri/tauri.conf.json", JSON.stringify({ version: "0.1.7" }));
  put("apps/tokenme-bar/src-tauri/BUILD_ID", "2\n"); put("crates/example/src/lib.rs", "pub fn value() -> u8 { 1 }\n");
  git(["init", "-q"]); git(["add", "Cargo.toml", "Cargo.lock", ".gitignore", "apps", "crates"]);
  git(["-c", "user.name=Agent", "-c", "user.email=agent@local", "commit", "-qm", "test: initialize diagnostic fixture"]);
  const first = prepare(root);
  put(".workbuddy-ai/local-note.txt", "fixture-secret-do-not-archive");
  assert.equal(prepare(root).source_sha256, first.source_sha256);
  put("crates/example/src/new.rs", "pub fn new_code() {}\n");
  const updated = prepare(root); assert.notEqual(updated.source_sha256, first.source_sha256); assert.equal(updated.dirty, true);
  const binary = pair(updated); put("target/TokenMe.exe", binary.exe); put("target/tokenme.pdb", binary.pdb);
  const destination = archive(join(root, "target/TokenMe.exe"), root);
  assert.deepEqual(readFileSync(join(destination, "TokenMe.exe")), binary.exe);
  assert.deepEqual(readFileSync(join(destination, "tokenme.pdb")), binary.pdb);
  assert.equal(archive(join(root, "target/TokenMe.exe"), root), destination);
  assert.equal(existsSync(join(destination, "sources/crates/example/src/new.rs")), true);
  assert.equal(JSON.stringify(JSON.parse(readFileSync(join(stagedDirectory(root), "extra-sources.json")))).includes("fixture-secret"), false);
  const mismatched = pair(updated, 2);
  assert.throws(() => symbolIdentity(binary.exe, mismatched.pdb), /identities do not match/);
  put("crates/example/src/lib.rs", "pub fn value() -> u8 { 2 }\n");
  assert.throws(() => archive(join(root, "target/TokenMe.exe"), root), /changed during the build/);
  prepare(root);
  assert.throws(() => archive(join(root, "target/TokenMe.exe"), root), /source fingerprint/);
  console.log("Windows diagnostics checks passed: source identity, private-state exclusion, exact archives, PDB matching and stale-build rejection.");
} finally {
  assert.match(relative(tmpdir(), root), /^tokenme-diagnostics-test-[^\\/]+$/);
  rmSync(root, { recursive: true, force: true });
}

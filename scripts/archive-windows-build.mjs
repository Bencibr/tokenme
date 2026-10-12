import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { currentSourceHash, repoRoot, stagedDirectory } from "./prepare-windows-diagnostics.mjs";

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

// PE CodeView and MSF7 PDB info both carry the linker's GUID and age. File
// names/mtime alone cannot prove that a PDB belongs to this executable.
export function symbolIdentity(exe, pdb) {
  if (exe.toString("ascii", 0, 2) !== "MZ") throw new Error("Not a PE executable.");
  const pe = exe.readUInt32LE(0x3c), optional = pe + 24;
  if (exe.toString("ascii", pe, pe + 4) !== "PE\0\0") throw new Error("Invalid PE signature.");
  const directories = optional + (exe.readUInt16LE(optional) === 0x20b ? 112 : 96);
  const debugRva = exe.readUInt32LE(directories + 6 * 8), debugSize = exe.readUInt32LE(directories + 6 * 8 + 4);
  const sections = optional + exe.readUInt16LE(pe + 20);
  function fileOffset(rva) {
    if (rva < exe.readUInt32LE(optional + 60)) return rva;
    for (let i = 0; i < exe.readUInt16LE(pe + 6); i++) {
      const section = sections + i * 40, start = exe.readUInt32LE(section + 12);
      const size = Math.max(exe.readUInt32LE(section + 8), exe.readUInt32LE(section + 16));
      if (rva >= start && rva - start < size) return exe.readUInt32LE(section + 20) + rva - start;
    }
    throw new Error("PE debug directory is unmapped.");
  }
  let expected;
  const debug = fileOffset(debugRva);
  for (let i = 0; i + 28 <= debugSize; i += 28) {
    if (exe.readUInt32LE(debug + i + 12) !== 2) continue;
    const record = exe.readUInt32LE(debug + i + 24);
    if (exe.toString("ascii", record, record + 4) === "RSDS") {
      expected = { guid: exe.subarray(record + 4, record + 20).toString("hex"), age: exe.readUInt32LE(record + 20) }; break;
    }
  }
  if (!expected) throw new Error("PE CodeView identity is missing.");
  if (!pdb.subarray(0, 32).equals(Buffer.from("Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0", "ascii"))) throw new Error("Unsupported PDB format.");
  const blockSize = pdb.readUInt32LE(32), directorySize = pdb.readUInt32LE(44), blockMap = pdb.readUInt32LE(52) * blockSize;
  if (blockSize < 512 || blockSize > 65536 || directorySize > pdb.length) throw new Error("Invalid PDB directory.");
  const pieces = [];
  for (let i = 0; i < Math.ceil(directorySize / blockSize); i++) {
    const block = pdb.readUInt32LE(blockMap + i * 4) * blockSize;
    if (block + blockSize > pdb.length) throw new Error("Invalid PDB directory block.");
    pieces.push(pdb.subarray(block, block + blockSize));
  }
  const directory = Buffer.concat(pieces).subarray(0, directorySize), count = directory.readUInt32LE(0);
  if (count < 2 || count * 4 + 4 > directory.length) throw new Error("PDB info stream is missing.");
  let cursor = 4 + count * 4, info;
  for (let stream = 0; stream <= 1; stream++) {
    const size = directory.readUInt32LE(4 + stream * 4), blocks = size === 0xffffffff ? 0 : Math.ceil(size / blockSize);
    const data = [];
    for (let i = 0; i < blocks; i++) {
      const offset = directory.readUInt32LE(cursor) * blockSize; cursor += 4;
      if (offset + blockSize > pdb.length) throw new Error("Invalid PDB stream block.");
      if (stream === 1) data.push(pdb.subarray(offset, offset + blockSize));
    }
    if (stream === 1) info = Buffer.concat(data).subarray(0, size);
  }
  const actual = { guid: info.subarray(12, 28).toString("hex"), age: info.readUInt32LE(8) };
  if (actual.guid !== expected.guid || actual.age !== expected.age) throw new Error("EXE and PDB identities do not match.");
  return expected;
}

export function archive(exe, root = repoRoot) {
  exe = resolve(exe);
  const pdb = join(dirname(exe), "tokenme.pdb");
  if (!existsSync(pdb)) throw new Error("Matching tokenme.pdb is missing; cannot archive this build.");
  const staged = stagedDirectory(root);
  const info = JSON.parse(readFileSync(join(staged, "build-info.json"), "utf8"));
  if (!/^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$/.test(info.version) || !/^[a-f0-9]{64}$/.test(info.source_sha256)) throw new Error("Invalid build identity.");
  const executableBytes = readFileSync(exe);
  if (!executableBytes.includes(Buffer.from(info.source_sha256))) throw new Error("Executable does not contain this prepared source fingerprint; rebuild before archiving.");
  if (currentSourceHash(root) !== info.source_sha256) throw new Error("Source files changed during the build; rebuild before archiving.");
  const pdbBytes = readFileSync(pdb), identity = symbolIdentity(executableBytes, pdbBytes);
  const exeHash = sha256(executableBytes), pdbHash = sha256(pdbBytes);
  const directory = join(root, "dist/windows-symbols", `${info.version}-${info.source_sha256.slice(0, 12)}-${exeHash.slice(0, 12)}-${pdbHash.slice(0, 8)}`);
  const manifestPath = join(directory, "manifest.json");
  if (existsSync(manifestPath)) {
    const prior = JSON.parse(readFileSync(manifestPath, "utf8"));
    if (prior.exe_sha256 !== exeHash || prior.pdb_sha256 !== pdbHash) throw new Error("Archive identity collision.");
    if (sha256(readFileSync(join(directory, "TokenMe.exe"))) !== exeHash || sha256(readFileSync(join(directory, "tokenme.pdb"))) !== pdbHash) throw new Error("Archived EXE/PDB files were altered.");
    return directory;
  }
  mkdirSync(directory, { recursive: true });
  copyFileSync(exe, join(directory, "TokenMe.exe"));
  copyFileSync(pdb, join(directory, "tokenme.pdb"));
  copyFileSync(join(staged, "source.patch"), join(directory, "source.patch"));
  const sources = JSON.parse(readFileSync(join(staged, "extra-sources.json"), "utf8"));
  for (const [path, bytes] of Object.entries(sources)) {
    const base = join(directory, "sources"), destination = resolve(base, path);
    const inside = relative(base, destination);
    if (isAbsolute(path) || inside.startsWith("..") || isAbsolute(inside)) throw new Error("Invalid archived source path.");
    mkdirSync(dirname(destination), { recursive: true });
    writeFileSync(destination, Buffer.from(bytes, "base64"));
  }
  writeFileSync(manifestPath, JSON.stringify({ ...info, exe_sha256: exeHash, pdb_sha256: pdbHash, pdb_guid: identity.guid, pdb_age: identity.age, archived_at: new Date().toISOString() }, null, 2));
  return directory;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const index = process.argv.indexOf("--exe");
  if (index < 0 || !process.argv[index + 1]) throw new Error("Pass --exe with the exact built executable path.");
  console.log(`Windows EXE/PDB archived: ${archive(process.argv[index + 1])}`);
}

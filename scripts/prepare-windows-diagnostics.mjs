import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, lstatSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const stagedDirectory = (root) => join(root, "apps/tokenme-bar/src-tauri/resources/diagnostics");

function git(root, args) {
  const result = spawnSync("git", args, { cwd: root, encoding: "utf8", windowsHide: true });
  if (result.status !== 0) throw new Error(`git ${args[0]} failed`);
  return result.stdout;
}

function isBuildSource(path) {
  if (/^(?:\.workbuddy-ai|\.bugx|docs\/internal)\//.test(path)) return false;
  return ["Cargo.toml", "Cargo.lock", ".cargo/config.toml"].includes(path)
    || path.startsWith("crates/") || path.startsWith("scripts/") || path.startsWith(".github/")
    || path.startsWith("apps/tokenme-bar/");
}

function sourceFiles(root) {
  return git(root, ["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
    .split("\0").filter(isBuildSource).filter((path) => existsSync(join(root, path))).sort();
}

export function currentSourceHash(root = repoRoot, files = sourceFiles(root)) {
  const hash = createHash("sha256");
  for (const path of files) {
    if (lstatSync(join(root, path)).isSymbolicLink()) continue;
    hash.update(path).update("\0").update(createHash("sha256").update(readFileSync(join(root, path))).digest());
  }
  return hash.digest("hex");
}

export function prepare(root = repoRoot) {
  const files = sourceFiles(root), sourceHash = currentSourceHash(root, files);
  const untracked = new Set(git(root, ["ls-files", "--others", "--exclude-standard", "-z"]).split("\0"));
  const extraSources = {};
  for (const path of files) {
    // Do not follow a symlink outside the checkout into personal files.
    if (lstatSync(join(root, path)).isSymbolicLink()) continue;
    const bytes = readFileSync(join(root, path));
    if (untracked.has(path) && /\.(?:rs|mjs|ps1|toml|json)$/.test(path)) extraSources[path] = bytes.toString("base64");
  }
  const version = JSON.parse(readFileSync(join(root, "apps/tokenme-bar/src-tauri/tauri.conf.json"), "utf8")).version;
  const rustc = spawnSync("rustc", ["--version"], { cwd: root, encoding: "utf8", windowsHide: true });
  const info = {
    schema: 1, version,
    build_id: readFileSync(join(root, "apps/tokenme-bar/src-tauri/BUILD_ID"), "utf8").trim(),
    git_revision: git(root, ["rev-parse", "HEAD"]).trim(),
    source_sha256: sourceHash,
    dirty: files.some((path) => untracked.has(path)) || Boolean(git(root, ["diff", "HEAD", "--name-only"]).trim()),
    rustc: rustc.status === 0 ? rustc.stdout.trim() : "unknown",
    target: process.env.TAURI_ENV_TARGET_TRIPLE ?? "x86_64-pc-windows-msvc",
  };
  const out = stagedDirectory(root);
  mkdirSync(out, { recursive: true });
  writeFileSync(join(out, "build-info.json"), JSON.stringify(info));
  // These source records are archived locally; bundle.resources names only
  // the diagnostic helpers, so neither a patch nor PDB is shipped to users.
  writeFileSync(join(out, "source.patch"), git(root, ["diff", "--binary", "HEAD", "--", "Cargo.toml", "Cargo.lock", "crates", "apps/tokenme-bar", "scripts", ".github"]));
  writeFileSync(join(out, "extra-sources.json"), JSON.stringify(extraSources));
  return info;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const info = prepare();
  console.log(`Windows diagnostic build: ${info.git_revision.slice(0, 8)} / ${info.source_sha256.slice(0, 12)}${info.dirty ? " (local changes)" : ""}`);
}

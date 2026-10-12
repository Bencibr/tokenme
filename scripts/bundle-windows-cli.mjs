// Build the console CLI separately from the case-insensitive TokenMe GUI name.
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const target = process.env.TAURI_ENV_TARGET_TRIPLE;
const args = ["build", "--release", "-p", "usage-cli", "--bin", "tokenme"];
if (target) args.push("--target", target);
const build = spawnSync("cargo", args, { cwd: root, stdio: "inherit", windowsHide: true });
if (build.error) throw build.error;
if (build.status !== 0) process.exit(build.status ?? 1);

// Cargo metadata honours CARGO_TARGET_DIR and workspace cargo configuration.
const metadata = spawnSync("cargo", ["metadata", "--no-deps", "--format-version", "1"], {
  cwd: root, encoding: "utf8", windowsHide: true,
});
if (metadata.error) throw metadata.error;
if (metadata.status !== 0) throw new Error(metadata.stderr);
const targetDir = JSON.parse(metadata.stdout).target_directory;
const binary = join(targetDir, ...(target ? [target] : []), "release", "tokenme.exe");
const staged = join(root, "apps/tokenme-bar/src-tauri/resources/cli");
mkdirSync(staged, { recursive: true });
copyFileSync(binary, join(staged, "tokenme.exe"));
console.log("Windows CLI staged as bin/tokenme.exe");

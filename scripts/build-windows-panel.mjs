// Tauri's normal NSIS build archives through installer-hooks.nsh. This wrapper
// also archives --no-bundle builds, which do not run an installer finalizer.
import { spawn } from "node:child_process";
import { join } from "node:path";
import { archive } from "./archive-windows-build.mjs";
import { repoRoot } from "./prepare-windows-diagnostics.mjs";

if (process.platform !== "win32") throw new Error("Run this Windows panel builder on Windows.");
const app = join(repoRoot, "apps/tokenme-bar");
const cli = join(app, "node_modules/@tauri-apps/cli/tauri.js");
const args = process.argv.slice(2);
const child = spawn(process.execPath, [cli, "build", ...args], { cwd: app, stdio: ["inherit", "pipe", "pipe"], windowsHide: true });
let tail = "";
for (const [stream, output] of [[child.stdout, process.stdout], [child.stderr, process.stderr]]) {
  stream.on("data", (data) => { output.write(data); tail = (tail + data.toString().replace(/\x1b\[[0-9;]*m/g, "")).slice(-32768); });
}
child.on("error", (error) => { throw error; });
child.on("close", (code) => {
  if (code !== 0) { process.exitCode = code ?? 1; return; }
  const matches = [...tail.matchAll(/Built application at:\s*([^\r\n]+)/g)];
  if (!matches.length) throw new Error("The build succeeded but its executable path was not reported.");
  console.log(`Windows build archived: ${archive(matches.at(-1)[1].trim())}`);
});

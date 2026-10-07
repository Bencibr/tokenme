/**
 * In-memory stand-in for the Rust `servers/*` commands, so `pnpm dev` can
 * render and drive the whole sheet in a plain browser (QA / screenshots).
 * Timers only — nothing persists, no socket is ever opened.
 *
 * QA pins, same family as `?lang=` / `?stale=`:
 *   ?srv=preset    start with one healthy server and one failing one
 *   ?srv=mismatch  the next probe answers with a fingerprint mismatch
 *   ?srv=badarch   the next probe answers with an unsupported architecture
 */

import type {
  InstallStep,
  ServerInstallOutcome,
  ServerInstallProgress,
  ServerInstallReq,
  ServerProbeOutcome,
  ServerProbeReq,
  ServerRemoveOutcome,
  ServerUpdateReq,
  ServerView,
} from "../types";

const PIN = typeof location !== "undefined" ? new URLSearchParams(location.search).get("srv") : null;

const FP = "SHA256:yFk8Rk3pQmTz6Wc1Jd94Lx2Hn7Vs0Gq5Ue8Ab3MoP1w";
const FP_OLD = "SHA256:sQ2nB7vT1xWk4Jm8Rc5Pe0Dz9Gh3La6Yu2Vf7iN4QoXk";

const STEP_KEYS = ["keygen", "pubkey", "reconnect", "clear_pw", "arch", "collector", "export", "detect", "merge"];

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const now = () => Date.now();
const clone = <T,>(v: T): T => JSON.parse(JSON.stringify(v)) as T;

let nextId = 1;
let servers: ServerView[] = [];
const sessions = new Map<number, { name: string; host: string; port: number; user: string; fingerprint: string }>();
const onUpdated = new Set<(servers: ServerView[]) => void>();
const onProgress = new Set<(p: ServerInstallProgress) => void>();

function emitUpdated() {
  const snap = clone(servers);
  for (const h of onUpdated) h(snap);
}

function emitProgress(p: ServerInstallProgress) {
  for (const h of onProgress) h(p);
}

function makeSteps(state: InstallStep["state"] = "pending"): InstallStep[] {
  return STEP_KEYS.map((key) => ({ key, state, detail: null }));
}

(function seed() {
  if (PIN !== "preset") return;
  const t = now();
  servers = [
    {
      id: nextId++,
      name: "ops-box",
      host: "ops.example.com",
      port: 22,
      user: "ubuntu",
      fingerprint: FP,
      every_secs: 900,
      days: 30,
      enabled: true,
      status: "ok",
      last_ok_ms: t - 3 * 60_000,
      last_error: null,
      last_rows: 41_455,
      last_took_ms: 2400,
      next_due_ms: t + 12 * 60_000,
      tools: ["Claude Code", "Codex", "Qoder"],
      history: [
        // chronological, oldest first — the wire order `push_history` writes
        { at_ms: t - 18 * 60_000, rows: 41_290, took_ms: 6100, ok: true },
        { at_ms: t - 3 * 60_000, rows: 41_455, took_ms: 2400, ok: true },
      ],
    },
    {
      id: nextId++,
      name: "edge-node",
      host: "edge.example.com",
      port: 2222,
      user: "deploy",
      fingerprint: FP,
      every_secs: 3600,
      days: 90,
      enabled: true,
      status: "error",
      last_ok_ms: t - 3 * 3_600_000,
      last_error: { kind: "unreachable", detail: "connect to edge.example.com:2222 failed: connection refused" },
      last_rows: 8_102,
      last_took_ms: 5100,
      next_due_ms: t + 8 * 60_000,
      tools: ["Claude Code", "MiniMax Code"],
      history: [
        // chronological, oldest first
        { at_ms: t - 3 * 3_600_000, rows: 8_102, took_ms: 5100, ok: true },
        { at_ms: t - 30 * 60_000, rows: 0, took_ms: 10_000, ok: false },
      ],
    },
  ];
})();

export const serverMock = {
  servers: async (): Promise<ServerView[]> => clone(servers),

  publicKey: async (): Promise<{ path: string; line: string }> => ({
    path: "~/.config/tokenme/tokenme_ed25519",
    line: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFixtureTokenmeAgentKeyPlaceholder tokenme@localhost",
  }),

  probe: async (req: ServerProbeReq): Promise<ServerProbeOutcome> => {
    const fail = (kind: string, detail: string): ServerProbeOutcome => ({
      ok: false,
      session: null,
      fingerprint: null,
      known_fingerprint: null,
      mismatch: false,
      arch: null,
      hostname: null,
      error: { kind, detail },
    });
    if (!/^[A-Za-z0-9._-]{1,64}$/.test(req.name)) return fail("name", "name must be 1–64 characters of A–Z a–z 0–9 . _ -");
    if (servers.some((s) => s.name === req.name)) return fail("name", "another server already uses this name");
    if (!req.host || !req.user) return fail("host", "invalid host");

    await sleep(700);
    if (req.host.includes("fail")) {
      return fail("unreachable", `connect to ${req.host}:${req.port ?? 22} failed: connection refused`);
    }
    if (req.auth.kind === "password" && req.auth.password === "bad") {
      return fail("auth", "the server rejected the username or password");
    }
    if (PIN === "badarch") {
      return { ...fail("unsupported_arch", 'architecture "armv7l" has no bundled collector — use scripts/install-linux.sh --push on this machine'), fingerprint: FP };
    }
    const fp = PIN === "mismatch" ? "SHA256:New9vQx2Zk4Jm7Rt1Wp5Lc8De0Gh3Yu6Fb8Ns2Vr4TqXw" : FP;
    const session = nextId++;
    sessions.set(session, { name: req.name, host: req.host, port: req.port ?? 22, user: req.user, fingerprint: fp });
    return {
      ok: true,
      session,
      fingerprint: fp,
      known_fingerprint: PIN === "mismatch" ? FP_OLD : null,
      mismatch: PIN === "mismatch",
      arch: "x86_64",
      hostname: req.host.split(".")[0] || req.host,
      error: null,
    };
  },

  install: async (req: ServerInstallReq): Promise<ServerInstallOutcome> => {
    const session = sessions.get(req.session);
    if (!session) {
      return { ok: false, server: null, detected: [], rows: 0, merge_pending: false, error: { kind: "proto", detail: "the wizard session expired — start over" } };
    }
    sessions.delete(req.session);

    const steps = makeSteps();
    const emit = (done: boolean, ok: boolean) =>
      emitProgress({ session: req.session, steps: clone(steps), done, ok, error: null });
    emit(false, true);

    const details: Record<string, string> = {
      keygen: "~/.config/tokenme/tokenme_ed25519",
      arch: "x86_64",
      collector: "uploaded and verified",
      detect: "3 tool(s)",
      merge: "41,290 rows merged",
    };
    for (const step of steps) {
      step.state = "active";
      emit(false, true);
      await sleep(step.key === "export" ? 1600 : 350);
      step.state = "done";
      step.detail = details[step.key] ?? null;
      emit(false, true);
    }

    const t = now();
    const view: ServerView = {
      id: nextId++,
      name: session.name,
      host: session.host,
      port: session.port,
      user: session.user,
      fingerprint: session.fingerprint,
      every_secs: req.every_secs,
      days: req.days,
      enabled: true,
      status: "ok",
      last_ok_ms: t,
      last_error: null,
      last_rows: 41_290,
      last_took_ms: 6100,
      next_due_ms: t + req.every_secs * 1000,
      tools: ["Claude Code", "Codex", "Qoder"],
      history: [{ at_ms: t, rows: 41_290, took_ms: 6100, ok: true }],
    };
    servers = [...servers, view];
    emitUpdated();
    emitProgress({ session: req.session, steps: clone(steps), done: true, ok: true, error: null });
    return { ok: true, server: clone(view), detected: view.tools, rows: view.last_rows, merge_pending: false, error: null };
  },

  abort: async (session: number): Promise<void> => {
    sessions.delete(session);
  },

  syncNow: async (id: number): Promise<void> => {
    servers = servers.map((s) => (s.id === id ? { ...s, status: "syncing" as const } : s));
    emitUpdated();
    await sleep(2400);
    servers = servers.map((s) => {
      if (s.id !== id) return s;
      const t = now();
      const rows = s.last_rows + 165;
      return {
        ...s,
        status: "ok" as const,
        last_ok_ms: t,
        last_error: null,
        last_rows: rows,
        last_took_ms: 2400,
        next_due_ms: t + s.every_secs * 1000,
        history: [...s.history, { at_ms: t, rows, took_ms: 2400, ok: true }].slice(-20),
      };
    });
    emitUpdated();
  },

  update: async (req: ServerUpdateReq): Promise<ServerView[]> => {
    servers = servers.map((s) =>
      s.id === req.id
        ? {
            ...s,
            every_secs: req.every_secs ?? s.every_secs,
            days: req.days ?? s.days,
            enabled: req.enabled ?? s.enabled,
          }
        : s,
    );
    emitUpdated();
    return clone(servers);
  },

  remove: async (id: number, cleanupRemote: boolean): Promise<ServerRemoveOutcome> => {
    servers = servers.filter((s) => s.id !== id);
    emitUpdated();
    if (cleanupRemote) await sleep(400);
    return { removed: true, cleaned: cleanupRemote ? true : null, cleanup_error: null };
  },

  onServersUpdated: (handler: (servers: ServerView[]) => void): (() => void) => {
    onUpdated.add(handler);
    return () => onUpdated.delete(handler);
  },

  onInstallProgress: (handler: (p: ServerInstallProgress) => void): (() => void) => {
    onProgress.add(handler);
    return () => onProgress.delete(handler);
  },
};

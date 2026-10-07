import { useEffect, useMemo, useRef, useState } from "react";
import type {
  InstallStep,
  ServerInstallOutcome,
  ServerProbeOutcome,
  ServerSyncEvent,
  ServerView,
  SshError,
} from "../types";
import { bridge } from "../lib/bridge";
import { count, relativeTime } from "../lib/format";
import { t, type StrKey } from "../lib/i18n";
import { useTicker } from "../lib/hooks";
import { IconBack, IconCheck, IconClose, IconNext, IconRefresh, IconWarn } from "./Icons";

/** Failure kinds → titles. `detail` (raw text from the connection layer) is
 *  always shown under the title, so an unmapped kind still reads fine. */
const ERR_KEY: Record<string, StrKey> = {
  auth: "srv.err.auth",
  unreachable: "srv.err.unreachable",
  dns: "srv.err.dns",
  timeout: "srv.err.timeout",
  host_key_changed: "srv.err.hostkey",
  key_missing: "srv.err.key",
  collector_missing: "srv.err.collector",
  bundle_too_big: "srv.err.bundle",
  sftp: "srv.err.sftp",
  remote_cmd: "srv.err.remote",
  local_io: "srv.err.local",
  proto: "srv.err.proto",
  unsupported_arch: "srv.err.arch",
  name: "srv.err.input",
  host: "srv.err.input",
  user: "srv.err.input",
};
const errTitle = (e: SshError): string => t(ERR_KEY[e.kind] ?? "srv.err.other");

const STEP_KEY: Record<string, StrKey> = {
  keygen: "srv.step.keygen",
  pubkey: "srv.step.pubkey",
  reconnect: "srv.step.reconnect",
  clear_pw: "srv.step.clear_pw",
  arch: "srv.step.arch",
  collector: "srv.step.collector",
  export: "srv.step.export",
  detect: "srv.step.detect",
  merge: "srv.step.merge",
};

const INTERVALS = [300, 900, 3600];
const WINDOWS = [7, 30, 90];

/** Clipboard with an execCommand fallback (older WKWebView gates the async API). */
async function copyText(s: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(s);
    return true;
  } catch {
    /* fall through */
  }
  try {
    const ta = document.createElement("textarea");
    ta.value = s;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}

/** "14 分钟" / "2 小时" / "不到 1 分钟" — a future span, for "下次 …". */
function eta(ms: number): string {
  if (ms < 60_000) return t("srv.eta.soon");
  const min = Math.round(ms / 60_000);
  if (min < 60) return t("srv.eta.min", { n: min });
  return t("srv.eta.hour", { n: Math.max(1, Math.round(min / 60)) });
}

function tookText(ms: number): string {
  if (ms <= 0) return "—";
  return `${(ms / 1000).toFixed(1)} s`;
}

/** The list/detail meta line: what happened last and when it happens next. */
function metaOf(s: ServerView, now: number): { text: string; err: boolean } {
  if (s.status === "syncing") return { text: t("srv.meta.syncing"), err: false };
  if (s.status === "error" && s.last_error) {
    const last = s.history.length ? s.history[s.history.length - 1].at_ms : s.last_ok_ms;
    const ago = last === null ? "—" : relativeTime(last, now);
    const next = s.next_due_ms !== null && s.next_due_ms > now
      ? t("srv.meta.backoff", { v: eta(s.next_due_ms - now) })
      : t("srv.meta.retry");
    return { text: `${ago} · ${errTitle(s.last_error)} · ${next}`, err: true };
  }
  if (!s.enabled) return { text: t("srv.meta.disabled"), err: false };
  const next = s.next_due_ms !== null && s.next_due_ms > now ? eta(s.next_due_ms - now) : "—";
  const rows = t("srv.meta.rows", { n: count(s.last_rows) });
  if (s.last_ok_ms === null) return { text: t("srv.meta.wait", { eta: next }), err: false };
  return { text: t("srv.meta.ok", { ago: relativeTime(s.last_ok_ms, now), rows, eta: next }), err: false };
}

/** history rows with the cumulative-count delta each merge added. */
function historyRows(history: ServerSyncEvent[], now: number) {
  let prevOk: number | null = null;
  const out = history.map((ev) => {
    const delta = ev.ok && prevOk !== null ? ev.rows - prevOk : ev.ok ? ev.rows : null;
    if (ev.ok) prevOk = ev.rows;
    return { ev, delta, ago: relativeTime(ev.at_ms, now) };
  });
  out.reverse();
  return out.slice(0, 6);
}

type View = { kind: "list" } | { kind: "wizard" } | { kind: "detail"; id: number } | { kind: "confirm"; id: number };

interface Wiz {
  step: 1 | 2 | 3 | 4 | 5;
  host: string;
  port: string;
  user: string;
  name: string;
  auth: "password" | "key" | "default";
  password: string;
  keyPath: string;
  passphrase: string;
  altOpen: boolean;
  pubkey: { path: string; line: string } | null;
  busy: boolean;
  formError: string | null;
  probeError: SshError | null;
  probe: ServerProbeOutcome | null;
  ack: boolean;
  session: number | null;
  steps: InstallStep[];
  installError: SshError | null;
  outcome: ServerInstallOutcome | null;
  every: number;
  days: number;
  version: string;
}

const emptyWiz = (): Wiz => ({
  step: 1,
  host: "",
  port: "22",
  user: "",
  name: "",
  auth: "password",
  password: "",
  keyPath: "",
  passphrase: "",
  altOpen: false,
  pubkey: null,
  busy: false,
  formError: null,
  probeError: null,
  probe: null,
  ack: false,
  session: null,
  steps: [],
  installError: null,
  outcome: null,
  every: 900,
  days: 30,
  version: "",
});

/** First label of the typed host, sanitized to the origin charset — the
 *  promised "default to the server hostname" can only happen after connect,
 *  and the name is validated before it, so this is what the name defaults to. */
function deriveName(host: string): string {
  const first = host.trim().split(".")[0] || host.trim();
  const clean = first.replace(/[^A-Za-z0-9_-]/g, "-").replace(/^-+|-+$/g, "");
  return clean.slice(0, 64) || "server";
}

/** A form field with the trailing clear affordance: the × shows only when
 *  there is something to clear, clearing keeps the field focused (the
 *  mousedown that would otherwise blur it is swallowed), and the parent's
 *  change handler travels as text so clearing is an ordinary empty value. */
function ClearableInput({
  value,
  onChangeText,
  className = "f-input",
  ...rest
}: Omit<React.ComponentProps<"input">, "value" | "onChange"> & {
  value: string;
  onChangeText: (text: string) => void;
  className?: string;
}) {
  const ref = useRef<HTMLInputElement>(null);
  const filled = value.length > 0;
  return (
    <span className="f-input-wrap">
      <input
        ref={ref}
        {...rest}
        className={filled ? `${className} has-clear` : className}
        value={value}
        onChange={(e) => onChangeText(e.target.value)}
      />
      {filled ? (
        <button
          type="button"
          className="f-clear"
          aria-label={t("srv.f.clear")}
          title={t("srv.f.clear")}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => {
            onChangeText("");
            ref.current?.focus();
          }}
        >
          <IconClose size={11} />
        </button>
      ) : null}
    </span>
  );
}

/**
 * The remote-server sheet: list → wizard (5 steps) → detail → removal.
 * All state changes that outlive this component arrive via the
 * `servers-updated` event; the local copy is only for in-flight form state.
 */
export function ServerSheet({
  servers,
  onServers,
  onClose,
  closeRef,
}: {
  servers: ServerView[];
  /** server_update answers with fresh views; the event usually beats it. */
  onServers: (servers: ServerView[]) => void;
  onClose: () => void;
  /** App's Escape path asks through this so a running install cannot be
   *  dismissed mid-flight (the close button is guarded the same way). */
  closeRef: React.MutableRefObject<() => void>;
}) {
  const [view, setView] = useState<View>({ kind: "list" });
  const [wiz, setWiz] = useState<Wiz | null>(null);
  const [cleanup, setCleanup] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const tick = useTicker(30_000);
  const now = useMemo(() => Date.now(), [tick]);
  const copyTimer = useRef<number | null>(null);

  const installing = view.kind === "wizard" && wiz?.busy === true;
  const close = () => {
    if (installing) return;
    // A session that was probed but never installed still holds the password;
    // dropping it zeroizes. After a successful install it is already consumed.
    if (wiz?.session != null) void bridge.serverAbortSession(wiz.session);
    onClose();
  };
  useEffect(() => {
    closeRef.current = close;
  });

  // Install progress streams in while step 4 is up. Filtered by session so a
  // stale event from a previous attempt can never repaint this wizard.
  useEffect(
    () =>
      bridge.onInstallProgress((p) => {
        setWiz((w) => (w && w.session !== null && p.session === w.session ? { ...w, steps: p.steps } : w));
      }),
    [],
  );

  const openWizard = () => {
    setNotice(null);
    setWiz(emptyWiz());
    setView({ kind: "wizard" });
    void bridge.panelSettings().then((s) => setWiz((w) => (w ? { ...w, version: s.version } : w)));
  };

  const showCopied = (ok: boolean) => {
    setCopied(ok);
    if (copyTimer.current !== null) window.clearTimeout(copyTimer.current);
    copyTimer.current = window.setTimeout(() => setCopied(false), 1600);
  };
  const copyKey = async () => {
    const pk = await bridge.serverPublicKey().catch(() => null);
    showCopied(pk ? await copyText(pk.line) : false);
  };

  /* ── wizard transitions ─────────────────────────────────────────────── */

  const step1Next = () => {
    const w = wiz!;
    const host = w.host.trim();
    const user = w.user.trim();
    const name = w.name.trim() || deriveName(host);
    if (!host || !user) {
      setWiz({ ...w, formError: t("srv.f.req") });
      return;
    }
    if (!/^[A-Za-z0-9._-]{1,64}$/.test(name)) {
      setWiz({ ...w, formError: t("srv.f.name.bad") });
      return;
    }
    setWiz({ ...w, host, user, name, step: 2, formError: null, probeError: null });
  };

  /** Probe with the currently selected auth. `overrideKeyPath` carries the
   *  manual-pubkey flow, which authenticates with the dedicated key. */
  const runProbe = async (w: Wiz, overrideKeyPath?: string) => {
    const req = {
      name: w.name,
      host: w.host,
      port: Number(w.port) || 22,
      user: w.user,
      auth:
        overrideKeyPath !== undefined
          ? ({ kind: "key", path: overrideKeyPath } as const)
          : w.auth === "password"
            ? ({ kind: "password", password: w.password } as const)
            : w.auth === "key"
              ? ({ kind: "key", path: w.keyPath.trim(), passphrase: w.passphrase || null } as const)
              : ({ kind: "default" } as const),
    };
    setWiz({ ...w, busy: true, probeError: null, formError: null });
    const out = await bridge.serverProbe(req).catch((e: unknown) => ({
      ok: false,
      session: null,
      fingerprint: null,
      known_fingerprint: null,
      mismatch: false,
      arch: null,
      hostname: null,
      error: { kind: "proto", detail: e instanceof Error ? e.message : String(e) },
    }));
    setWiz((cur) => {
      if (!cur) return cur;
      if (!out.ok) return { ...cur, busy: false, probeError: out.error, session: null };
      return { ...cur, busy: false, probe: out, session: out.session, ack: false, step: 3 };
    });
  };

  const step2Connect = () => {
    const w = wiz!;
    if (w.auth === "password" && !w.password) {
      setWiz({ ...w, formError: t("srv.auth.pwd.ph") });
      return;
    }
    if (w.auth === "key" && !w.keyPath.trim()) {
      setWiz({ ...w, formError: t("srv.auth.key.ph") });
      return;
    }
    void runProbe(w);
  };

  const altDone = async () => {
    const w = wiz!;
    const pk = w.pubkey ?? (await bridge.serverPublicKey().catch(() => null));
    if (!pk) return;
    setWiz({ ...w, pubkey: pk });
    void runProbe(w, pk.path);
  };

  const wizBack = (to: 1 | 2) => {
    const w = wiz!;
    if (w.step === 2 && w.busy) return;
    if (w.step === 3 && w.session !== null) void bridge.serverAbortSession(w.session);
    setWiz({ ...w, step: to, probeError: null, formError: null, session: to < 3 ? null : w.session, busy: false });
  };

  const startInstall = async () => {
    const w = wiz!;
    if (w.session === null || !w.probe?.fingerprint) return;
    const req = { session: w.session, every_secs: w.every, days: w.days, fingerprint: w.probe.fingerprint };
    setWiz({ ...w, step: 4, busy: true, installError: null, steps: [], outcome: null });
    const out = await bridge.serverInstall(req).catch((e: unknown) => ({
      ok: false,
      server: null,
      detected: [],
      rows: 0,
      merge_pending: false,
      error: { kind: "proto", detail: e instanceof Error ? e.message : String(e) },
    }));
    setWiz((cur) => (cur ? { ...cur, busy: false, outcome: out, installError: out.error, step: out.ok ? 5 : 4 } : cur));
  };

  const patchServer = async (id: number, req: { every_secs?: number; days?: number; enabled?: boolean }) => {
    const views = await bridge.serverUpdate({ id, ...req }).catch(() => null);
    if (views) onServers(views);
  };

  const removeServer = async (id: number) => {
    const s = servers.find((v) => v.id === id);
    const out = await bridge
      .serverRemove(id, cleanup)
      .catch((e: unknown) => ({ removed: false, cleaned: null, cleanup_error: { kind: "proto", detail: String(e) } }));
    setCleanup(false);
    setView({ kind: "list" });
    if (!s) return;
    if (out.cleaned === false && out.cleanup_error) {
      setNotice(t("srv.rm.done.cleanupfail", { name: s.name, e: errTitle(out.cleanup_error) }));
    } else if (out.cleaned === true) {
      setNotice(t("srv.rm.done.cleaned", { name: s.name }));
    } else {
      setNotice(t("srv.rm.done", { name: s.name }));
    }
  };

  /* ── shared bits ────────────────────────────────────────────────────── */

  const errBox = (e: SshError, actions?: { label: string; onClick: () => void }[]) => (
    <div className="err-box">
      <div className="t">{errTitle(e)}</div>
      <div className="d">{e.detail}</div>
      {actions && actions.length ? (
        <div className="a">
          {actions.map((a) => (
            <button key={a.label} type="button" className="btn-quiet" onClick={a.onClick}>
              {a.label}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );

  const noticeBar = notice ? (
    <div className="srv-notice" role="status">
      {notice}
      <button type="button" className="sheet-close" aria-label={t("srv.cancel")} onClick={() => setNotice(null)}>
        <IconClose size={10} />
      </button>
    </div>
  ) : null;

  /* ── list ───────────────────────────────────────────────────────────── */

  const listBody = (
    <>
      <div className="sheet-hd">
        <h2 className="srv-hd">{t("srv.title")}</h2>
        <button type="button" className="sheet-close" aria-label={t("srv.cancel")} onClick={close}>
          <IconClose size={12} />
        </button>
      </div>
      {noticeBar}
      <div className="sheet-body">
        {servers.length === 0 ? (
          <div className="srv-empty">
            <div className="big">{t("srv.empty.big")}</div>
            <div className="hint">{t("srv.empty.hint")}</div>
          </div>
        ) : (
          servers.map((s) => {
            const meta = metaOf(s, now);
            return (
              <div
                key={s.id}
                className="srv-row"
                role="button"
                tabIndex={0}
                onClick={() => setView({ kind: "detail", id: s.id })}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    setView({ kind: "detail", id: s.id });
                  }
                }}
              >
                <span className="srv-dot" data-s={s.status} aria-hidden="true" />
                <span className="srv-main">
                  <span className="srv-name">
                    {s.name} <span className="srv-host">{`${s.user}@${s.host}:${s.port}`}</span>
                  </span>
                  <span className="srv-meta num" data-err={meta.err || undefined}>
                    {meta.text}
                  </span>
                </span>
                <span className="srv-act">
                  <button
                    type="button"
                    className="icon-btn"
                    title={t("srv.sync.now")}
                    aria-label={t("srv.sync.now")}
                    disabled={s.status === "syncing"}
                    onClick={(e) => {
                      e.stopPropagation();
                      void bridge.serverSyncNow(s.id);
                    }}
                  >
                    {s.status === "syncing" ? <span className="spin" /> : <IconRefresh size={13} />}
                  </button>
                  <span className="icon-btn" aria-hidden="true">
                    <IconNext size={12} />
                  </span>
                </span>
              </div>
            );
          })
        )}
      </div>
      <div className="sheet-foot">
        <button type="button" className="btn-accent srv-add" onClick={openWizard}>
          ＋ {t("srv.add")}
        </button>
        {servers.length === 0 ? <div className="foot-note">{t("srv.foot.note")}</div> : null}
      </div>
    </>
  );

  /* ── wizard ─────────────────────────────────────────────────────────── */

  const w = wiz;
  let wizardBody = listBody;
  if (view.kind === "wizard" && w) {
    const titles: Record<number, string> = {
      1: t("srv.wiz.add"),
      2: t("srv.wiz.auth"),
      3: t("srv.wiz.fp"),
      4: t("srv.wiz.install"),
      5: t("srv.wiz.done"),
    };

    let body: React.ReactNode = null;
    if (w.step === 1) {
      body = (
        <>
          <div className="f-row">
            <div className="f-label">
              <span>{t("srv.f.host")}</span>
            </div>
            <ClearableInput
              value={w.host}
              onChangeText={(host) => setWiz({ ...w, host })}
              placeholder={t("srv.f.host.ph")}
              autoComplete="off"
              spellCheck={false}
              autoFocus
            />
          </div>
          <div className="f-grid">
            <div className="f-row">
              <div className="f-label">
                <span>{t("srv.f.user")}</span>
              </div>
              <ClearableInput
                value={w.user}
                onChangeText={(user) => setWiz({ ...w, user })}
                placeholder={t("srv.f.user.ph")}
                autoComplete="off"
                spellCheck={false}
              />
            </div>
            <div className="f-row">
              <div className="f-label">
                <span>{t("srv.f.port")}</span>
              </div>
              <ClearableInput
                className="f-input num"
                value={w.port}
                onChangeText={(port) => setWiz({ ...w, port })}
                inputMode="numeric"
              />
            </div>
          </div>
          <div className="f-row">
            <div className="f-label">
              <span>{t("srv.f.name")}</span>
              <span className="opt">{t("srv.f.opt")}</span>
            </div>
            <ClearableInput
              value={w.name}
              onChangeText={(name) => setWiz({ ...w, name })}
              placeholder={t("srv.f.name.ph")}
              autoComplete="off"
              spellCheck={false}
            />
          </div>
          <div className="f-hint" data-err={w.formError ? "" : undefined}>
            {w.formError ?? t("srv.f.hint1")}
          </div>
          <div className="wiz-foot">
            <div className="left">
              <button type="button" className="btn-quiet" onClick={close}>
                {t("srv.cancel")}
              </button>
            </div>
            <div className="left">
              <button type="button" className="btn-accent" onClick={step1Next}>
                {t("srv.next.auth")}
              </button>
            </div>
          </div>
        </>
      );
    } else if (w.step === 2) {
      const alt = w.altOpen ? (
        <div className="alt-panel">
          <div className="auth-name">{t("srv.auth.alt.title")}</div>
          <div className="f-hint">{t("srv.auth.alt.hint", { f: "~/.ssh/authorized_keys" })}</div>
          <div className="mono">{w.pubkey?.line ?? "…"}</div>
          <div className="alt-actions">
            <button
              type="button"
              className="btn-quiet"
              onClick={() => void copyKey()}
            >
              {copied ? t("srv.copy.done") : t("srv.auth.alt.copy")}
            </button>
            <button type="button" className="btn-accent" disabled={w.busy} onClick={() => void altDone()}>
              {t("srv.auth.alt.test")}
            </button>
          </div>
        </div>
      ) : null;
      body = (
        <>
          {w.probeError ? errBox(w.probeError, probeActions(w, () => void runProbe(w))) : null}
          <div className="auth-cards" role="radiogroup" aria-label={t("srv.wiz.auth")}>
            <button
              type="button"
              role="radio"
              aria-checked={w.auth === "password"}
              className="auth-card"
              onClick={() => setWiz({ ...w, auth: "password", probeError: null, formError: null })}
            >
              <span className="auth-radio" aria-hidden="true" />
              <span>
                <span className="auth-name">{t("srv.auth.password")}</span>
                <span className="auth-sub">{t("srv.auth.password.sub")}</span>
              </span>
            </button>
            {w.auth === "password" ? (
              <div className="auth-field">
                <ClearableInput
                  type="password"
                  value={w.password}
                  onChangeText={(password) => setWiz({ ...w, password })}
                  placeholder={t("srv.auth.pwd.ph")}
                  autoComplete="off"
                  autoFocus
                />
              </div>
            ) : null}
            <button
              type="button"
              role="radio"
              aria-checked={w.auth === "key"}
              className="auth-card"
              onClick={() => setWiz({ ...w, auth: "key", password: "", probeError: null, formError: null })}
            >
              <span className="auth-radio" aria-hidden="true" />
              <span>
                <span className="auth-name">{t("srv.auth.key")}</span>
                <span className="auth-sub">{t("srv.auth.key.sub")}</span>
              </span>
            </button>
            {w.auth === "key" ? (
              <div className="auth-field">
                <ClearableInput
                  className="f-input mono"
                  value={w.keyPath}
                  onChangeText={(keyPath) => setWiz({ ...w, keyPath })}
                  placeholder={t("srv.auth.key.ph")}
                  autoComplete="off"
                  spellCheck={false}
                />
                <ClearableInput
                  type="password"
                  value={w.passphrase}
                  onChangeText={(passphrase) => setWiz({ ...w, passphrase })}
                  placeholder={t("srv.auth.pass.ph")}
                  autoComplete="off"
                />
              </div>
            ) : null}
            <button
              type="button"
              role="radio"
              aria-checked={w.auth === "default"}
              className="auth-card"
              onClick={() => setWiz({ ...w, auth: "default", password: "", probeError: null, formError: null })}
            >
              <span className="auth-radio" aria-hidden="true" />
              <span>
                <span className="auth-name">{t("srv.auth.agent")}</span>
                <span className="auth-sub">{t("srv.auth.agent.sub")}</span>
              </span>
            </button>
          </div>
          <button
            type="button"
            className="alt-toggle"
            onClick={() => {
              const next = !w.altOpen;
              setWiz({ ...w, altOpen: next });
              if (next && !w.pubkey) void bridge.serverPublicKey().then((pk) => setWiz((cur) => (cur ? { ...cur, pubkey: pk } : cur)));
            }}
          >
            {t("srv.auth.alt.toggle")}
          </button>
          {alt}
          {w.formError ? <div className="f-hint" data-err="">{w.formError}</div> : null}
          <div className="wiz-foot">
            <div className="left">
              <button type="button" className="btn-quiet" onClick={() => wizBack(1)}>
                {t("srv.back")}
              </button>
            </div>
            <div className="left">
              <button type="button" className="btn-accent" disabled={w.busy} onClick={step2Connect}>
                {w.busy ? t("srv.connecting") : t("srv.connect")}
              </button>
            </div>
          </div>
        </>
      );
    } else if (w.step === 3 && w.probe) {
      const bad = w.probe.mismatch;
      const fp = w.probe.fingerprint ?? "";
      body = (
        <>
          <div className="fp-box" data-bad={bad || undefined}>
            <div className="fp-title">{bad ? t("srv.fp.mismatch") : t("srv.fp.first")}</div>
            <div className="fp-val">{fp}</div>
            {w.probe.arch || w.probe.hostname ? (
              <>
                {w.probe.hostname ? (
                  <div className="fp-row">
                    <span className="k">{t("srv.fp.hostname")}</span>
                    <span className="v">{w.probe.hostname}</span>
                  </div>
                ) : null}
                {w.probe.arch ? (
                  <div className="fp-row">
                    <span className="k">{t("srv.fp.arch")}</span>
                    <span className="v">{w.probe.arch}</span>
                  </div>
                ) : null}
              </>
            ) : null}
            {bad ? (
              <>
                <div className="fp-row">
                  <span className="k">{t("srv.fp.old")}</span>
                  <span className="v">{w.probe.known_fingerprint ?? "—"}</span>
                </div>
                <div className="f-hint" data-err="">
                  {t("srv.fp.mismatch.hint")}
                </div>
                <button
                  type="button"
                  className="chk"
                  role="checkbox"
                  aria-checked={w.ack}
                  onClick={() => setWiz({ ...w, ack: !w.ack })}
                >
                  <i aria-hidden="true">{w.ack ? "✓" : ""}</i>
                  <span>{t("srv.trust.mismatch")}</span>
                </button>
              </>
            ) : (
              <div className="f-hint">{t("srv.fp.remember")}</div>
            )}
          </div>
          <div className="wiz-foot">
            <div className="left">
              <button type="button" className="btn-quiet" onClick={() => wizBack(2)}>
                {t("srv.back")}
              </button>
            </div>
            <div className="left">
              <button type="button" className="btn-quiet" onClick={close}>
                {t("srv.cancel")}
              </button>
              <button
                type="button"
                className="btn-accent"
                disabled={bad && !w.ack}
                onClick={() => void startInstall()}
              >
                {bad ? t("srv.trust.mismatch") : t("srv.trust")}
              </button>
            </div>
          </div>
        </>
      );
    } else if (w.step === 4) {
      body = (
        <>
          <div className="prog">
            {w.steps.map((s) => (
              <InstallRow key={s.key} step={s} days={w.days} name={w.name} />
            ))}
            {w.steps.length === 0 ? (
              <div className="prog-row" data-s="run">
                <span className="spin" />
                <span>{t("srv.installing")}</span>
              </div>
            ) : null}
          </div>
          {w.installError ? errBox(w.installError, [{ label: t("srv.install.retry"), onClick: () => wizBack(2) }]) : null}
          <div className="wiz-foot">
            <div className="left">
              {!w.busy && w.installError ? (
                <button type="button" className="btn-quiet" onClick={close}>
                  {t("srv.cancel")}
                </button>
              ) : null}
            </div>
            <div className="left">
              {w.busy ? <span className="f-hint inst-hint">{t("srv.installing")}</span> : null}
            </div>
          </div>
        </>
      );
    } else if (w.step === 5) {
      const out = w.outcome;
      const merge = out?.merge_pending
        ? t("srv.sum.merge.pending")
        : t("srv.sum.merge.v", { rows: count(out?.rows ?? 0), n: out?.detected.length ?? 0 });
      body = (
        <>
          <div className="sum-box">
            <div className="sum-title">
              <span className="ok-w">
                <IconCheck size={9} />
              </span>
              {t("srv.sum.title")}
            </div>
            <ul className="sum-list">
              <li>
                <span className="k">{t("srv.sum.name")}</span>
                <span className="v">{t("srv.sum.name.v", { name: w.name })}</span>
              </li>
              <li>
                <span className="k">{t("srv.sum.auth")}</span>
                <span className="v">{t("srv.sum.auth.v")}</span>
              </li>
              <li>
                <span className="k">{t("srv.sum.fp")}</span>
                <span className="v mono">{(w.probe?.fingerprint ?? "").slice(0, 30)}…</span>
              </li>
              <li>
                <span className="k">{t("srv.sum.collector")}</span>
                <span className="v">{t("srv.sum.collector.v", { v: w.version, arch: w.probe?.arch ?? "—" })}</span>
              </li>
              <li>
                <span className="k">{t("srv.sum.merge")}</span>
                <span className="v num">{merge}</span>
              </li>
            </ul>
          </div>
          <div className="f-row">
            <div className="f-label">
              <span>{t("srv.interval")}</span>
            </div>
            <div className="seg" role="radiogroup" aria-label={t("srv.interval")}>
              {INTERVALS.map((secs) => (
                <button
                  key={secs}
                  type="button"
                  role="radio"
                  className="seg-btn"
                  aria-checked={w.every === secs}
                  onClick={() => {
                    setWiz({ ...w, every: secs });
                    if (out?.server) void patchServer(out.server.id, { every_secs: secs });
                  }}
                >
                  {secs === 300 ? t("srv.int.5m") : secs === 900 ? t("srv.int.15m") : t("srv.int.1h")}
                </button>
              ))}
            </div>
          </div>
          <div className="f-row">
            <div className="f-label">
              <span>{t("srv.window")}</span>
            </div>
            <div className="seg" role="radiogroup" aria-label={t("srv.window")}>
              {WINDOWS.map((days) => (
                <button
                  key={days}
                  type="button"
                  role="radio"
                  className="seg-btn"
                  aria-checked={w.days === days}
                  onClick={() => {
                    setWiz({ ...w, days });
                    if (out?.server) void patchServer(out.server.id, { days });
                  }}
                >
                  {days === 7 ? t("srv.days.7") : days === 30 ? t("srv.days.30") : t("srv.days.90")}
                </button>
              ))}
            </div>
          </div>
          <div className="wiz-foot">
            <div className="left" />
            <div className="left">
              <button
                type="button"
                className="btn-accent"
                onClick={() => {
                  setWiz(null);
                  setView({ kind: "list" });
                }}
              >
                {t("srv.wiz.done")}
              </button>
            </div>
          </div>
        </>
      );
    }

    wizardBody = (
      <>
        <div className="sheet-hd">
          <h2 className="srv-hd">
            {titles[w.step]}
            <span className="step-chip">{t("srv.wiz.step", { n: w.step })}</span>
          </h2>
          <button
            type="button"
            className="sheet-close"
            aria-label={t("srv.cancel")}
            disabled={w.busy}
            onClick={close}
          >
            <IconClose size={12} />
          </button>
        </div>
        <div className="sheet-body">
          <div className="wiz-steps" aria-hidden="true">
            {[1, 2, 3, 4, 5].map((i) => (
              <i key={i} data-on={i <= w.step || undefined} />
            ))}
          </div>
          {body}
        </div>
      </>
    );
  }

  /* ── detail ─────────────────────────────────────────────────────────── */

  let detailBody = listBody;
  if (view.kind === "detail") {
    const s = servers.find((v) => v.id === view.id);
    if (s) {
      const rows = historyRows(s.history, now);
      detailBody = (
        <>
          <div className="sheet-hd">
            <h2 className="srv-hd">
              {s.name}
              <span className="step-chip mono">{`${s.user}@${s.host}:${s.port}`}</span>
            </h2>
            <button type="button" className="sheet-close" aria-label={t("srv.back.list")} onClick={() => setView({ kind: "list" })}>
              <IconBack size={12} />
            </button>
          </div>
          {noticeBar}
          <div className="sheet-body">
            {s.status === "error" && s.last_error
              ? errBox(s.last_error, [
                  { label: t("srv.sync.now"), onClick: () => void bridge.serverSyncNow(s.id) },
                  { label: copied ? t("srv.copy.done") : t("srv.copy"), onClick: () => void copyKey() },
                ])
              : null}
            <div className="d-row">
              <span className="k">{t("srv.d.status")}</span>
              <span className="v">
                {!s.enabled
                  ? t("srv.d.status.disabled")
                  : s.status === "ok" || s.status === "warn"
                    ? t("srv.d.status.ok", { ago: s.last_ok_ms === null ? "—" : relativeTime(s.last_ok_ms, now) })
                    : s.status === "syncing"
                      ? t("srv.d.status.syncing")
                      : s.status === "error"
                        ? t("srv.d.status.error")
                        : t("srv.d.status.none")}
              </span>
            </div>
            <div className="d-row">
              <span className="k">{t("srv.d.auth")}</span>
              <span className="v">{t("srv.d.auth.v")}</span>
            </div>
            <div className="d-row">
              <span className="k">{t("srv.d.fp")}</span>
              <span className="v mono">{s.fingerprint}</span>
            </div>
            <div className="d-row">
              <span className="k">{t("srv.d.tools")}</span>
              <span className="v">
                {s.tools.length ? (
                  <span className="tool-chips">
                    {s.tools.map((tool) => (
                      <span key={tool} className="tool-chip">
                        {tool}
                      </span>
                    ))}
                  </span>
                ) : (
                  t("srv.d.tools.none")
                )}
              </span>
            </div>
            <div className="d-row">
              <span className="k">{t("srv.d.interval")}</span>
              <span className="v">
                <span className="seg" role="radiogroup" aria-label={t("srv.d.interval")}>
                  {INTERVALS.map((secs) => (
                    <button
                      key={secs}
                      type="button"
                      role="radio"
                      className="seg-btn"
                      aria-checked={s.every_secs === secs}
                      onClick={() => void patchServer(s.id, { every_secs: secs })}
                    >
                      {secs === 300 ? t("srv.int.5m") : secs === 900 ? t("srv.int.15m") : t("srv.int.1h")}
                    </button>
                  ))}
                </span>
              </span>
            </div>
            <div className="d-row">
              <span className="k">{t("srv.d.window")}</span>
              <span className="v">
                <span className="seg" role="radiogroup" aria-label={t("srv.d.window")}>
                  {WINDOWS.map((days) => (
                    <button
                      key={days}
                      type="button"
                      role="radio"
                      className="seg-btn"
                      aria-checked={s.days === days}
                      onClick={() => void patchServer(s.id, { days })}
                    >
                      {days === 7 ? t("srv.days.7") : days === 30 ? t("srv.days.30") : t("srv.days.90")}
                    </button>
                  ))}
                </span>
              </span>
            </div>
            <div className="d-row">
              <span className="k">{t("srv.d.enabled")}</span>
              <span className="v">
                <button
                  type="button"
                  role="switch"
                  className="switch"
                  aria-checked={s.enabled}
                  aria-label={t("srv.d.enabled")}
                  onClick={() => void patchServer(s.id, { enabled: !s.enabled })}
                />
              </span>
            </div>
            <div className="sec-label d-hist-label">{t("srv.d.history")}</div>
            {rows.length === 0 ? (
              <div className="f-hint">{t("srv.d.history.empty")}</div>
            ) : (
              <ul className="hist">
                {rows.map(({ ev, delta, ago }) => (
                  <li key={ev.at_ms}>
                    <span>{ago}</span>
                    <span className="num">
                      {ev.ok
                        ? `${t("srv.d.hist", { rows: count(ev.rows), t: tookText(ev.took_ms) })}${
                            delta !== null ? `（${t("srv.d.hist.add", { n: count(delta) })}）` : ""
                          }`
                        : t("srv.d.hist.fail")}
                    </span>
                  </li>
                ))}
              </ul>
            )}
            <div className="wiz-foot">
              <div className="left">
                <button type="button" className="btn-quiet" onClick={() => setView({ kind: "list" })}>
                  {t("srv.back.list")}
                </button>
              </div>
              <div className="left">
                <button type="button" className="btn-quiet" onClick={() => void copyKey()}>
                  {copied ? t("srv.copy.done") : t("srv.copy")}
                </button>
                <button
                  type="button"
                  className="btn-quiet"
                  disabled={s.status === "syncing"}
                  onClick={() => void bridge.serverSyncNow(s.id)}
                >
                  {t("srv.sync.now")}
                </button>
                <button
                  type="button"
                  className="btn-danger"
                  onClick={() => {
                    setCleanup(false);
                    setView({ kind: "confirm", id: s.id });
                  }}
                >
                  {t("srv.remove")}
                </button>
              </div>
            </div>
          </div>
        </>
      );
    }
  }

  /* ── confirm removal ────────────────────────────────────────────────── */

  let confirmBody = listBody;
  if (view.kind === "confirm") {
    const s = servers.find((v) => v.id === view.id);
    if (s) {
      confirmBody = (
        <>
          <div className="sheet-hd">
            <h2 className="srv-hd">{t("srv.rm.title")}</h2>
            <button
              type="button"
              className="sheet-close"
              aria-label={t("srv.back")}
              onClick={() => setView({ kind: "detail", id: s.id })}
            >
              <IconBack size={12} />
            </button>
          </div>
          <div className="sheet-body">
            <div className="confirm-box">
              <div className="confirm-q">{t("srv.rm.q", { name: s.name })}</div>
              <div className="f-hint">{t("srv.rm.hint")}</div>
              <button
                type="button"
                className="chk"
                role="checkbox"
                aria-checked={cleanup}
                onClick={() => setCleanup((v) => !v)}
              >
                <i aria-hidden="true">{cleanup ? "✓" : ""}</i>
                <span>{t("srv.rm.cleanup")}</span>
              </button>
            </div>
            <div className="wiz-foot">
              <div className="left">
                <button type="button" className="btn-quiet" onClick={() => setView({ kind: "detail", id: s.id })}>
                  {t("srv.cancel")}
                </button>
              </div>
              <div className="left">
                <button type="button" className="btn-danger" onClick={() => void removeServer(s.id)}>
                  {t("srv.rm.confirm")}
                </button>
              </div>
            </div>
          </div>
        </>
      );
    }
  }

  return (
    <div className="sheet-backdrop" onClick={close}>
      <div
        className="sheet sheet-srv"
        role="dialog"
        aria-modal="true"
        aria-label={t("srv.title")}
        onClick={(e) => e.stopPropagation()}
      >
        {view.kind === "list" ? listBody : view.kind === "wizard" ? wizardBody : view.kind === "detail" ? detailBody : confirmBody}
      </div>
    </div>
  );
}

/** One install-progress line; the export label carries the request's own args. */
function InstallRow({ step, days, name }: { step: InstallStep; days: number; name: string }) {
  const label = t(STEP_KEY[step.key] ?? "srv.wiz.install", step.key === "export" ? { d: days, name } : undefined);
  return (
    <>
      <div className="prog-row" data-s={step.state === "active" ? "run" : step.state === "done" ? "ok" : step.state === "error" ? "err" : step.state === "warn" ? "warn" : "wait"}>
        <span className="st">
          {step.state === "active" ? (
            <span className="spin" />
          ) : step.state === "done" ? (
            <span className="ok-w">
              <IconCheck size={9} />
            </span>
          ) : step.state === "error" ? (
            <span className="err-w">✕</span>
          ) : step.state === "warn" ? (
            <IconWarn size={13} />
          ) : (
            <span className="wait-w" />
          )}
        </span>
        <span>{label}</span>
      </div>
      {step.detail && (step.state === "done" || step.state === "warn") ? (
        <div className="prog-row detail">{step.detail}</div>
      ) : null}
    </>
  );
}

/** Failure kinds that have a next move; the rest just show the box. */
function probeActions(w: Wiz, retry: () => void): { label: string; onClick: () => void }[] | undefined {
  const kind = w.probeError?.kind;
  if (kind === "unreachable" || kind === "dns" || kind === "timeout") {
    return [{ label: t("srv.retry"), onClick: retry }];
  }
  return undefined;
}

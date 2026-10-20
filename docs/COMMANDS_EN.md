# TokenMe CLI Reference

> Version: 0.1.8 · Updated: 2026-10-13 · Audience: CLI users · Source of truth: `crates/usage-cli/src/args.rs` (verified against `tokenme --help`) · [中文](COMMANDS.md)

All commands share these global options (place them anywhere on the line):

| Option | Effect |
| :--- | :--- |
| `--tool <ID>` | Restrict to specific sources; repeatable (`claude`, `codex`, `cline`, …) |
| `--json` | Machine-readable output on stdout |
| `--offline` | Never touch the network; use the cached/bundled price table |
| `--pricing-override <MODEL=IN/OUT/CC/CR>` | Force a price (repeatable), e.g. `glm-5.2=2/8/2.5/0.4` — USD per 1M tokens |
| `--since <YYYY-MM-DD>` / `--until <YYYY-MM-DD>` | Clip the window |
| `--db <PATH>` | Use a specific index database |
| `--no-ingest` | Report from the existing index without re-scanning |
| `-q` / `-v` | Quiet / verbose |

## detect

List every supported AI tool found on this machine, its log roots and index state.

```bash
tokenme detect
```

## daily / weekly / monthly

Per-period totals with session counts, token stages, cost and cache rate.

```bash
tokenme daily --days 30
tokenme weekly --weeks 8
tokenme monthly --months 6
```

| Flag | Default | Notes |
| :--- | :--- | :--- |
| `--days/--weeks/--months <N>` | 14 / 12 / 12 | Capped at 90 periods |

## report

One window broken down by tool, model, project, MCP server or skill, with the delta against the previous slice.

```bash
tokenme report --window week --group model
```

| Flag | Values |
| :--- | :--- |
| `--window` | `day` (default), `week`, `month` |
| `--group` | `tool` (default), `model`, `project`, `mcp`, `skill` |

## sessions

Recent sessions with per-session totals.

```bash
tokenme sessions --limit 20
```

## quota

Newest live quota sample each source reported: rolling 5-hour pools, weekly windows, credit plans, reset countdowns.

```bash
tokenme quota
```

Answers are cached under one shared 5-minute TTL (`crates/usage-quota/src/lib.rs::TTL`; one JSON file per tool under the OS config directory). This command waits up to 30 seconds for a live probe — the panel waits 5 — and a probe that overruns the wait still contributes its previous answer inside a 6-hour grace window. Pausing a probe whose host app has quit is the panel's `host_exit_pause` setting, not something the CLI does.

OpenCode quota uses only the official Go endpoint. The credential lookup order is `OPENCODE_API_KEY`; then OpenCode V2's own store, the `credential` table of `opencode.db` (`integration_id` `opencode-go`/`opencode`, preferring the `active=1` row; `access` for an OAuth login, `key` for a pasted one; an expired token is neither refreshed nor sent); then `provider.<id>.options.apiKey` in `opencode.json` — tried at `$XDG_CONFIG_HOME/opencode/opencode.json` (when set), `~/.config/opencode/opencode.json`, and on macOS also `~/Library/Application Support/opencode/opencode.json`, counting only provider blocks whose `baseURL` is `https://opencode.ai/zen/go/v1`; then the legacy `~/.local/share/opencode/auth.json`, read for exactly the two ids OpenCode writes a Go key under. Custom provider URLs are ignored for this quota. The result has rolling 5-hour, weekly and monthly windows.

Open Design quota reads `profiles.prod` from `~/.amr/config.json` (`OPEN_DESIGN_HOME` moves it): the `vela_ctrl_…` control key calls `/api/v1/workspaces/current` and then `/api/v1/workspaces/<id>/billing/preflight` for the Design Plan's 5-hour and 7-day windows. The `sk-amr-…` runtime key cannot read the control plane, and the wallet balance is not a window, so neither enters the result.

Most probes only read credentials. Exactly three write a rotated one back, each into the tool's own store, atomically: **MiniMax Code** (exchanges the refresh token in its `auth.json` when the stored access token is within 2 minutes of expiry, under the vendor's own lock), **Kimi Code** (exchanges it only after `/usages` declines the stored token), and **Cline** (rotates its gateway token ahead of expiry). Every other probe writes nothing — the WorkBuddy credential on disk comes from `tokenme workbuddy-login`, a command you ran, not from a probe.

## dsh-doctor

DSH accuracy audit: the desktop app's own projection ledger (`tokenUsage`) printed next to what the index holds, per session, with a `status` of `ok`, `MISMATCH`, `v3 stream`, `not indexed` or `indexed but no projection`. Notes carry the structural reasons for honest drift — inherited pre-v4 events, subagent spend that lives outside `tokenUsage`. No flags.

```bash
tokenme dsh-doctor
```

## codex-doctor

Codex accuracy audit: every rollout re-parsed from scratch (cursor 0, no manifest, no dedupe) and aggregated per session, next to the indexed totals. Codex keeps no vendor-side ledger, so the replay is the independent truth — a `MISMATCH` row means the indexer dropped a call or counted it twice. No flags.

```bash
tokenme codex-doctor
```

## workbuddy-login

Log in to domestic WorkBuddy for the quota probe: asks the vendor for a state, opens Tencent SSO in your browser, then polls up to 5 minutes for the token. What it saves lands in `workbuddy-auth.json` under the OS config directory (the probe reads it, and never refreshes it); nothing is stored when the window elapses. No flags. The saved credential's domain selects its edition; a domestic login never feeds the international WorkBuddy AI row. Installed desktop clients normally need no extra login.

```bash
tokenme workbuddy-login
```

## pricing

Price provenance: models.dev sells most models through several providers, so a cost number is only meaningful together with the listing that won.

```bash
tokenme pricing explain deepseek-v4-flash   # which listing priced it, and the losers
tokenme pricing contested --limit 25        # multi-provider models and the money at stake
```

## budget

Spend caps measured against tokenme's own cost, for tools whose vendor publishes no limit.

```bash
tokenme budget list
tokenme budget set zcode --daily 5 --monthly 50
tokenme budget rm zcode
```

Units are USD. `0` clears just that window.

## index

Drive the index explicitly.

```bash
tokenme index --status              # show state without ingesting
tokenme index --rebuild             # drop and re-read the retention window
tokenme index --prune               # drop events older than retention
tokenme index --force               # steal the ingest lease from a dead process
```

Every command verifies the index image when it opens it (`PRAGMA quick_check`). A damaged image is not an error you have to act on: it is moved aside as `index.db.corrupt-<timestamp>` (with its `-wal` / `-shm`, so the rebuild cannot inherit them), a fresh empty index takes its place, a `warning:` line says so on stderr, and the next ingest refills everything from the tools' own logs. A file that cannot be opened at all is quarantined the same way, silently, before the command starts.

## icons

The macOS application icon each tool row would show, as data URLs (what the panel requests over IPC). Written to `icons.json` for review.

## export

Export a window of this machine's events as a sync bundle (gzipped JSONL + a manifest carrying the sha256 and per-tool sums) for another machine to import. Lands in `~/tokenme-sync` by default — the directory the menu-bar engine merges automatically — so a Linux collector's whole writer is one timer line. Recent events are ingested first (same as every reporting command; `--no-ingest` skips that). A quiet or logless machine still gets a valid bundle with `rows 0` — the collector's timer never fails on a slow day.

```bash
tokenme export                          # last 30 days → ~/tokenme-sync/tokenme-<host>.jsonl.gz
tokenme export --days 400               # first backfill: the whole retention window
tokenme export --out /tmp/share         # somewhere else (a Syncthing folder, a USB stick)
tokenme export --origin workstation-01  # pin the identity (default: hostname)
```

The `.gz` is written via `*.tmp` + rename and the manifest last, so a transport never sees a half-written bundle. Re-exporting the same window is safe: imports are idempotent. Both files land `0600` on unix — a bundle names sessions, projects and local log paths, so it is private by default.

## import

Merge one bundle exported by another machine into the local index. Validates the sha256 and every batch's totals against the manifest first; any failure rolls the whole merge back and the previous numbers stay. Re-importing the same file changes nothing.

```bash
tokenme import ~/tokenme-sync/tokenme-build-01.jsonl.gz
tokenme import bundle.jsonl.gz --dry-run   # parse + reconcile, then roll back
```

On the menu-bar side you rarely need this: the panel engine auto-imports `~/tokenme-sync` every pass and shows a **多端同步 / Multi-device sync** badge with the newest merge per machine (warning-hued past 24 h).

## sync run / sync stop / sync reset / sync status

`sync pair --invite '<complete tokenme://device-pair invite>'` is the preferred secondary entry point. After certificate-pin verification it sends the pairing request and stores the endpoint, server name, certificate, pin, primary binding and local identity path in the user data directory; ordinary users do not enter a stream or generation. After approval, `sync run` keeps the single secondary runtime in the foreground: each pass ingests local events, builds a stable snapshot for the local origin, allocates generation and payload revision and writes the journal and immutable outbox in one SQLite transaction, then uploads and ACKs over pinned TLS 1.3.

```bash
tokenme sync run       # user-level Ubuntu/Linux resident entry; supervise with systemd --user if desired
tokenme sync status    # devices, stream/epoch, runtime, pending bytes, last connection and error
tokenme sync stop      # request a stop through the private user Unix socket; keep config and outbox
tokenme sync reset     # after stopping, drop stream/outbox/ACK checkpoints, recover a new stream and re-sync from scratch
```

`sync reset` keeps local usage history and the pairing. It drops only state bound to the old stream: queued packages, the journal, generation/ACK checkpoints and the primary-download cursor. The next `sync run` uses the recovery handshake to freeze that stream, adopts a fresh one, uploads the full local snapshot again and re-reads the primary's history from the first identity. A running runtime is refused; stop it first so the reset cannot delete rows the runtime still holds in memory.

`sync run` and `sync serve` are self-supervising: a failed runtime, database handle or listener is rebuilt with 1, 2, 4, … up to 60 seconds of backoff, and a healthy 60-second run resets that budget. Revocation, unpairing or an explicit disabled state is terminal, not a restart loop. `sync stop` uses `device-sync/secondary-runtime.sock` under the user data directory, with a `0700` directory and `0600` socket; it is not a loopback-TCP control plane. An abnormal runtime exit cleans up the socket and keeps pending packages; the next explicit `sync run` or Panel recovery can continue. The macOS/Unix path is tested here; Windows named-pipe ACL behavior remains unverified on this machine.

The Panel device-sync sheet exposes the matching primary start/stop, private certificate and invite generation, QR-file save, secondary QR-file read or invite/pair, approve/reject, pause/resume/revoke/unpair, and secondary runtime start/stop actions. After stopping primary, the device can read a new invite and switch directly to secondary; after stopping secondary, it can configure a listener and switch directly to primary. A switch isolates the old trust, stream, receipt and pending-package state while keeping local usage history. A cross-device listener also requires an advertised address; the panel refuses loopback or unspecified addresses in an invite. Primary startup saves identity and certificate in the user-private directory; after a Panel restart the secondary runtime follows the persisted `enabled` state, while an explicit stop persists disabled. `sync run/stop/status` is the same runtime entry for Ubuntu and other headless machines.
With Allow LAN enabled, the primary also publishes an `_tokenme-sync._tcp.local.` mDNS candidate. Scanning only shows the endpoint, device ID and certificate pin; it never broadcasts an invite secret. “Pair this device” first requests a five-minute one-time invite over pin-verified TLS, then sends the signed request and waits for primary approval. Discovery or publication errors mention local-network permission, UDP 5353 and the listener port, while QR/full-invite fallback remains available. Stopping the primary unregisters the advertisement, and restart restores only a role persisted as enabled.

## diagnostics-stdio

```bash
tokenme --db ~/.local/share/tokenme/index.db diagnostics-stdio
```

Starts the local diagnostics MCP newline-delimited JSON-RPC stdio host. It only reads the existing device-sync identity from `--db`; it does not ingest logs or open a listener. stdin/stdout are reserved for protocol responses and operational errors go to stderr. Each start creates a process-scoped diagnostics authority, so grants issued by an older process are not accepted after restart. The command fails with an actionable pairing/start hint when the database has no initialized device-sync state.

Use a Panel-exported diagnostic grant as a real remote bridge configuration:

```bash
tokenme --db ~/.local/share/tokenme/index.db \
  --diagnostics-config ~/Downloads/tokenme-diagnostics.json \
  diagnostics-stdio
```

This mode loads the debugger private key from the paired secondary identity in `--db`, signs each `initialize`, `tools/list`, `tools/call`, and notification request, and forwards it to the primary's `/diagnostics/mcp` over HTTPS/TLS 1.3 with the DER certificate and SHA-256 pin. `server_name` is optional; older Panel configurations without it derive the name from the HTTPS endpoint host. Without a config file the command remains a local fail-closed host and never connects to a remote endpoint automatically.

## sync upload

Upload local events to an already-approved primary over strict HTTPS/TLS 1.3. This command does not fake pairing, does not clear local data, and does not change the existing `export/import` file path. The primary must issue `primary_id`, `stream_id` and `data_epoch`; provide its certificate PEM and exact 64-hex SHA-256 pin:

```bash
tokenme sync upload --endpoint https://192.168.1.20:47821 \
  --server-name tokenme-primary \
  --trust-cert ~/.config/tokenme/primary-cert.pem \
  --pin-sha256 <64-hex-cert-sha256> \
  --identity ~/.config/tokenme/device-identity.json --origin laptop \
  --primary-id <primary-id> --stream-id <stream-id> \
  --data-epoch 1 --generation 1
```

The primary applies receipts idempotently by device, stream and generation. On disconnect or `409 RECONCILE_REQUIRED`, pending local data stays intact and must be reconciled before retrying. `sync upload` remains a low-level single-package/compatibility entry point and still requires the caller to supply stream, epoch and generation; ordinary users should use a complete invite followed by `sync run` instead. The legacy `export/import` file path, SSH importer and the new HTTPS runtime remain separate entry points.

Limits and trust: payload rows are capped (the manifest's count plus slack) so a corrupt file can't exhaust memory; a bundle can only move a row forward — the growth predicate refuses lower counts — and it never deletes (rows the collector purged later are reported as stale). A bundle is validated, not signed: anyone who can write into the sync folder can add rows, so treat it like the index itself. Task guide: [USER_GUIDE.md](USER_GUIDE.md) §4.

## Storage locations

| Data | macOS | Linux | Windows |
| :--- | :--- | :--- | :--- |
| SQLite index | `~/Library/Application Support/tokenme/index.db` | `~/.local/share/tokenme/index.db` | `%LOCALAPPDATA%\tokenme\index.db` |
| Settings | `~/Library/Application Support/tokenme/settings.json` | `~/.config/tokenme/settings.json` | `%APPDATA%\tokenme\settings.json` |
| Quota cache | `<config dir>/tokenme/quota/*.json` | same pattern | same pattern |

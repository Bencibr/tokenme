# TokenMe User Guide

> Version: 0.1.8 · Updated: 2026-10-13 · Audience: daily users · Covers both the CLI and the menu-bar panel · [中文版](USER_GUIDE.md)

## Feature checklist

| What it does | Where |
| :--- | :--- |
| Tokens, cost and cache rate for today / this week / this month / this year | Panel → Overview (swipe right for last week, last month, last year; the **Activity** tab is the year-round heatmap) |
| Per-tool usage for every AI coding tool | Panel → Tools (27 tools, one per row, each row tinted from the app's icon) |
| Find the most expensive sessions | Panel → Rankings |
| Single requests, filtered by model or project | Panel → Details |
| Measure TTFT (time to first token) and TPS | Panel → Speed (passive measurement; Codex / Claude Code can also fire one short live probe) |
| Each subscription's remaining quota and reset countdown | The quota strip below Overview (a system notification fires when a window crosses 80% or is exhausted) |
| One-click daily check-in | Panel → Tools, in the row header (Trae CN, Qoder, Qoder CN, MiniMax Code, WorkBuddy CN, FunIDE) |
| Fold in usage from servers and containers | One-click deploy from the panel footer's server button, or install the Linux collector by hand (see §4); the strict HTTPS device path is available in the device-sync sheet or through `tokenme sync run` (see §4.1) |
| Use the strict device-sync path | Panel primary/secondary management or `tokenme sync run/stop/status` over pinned HTTPS/TCP/TLS 1.3 (see §4.1) |
| Cost caps for tools without a vendor-published limit | `tokenme budget set` (see §5) |
| Digits or an icon living in the menu bar | Six tray display modes to choose from (see §3) |
| Data never goes to a third party | Read-only indexing by default; multi-device sync sends only to your approved primary, with no telemetry or third-party proxy (see §7) |

## 1. Install

macOS (Homebrew, recommended):

```bash
brew tap Bencibr/tokenme https://github.com/Bencibr/homebrew-tokenme
brew trust bencibr/tokenme        # once — brew 6 asks you to trust third-party casks
brew install --cask tokenme       # the menu-bar panel
brew install tokenme-cli          # the CLI
```

Windows: download `TokenMe_<v>_x64-setup.exe` from [Releases](https://github.com/Bencibr/tokenme/releases/latest). The installer installs both the panel and the CLI; the CLI lives at `<install directory>\bin\tokenme.exe` and is added to PATH automatically ("Only me" adds your user PATH, "All users" the system PATH). Open a new terminal after installing and run `tokenme --help`. Upgrades keep the same PATH entry; uninstall removes only the entry this installation added.

Linux (headless collector): download `tokenme-cli-<v>-<arch>-linux-musl.tar.gz` (fully static — no Rust or Python needed on that machine):

```bash
tar xzf tokenme-cli-<v>-x86_64-linux-musl.tar.gz && cd tokenme-cli-<v>-x86_64-linux-musl
./install-linux.sh --every 15 --push you@display-host
```

That one line puts the binary in `~/.local/bin` and installs a timer that exports every 15 minutes and scp's each fresh bundle to the display machine's `~/tokenme-sync`; the panel there merges it on its next pass, shown in the **Multi-device sync** badge. The timer is a systemd `--user` unit, degrading to an equivalent cron job on machines without a reachable user manager (containers, bare ssh) — it runs without a login session once `loginctl enable-linger` is on (on a headless machine the installer prints that one line for you). `--uninstall` removes the binary, the generated scripts and the timer; your data (index, bundles) stays. Without `--push`, move the `~/tokenme-sync` folder however you like (Syncthing, a mount, a nightly rsync) — merging bundles is idempotent.

The macOS panel can also be built from source (ad-hoc signed; right-click → Open on first launch):

```bash
./scripts/build-macos.sh          # produces TokenMe.app and a drag-install DMG
```

## 2. First run

```bash
tokenme detect        # 1. which of your tools were found, and where their logs live
tokenme daily --days 7
tokenme quota
```

## 3. The menu-bar panel (macOS · Windows)

Built by `scripts/build-macos.sh` on macOS and by `pnpm tauri build --bundles nsis` in `apps/tokenme-bar` on Windows. A left-click on the dual-ring icon toggles the panel; a right-click opens the menu (refresh, data dir, autostart, quit).

### 3.1 The five pages

- **Overview** — today's tokens, cost and cache hit rate, with usage charted by hour; the **Activity** tab is a GitHub-style year heatmap, and swiping right walks back through last week, last month and last year. The vendors' quota bars and notifications sit below.
- **Speed** — passive TTFT (time to first token) and TPS measurement: Codex is read from its local rollout stream, Claude Code comes through OpenTelemetry spans, DSH from local session logs; Codex and Claude Code can also fire one short live request to measure for real.
- **Tools** — per-tool requests, sessions, tokens and cache rate, one row each, the row colour taken from the app's icon; tools that support check-in claim right from the row header.
- **Rankings** — sessions ranked, so the expensive ones stand out at a glance.
- **Details** — request-level records, filterable by model and project.

### 3.2 Tray display modes

Each click on the display chip in the panel's bottom bar moves it one step:

| Mode | Menu bar shows |
| :--- | :--- |
| Tray only | icon only |
| Tokens | token total, no icon |
| Cost | cost, no icon |
| Tray·Tokens | icon + tokens (default) |
| Tray·Cost | icon + cost |
| Tray·Tokens·Cost | icon + tokens + cost |

### 3.3 Settings

The gear in the bottom bar opens the settings sheet (fixed at 65% of the panel's height, scrolling inside, with version · Check for updates · Quit pinned at the bottom). Four tabs: **General** (auto-refresh cadence, Appearance, Cost in USD, Show zero-session tools), **Alerts** (System notifications, Quota alert tiers, Plan expiry reminder, Muted tools), **Advanced** (Check quotas automatically, Quota checks by tool, Launch at login, Auto update check, servers & sync) and **About**. `›` opens a subpage inside the same sheet, `↗` jumps to another surface, `‹ Back` comes back. With a master switch off, every switch on its subpage greys out and says why — no fake toggles that look live.

### 3.4 Refresh & probe switches

Refresh cadence controls index re-scanning; vendor quota answers are cached for 5 minutes, keeping probes polite. **Polling is yours to control**: Check quotas automatically is the master gate, Quota checks by tool the per-vendor switches. A switched-off probe sends no request, but its cache is still read — the row keeps its last answer instead of vanishing; with the master gate off, the quota section says outright **Polling stopped · data as of N minutes ago** rather than pretending the numbers are fresh. The per-tool list defaults to the probes this machine has actually answered (Show all reaches the ones it never has); Copilot, Gemini CLI, Kimi Code and MiniMax Code have no menu-bar process to key off, so this list is the only way to stop them, and their rows are tagged **Not tied to app state**. Neither switch forces a refresh — a forced one would throw away the answer you just chose to keep. Turn one back on and the next scheduled pass within 60 s resumes it — or hit refresh to bring it back at once. The bottom bar's refresh button is a full refresh: it re-scans logs, forces a fresh probe of every vendor (bypassing the cache) and pulls a new price table. The **Last refresh · N s ago** chip in the bottom bar's left corner says how fresh the figures on screen are.

### 3.5 OpenCode official quota

TokenMe reads only the official OpenCode Go provider's quota: `OPENCODE_API_KEY` first, then the `provider.<id>.options.apiKey` of any provider in `opencode.json` (tried in order: `$XDG_CONFIG_HOME/opencode/opencode.json`, `~/.config/opencode/opencode.json`, and on macOS also `~/Library/Application Support/opencode/opencode.json`; that provider's `options.baseURL` must be `https://opencode.ai/zen/go/v1`), falling back to `~/.local/share/opencode/auth.json`. A custom proxy address is never taken for official quota credentials. The OpenCode row shows three windows — 5 hours, 7 days and monthly. If the row never appears, check, in order: Advanced → Check quotas automatically, the OpenCode switch under Quota checks by tool, and the quota poll lines in `panel.log`.

### 3.6 Automatic token renewal

Three tools keep a short-lived login on disk — MiniMax Code (about an hour), Kimi Code (the vendor's 15-minute schedule) and Cline (the gateway token, one hour) — and tokenme renews each in place as it expires: the refresh token in the tool's own credential file is exchanged for a fresh pair, written back atomically in that tool's own format. The write-back is what makes the renewal safe — a rotation that never lands on disk is a rotation already spent, and the tool's own next refresh would find its login dead. Each probe renews only the login it is actually reading; a static API key is read, never written. A failed exchange changes nothing, and the row keeps its last real number for up to six hours (the probe cache's grace). Every other probe reads credentials and never writes them — the WorkBuddy credential tokenme does store comes from `tokenme workbuddy-login`, a command you ran, not from a probe.

### 3.7 The two WorkBuddy editions

Domestic **WorkBuddy** (`workbuddy_cn`) reads `~/.workbuddy`; international **WorkBuddy AI** (`workbuddy`) reads `~/.workbuddy-ai`. Their usage, quota caches and host-exit gates are separate. Domestic polling checks the day's activity and claims any eligible, unclaimed day; with Advanced → Auto check-in enabled, claims continue with the WorkBuddy client closed for as long as TokenMe runs. The master gate and the per-tool switch still stop requests. Confirmed days are remembered per account and expire at Beijing midnight.

FunIDE uses the login state written by its official client. Polling reads `/billing/balance`, calls `/billing/checkin` only when the activity is open and today is unclaimed, then reads the balance again to confirm the result. Turning off Advanced → Auto check-in stops automatic claims; the row-header manual action remains available.

### 3.8 Quota banners

The moment a window crosses **80%** used, a system notification fires (`Qoder · 5 hours` · 83% used · 17% left), and a second one when it is used up (100%). Only windows the vendors actually meter get banners; the §5 budgets you set yourself stay panel-only. **Alert tiers** choose 80% and 100% / Only 100% / Off. **Plan expiry reminder** chooses 5, 3, or 1 day before expiry, or Off; it defaults to 3 days and fires once when an existing package row enters that range. **Muted tools** silences chosen windows — while muted the tier is still recorded, so un-muting never replays a banner you missed. A banner marks the crossing, not the state: each line fires once per window per instance, the fact persists (`notify-state.json` beside the index), and a reset window re-arms silently. Nothing is probed while the panel is quit, so a crossing that happened in the gap is not replayed. The first banner that is actually due raises the system's notification permission prompt once; if nothing arrives after that, the toggle lives in the OS settings (macOS System Settings → Notifications → TokenMe). On macOS the quota section offers its own guide line: **Enable** / **Open Settings** / Dismiss. A bare `cargo run` has no app identity and never posts a banner (the log says so).

### 3.9 How fresh the data is

The bottom bar's left corner always shows **Last refresh · N s ago**; after five consecutive cadences without a publish it becomes **Updates stopped**, with the time of the last pass — the engine restarts its loop by itself (with backoff; the reason is in `panel.log`). While the panel has just launched and is showing the report restored from the previous run, the same corner reads **From the last scan** plus the data's actual age — the engine is alive and scanning; this is not "stopped".

### 3.10 The multi-device sync badge

Once a Linux machine syncs in, a **Multi-device sync** badge appears under the figures: the newest merge's machine, time and row count, turning warning-hued past 24 hours without a merge. Hover it for every machine's windows and file names. No bundle has ever been imported → no badge.

### 3.11 Switching machines

With more than one machine reporting, the header's dropdown folds the whole panel — every period, the model and project rankings, the call series, the session list — under **All machines**, **This machine**, or any single remote by its own name. It re-summarises the same index rather than re-scanning anything, so it is instant: all = the sum of the machines. The trigger line shows the current scope: All machines carries up to three machine colour chips and the remote share of the figures (Remote 42%); a single machine carries its own chip, its share and its latest merge time. An overdue machine raises a warning mark on the All machines line. With nothing imported the control is not rendered at all — a single-machine panel looks exactly as it did before.

Registered strict-sync devices also appear in this dropdown: a device with no data is shown at 0, and paused, revoked or reconcile-required states are labelled. The device-sync sheet can manage primary/secondary roles, start and stop the primary service, create and save a QR invite file, read that file on a secondary to pair, approve/reject, pause/resume, revoke and unpair. After stopping the primary, the device can read a new invite and switch directly to secondary; after stopping secondary sync, it can configure a listener and switch directly to primary. A role switch isolates the old trust, stream, receipt and pending-package state while keeping local usage history. A cross-device listener must provide an advertised address; 127.0.0.1 and 0.0.0.0 are never placed in a usable invite. A secondary also shows its endpoint, stream/epoch, pending bytes, last connection and actionable errors. Status and mutations share one private Tauri IPC command; no loopback-TCP control plane is opened.

### 3.12 The first screen after launch

The panel can show numbers about a second after it can paint: the first frame is the previous run's own published report (`report.json` beside the index, read back in 3–22 ms — no scan, no probe, no query). It is labelled for what it is — the **From the last scan** chip in the left corner carries the data's actual age, and the quota strip shows **Probing quotas…** instead of the previous run's windows. A cached report is used only when it can be described honestly (same machine scope, same local day, under an hour old, not a write from a restore); a fresh install or an overnight gap has nothing to restore, and the wait has two layers: a spinner plus the line **Indexing local usage…** on the window's native backing (its colour follows Appearance), then `index.html`'s own loading card as the page takes over. `panel.log` keeps the milestones apart: `the page booted N ms after launch` is the bundle running, `the page reported content N ms after launch` is figures on screen, and `boot: restored …` says whether this launch had a report to restore.

## 4. Cross-machine sync (Linux collector → macOS/Windows panel)

Numbers from any machine can meet in one panel. A **collector** — the fully static Linux CLI — exports a window of its index into a folder; every **display machine** (macOS or Windows panel) merges that folder automatically. The transport is any file copy you already trust: tokenme never opens a port and has no accounts of its own.

Collector side, the minimal path (one line — same as §1):

```bash
./install-linux.sh --every 15 --push you@display-host
```

The timer (systemd `--user`, cron fallback) exports `~/tokenme-sync/tokenme-<host>.jsonl.gz` every 15 minutes and scp's each fresh bundle to the display machine's `~/tokenme-sync`. No push target? Drop `--push` and move that folder your own way (Syncthing, a mount, a nightly rsync) — or skip the installer entirely and run `tokenme export` by hand (defaults: last 30 days into `~/tokenme-sync`). A quiet or logless machine still produces a valid bundle (`rows 0`) — the timer never fails on a slow day.

Display side needs no setup: every panel pass merges any `tokenme-*.jsonl.gz` it finds in `~/tokenme-sync`, and the badge (§3.10) shows the newest merge per machine. Merges are idempotent — re-importing the same file changes nothing. To rehearse or go manual: `tokenme import <bundle>` (add `--dry-run` to parse and reconcile, then roll back).

**The panel can do the SSH half for you.** Click the server icon in the panel footer and add a machine: address, user, and the SSH password once (a private-key file works too, and both accept `~`). The wizard pins the host's SSH fingerprint on first connect, generates a dedicated key pair, plants its public half in the remote `~/.ssh/authorized_keys`, uploads the static collector to `~/.tokenme/bin/`, runs the first export, and pulls + merges the bundle — the whole install is one guided flow with a per-step log. After that the panel re-pulls every 15 minutes (configurable per server), and each fresh merge shows up like any file-based source, per machine. The password is used for that single setup connection and is never written anywhere. Removing a server cleans up both ends: the collector binary and the key line are deleted on the remote machine, and the pinned fingerprint and history are dropped locally.

**Security & limits**

- No listener, no cloud, no sync account: authentication is entirely the channel you pick (ssh keys for scp, Syncthing TLS + device IDs). tokenme stores no sync credentials. The in-panel SSH flow keeps that property: one password authenticates one setup connection, the host key is pinned on first sight (shown in the panel), and steady-state trust is a single dedicated key line in the remote `authorized_keys` — revocable by deleting that line or removing the server in the panel.
- Every import is validated before a single row lands: the bundle's sha256 against its manifest, the batch's token sums against the manifest, and a format + schema-version gate. The merge runs in one transaction — any failure rolls back whole, the file is kept on disk, and nothing merges.
- Row counts are capped (the manifest's count plus slack), so a corrupt file can't exhaust memory. A bundle can only move a row forward — the growth predicate refuses lower counts — and imports never delete: rows a collector purged later are reported as stale, not removed.
- On unix, bundles and manifests are written `0600`. A bundle carries what the index already carries — token counts, model/session/project names, local log paths — never code, prompts or credentials. Treat `~/tokenme-sync` like the index itself: whoever can write into that folder (your account, or anyone you share it with) can inject usage rows — merges are duplicate-safe, not signed.

### 4.1 Strict HTTPS multi-device channel (CLI/service integration)

This is a strict channel alongside the legacy file-bundle path above: the secondary device pushes over **HTTPS over TCP + TLS 1.3** to the primary; the primary never pulls. Pairing uses a one-time high-entropy invite, a SHA-256 certificate pin and an Ed25519 device identity. The invite secret is not sent until the pin has passed. Uploads sign the device, stream, epoch, generation, raw manifest and gzip hash; the primary applies them idempotently by receipt, while the caller keeps pending data through disconnects.

The protocol library, TLS transport, shared SQLite outbox/journal/FIFO scheduler, authenticated status/recovery, CLI `sync run/stop/status` and the lower-level `invite/approve/open-stream/serve/upload` paths are delivered, along with the Panel's primary/secondary pairing and device-management sheet. After a complete invite is paired, the secondary persists the endpoint, certificate pin, primary binding and identity; the runtime obtains the stream/epoch, snapshots the local origin, writes journal/outbox state, uploads over TLS and ACKs it. Ordinary users do not enter a stream or generation. The legacy `export/import` file path and SSH importer remain separate. Windows has a `cfg(windows)` current-SID private-DACL implementation, but this machine cannot compile or run that branch, so it remains unverified.

The Panel primary can start/stop the listener and generate a user-private certificate and invite; a secondary can click “Pair this device” for a discovered LAN primary, paste a complete invite, or scan a QR code. Discovery pairing first requests a five-minute, one-time invite over a certificate-pinned TLS channel, then follows the existing signed request and primary approval flow. The primary approves/rejects, pauses/resumes or revokes. On Ubuntu/headless machines, use the same saved configuration with `tokenme sync run`; `sync stop` uses a private user Unix socket and `sync status` shows pending data, last connection and errors:

When the primary enables “Allow LAN”, it publishes an `_tokenme-sync._tcp.local.` mDNS/Bonjour candidate. The pairing sheet scans for 1.5 seconds and shows the address, device ID and certificate pin. “Pair this device” never broadcasts an invite secret: it requests a temporary invite over a pin-verified TLS channel, then uses the existing signed request and primary approval. If discovery fails or permissions are denied, the UI explains the local-network permission, UDP 5353 and listener requirements and keeps QR/full-invite fallback. Stopping the primary unregisters the advertisement; a restart restores only a role that was explicitly enabled.

```bash
tokenme sync upload \
  --endpoint https://192.168.1.20:47821 \
  --server-name tokenme-primary \
  --trust-cert ~/.config/tokenme/primary-cert.pem \
  --pin-sha256 <64-hex-cert-sha256> \
  --identity ~/.config/tokenme/device-identity.json \
  --origin laptop \
  --primary-id <primary-id> --stream-id <stream-id> \
  --data-epoch 1 --generation 1
```

Low-level primary service example (the Panel can generate the certificate and start the service automatically):

```bash
tokenme sync serve --primary-id <primary-id> \
  --bind 0.0.0.0:47821 --cert primary-cert.pem --key primary-key.pem --allow-lan
```

Low-level CLI invite submission (ordinary users should prefer the complete invite; the secret comes from stdin so it is not exposed in the process list):

```bash
printf '%s\n' '<invite-secret-hex>' | tokenme sync pair \
  --endpoint https://192.168.1.20:47821 --server-name tokenme-primary \
  --trust-cert primary-cert.pem --pin-sha256 <64-hex-cert-sha256> \
  --invite-id <invite-id> --invite-secret-hex - \
  --identity ~/.config/tokenme/device-identity.json --origin laptop --os linux
```

The suggested default port is `47821`; a server must explicitly opt in before binding a non-loopback LAN/VPN address. The existing `export/import` file path keeps its old semantics and is not selected automatically by enabling this channel. UDP, QUIC and SSH are not transports for this path. A successful upload never deletes the local index; `409 RECONCILE_REQUIRED` requires reconciliation and must not trigger automatic outbox cleanup.

Verification boundary: `cargo test -p usage-device-sync` covers loopback TLS1.3, an independent primary/secondary SQLite upload and ACK over real TLS, a LAN-opt-in bind round trip on the local host, bad pins/bad connections/early rejection, outbox FIFO/lease, repeated snapshots and late historical updates; it is not proof of cross-host LAN/VPN or macOS/Windows/Ubuntu interoperability. Server concurrency, client/server bodies, responses, timeouts and staging quota are bounded. Panel Rust/TypeScript checks run on this machine; Windows named-pipe ACLs, a native Windows build and three-device testing remain UNVERIFIED.

## 5. Budgets

For tools whose vendor publishes no limit (Cline, ZCode, …), set your own caps — tokenme measures them against its own cost math and raises a quota bar:

```bash
tokenme budget set zcode --daily 5 --monthly 50
```

## 6. Supported tools & data sources

| Tool | Metric | Local data source |
| :--- | :--- | :--- |
| Claude Code | Tokens | `~/.claude/projects/**/*.jsonl` |
| Codex | Tokens | `~/.codex/sessions/**/rollout-*.jsonl` |
| OpenCode | Tokens | `~/.local/share/opencode/opencode.db` |
| Pi | Tokens | `~/.pi/agent/sessions/**` |
| Oh My Pi | Tokens | `~/.omp/agent/sessions/**` (`OMP_AGENT_DIR` / `OMP_CODING_AGENT_DIR` to move it) |
| Cline | Tokens | `~/.cline/data/sessions/**` (the desktop app writes a sub-agent's transcript inside its parent session dir and its meta into a sibling `<session>__agent_<uuid>/`; each is attributed to the session's own workspace) |
| ZCode | Tokens | `~/.zcode/cli/db/db.sqlite` |
| Antigravity CLI | Tokens | `~/.gemini/antigravity-cli/conversations/*.db` |
| Qoder CN | Credits + Tokens | `~/.qoder-cn/projects/**/*.jsonl` (credits) · `~/Library/Application Support/QoderCN/SharedClientCache/cache/db/local.db` (tokens); separate account, quota and daily check-in |
| Qoder | Credits + Tokens | `~/.qoder/projects/**/*.jsonl` (credits) · `~/Library/Application Support/Qoder/SharedClientCache/cache/db/local.db` (tokens) |
| WorkBuddy AI | Tokens + credits | `~/.workbuddy-ai/projects/**/<session>.jsonl` |
| WorkBuddy (China) | Tokens + credits | `~/.workbuddy/projects/**/<session>.jsonl` |
| AgnesCode / AtomCode / Crow5 / Mimocode / Cola / DSH | Tokens | auto-discovered, same shape as their parent tools |
| Kimi Code | Tokens | `~/.kimi-code/sessions/**/wire.jsonl` (legacy `~/.kimi`; the desktop app's embedded runtime home under `~/Library/Application Support/kimi-desktop/daimon-share/daimon/runtime/kimi-code/home` is read too) |
| MiniMax Code | Tokens | `~/.minimax/v2/sessions/**/messages.jsonl` (legacy `~/.mavis`) |

Cloud-only tools without local per-request logs (Cursor, VS Code Copilot) are not supported locally.

## 7. Privacy

- Only per-request token counts, model names and project paths are indexed — never your code or prompts.
- Scanning is read-only end to end: probes read credentials, and the only ones that hand a refreshed one back are the three logins §3.6 names, each into that tool's own store. No telemetry, no third-party proxies.
- Network calls include live quota probes against your own vendor accounts, the models.dev price list, mail/release links you click, and any device-sync endpoint you explicitly configure.
- Cross-machine sync (§4) moves only that same metadata — bundles carry no code, prompts or credentials. The legacy file-bundle path uses a channel you own (ssh/scp, Syncthing, or a mounted disk); strict device sync opens only the explicitly configured, approved primary over pinned HTTPS/TCP/TLS 1.3. tokenme uses no cloud relay, stores no third-party sync credentials, and validates every bundle or strict package before merging.

## 8. Troubleshooting

| Symptom | Fix |
| :--- | :--- |
| `no supported AI tool logs found` | Run one of your tools once, then `tokenme detect` |
| Costs look wrong | `tokenme pricing explain <model>` — check which listing won; override with `--pricing-override` |
| Numbers seem stale | `tokenme index --status`; another tokenme process may hold the ingest lease — `tokenme index --force` |
| A quota row stopped moving | Advanced → Check quotas automatically (the master gate) or Quota checks by tool turned that probe off: with a gate closed the row is the cache's last answer; with the master gate off the whole section reads **Polling stopped · data as of …**. Open the switch again — the next pass within 60 s or the refresh button restores it |
| The bottom bar's left corner reads **Updates stopped** | The engine is not publishing, so the figures are a snapshot of the previous pass — the swap happens by itself after five cadences without a publish, and it carries the time. The panel restarts the loop itself (backoff; the reason is in `panel.log`); press refresh to force a pass, and read `panel.log` if the badge outlives a minute. Not to be confused with the **From the last scan** chip: that one means the engine is alive and still scanning, showing the report the previous run published |
| Index corrupted | `tokenme index --rebuild` (quarantines the old file automatically) |
| Panel shows no data | Check `tokenme detect` finds sources; the panel reads the same index |
| A blank card for the first seconds after launch | Usually not: the first frame is the last report read from `report.json`, so figures are there the moment the page can paint. When you do see the card — a fresh install, or nothing restorable — what you are looking at is the window's **native backing** (white under Light, dark under Dark), not a page that failed to paint; `panel.log` says which phase cost the time |
| The panel opened in a screen corner, not under the menu bar | That open had no tray rectangle *and* the session exposed no monitors — a relaunch while the screen is locked or the display is asleep (`panel.log` says `no monitor to anchor to`). The next open anchors to the menu bar normally |
| Quota banners never arrive | Allow TokenMe to notify first (macOS System Settings → Notifications → TokenMe; Windows Settings → System → Notifications) — a denial leaves one `notify: [<window>] blocked` line in the log. Prove the plumbing by relaunching with `TOKENME_TEST_NOTIFY=1`: `panel.log` answers `notify test: posted` / `blocked` / `permission was just requested` |
| Sync badge is old or absent | On the collector: `systemctl --user list-timers tokenme-export.timer` (is the timer alive?) and `tokenme export` by hand; on the display machine the bundles must land in `~/tokenme-sync` — `tokenme import <file> --dry-run` shows what a merge would do |
| A bundle was rejected | The import message names the reason (sha256 mismatch = corrupted or truncated in transit; format/schema mismatch = version skew between the machines). The file is kept, nothing was merged — fix the transport or update the older side, then it retries by itself |


## Desktop pet greetings (Windows)

General settings has separate switches for pet greetings, break reminders and motions, plus the reminder interval and a **Say hello** preview. Greetings follow local time: morning 06:00–11:00, noon 11:00–14:00, evening 18:00–22:00 and a gentle night greeting from 22:00–06:00. Each period greets once per local day; the hours around midnight belong to one night.

Break reminders default to 60 minutes of active use. Three minutes idle, a screen lock, a hidden pet or sleep restarts the counter. Automatic delivery waits during fullscreen, and break reminders stay quiet between 22:00 and 06:00. Rest for 5 min pauses the counter; Remind in 10 min postpones the next reminder. The greeting card closes automatically and stays beside the pet without stealing focus; the existing click-to-open, drag and edge docking remain available. Say hello is an explicit interaction and works during fullscreen; the screen lock and the pet master switch still apply.

All existing visible skins have transient greeting, hop, stretch, sleepy and landing reactions. The kitten also has an additional four-pose reaction atlas: an energetic hello, a caring cup of tea, a sleepy yawn and an overhead stretch. The atlas is optional per skin and declared by `reactionAtlas` in `pet-skins.json`. Motions respect the system reduced-motion preference. All timing and notices run locally; no language-model call is involved.

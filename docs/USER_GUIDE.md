# TokenMe User Guide

> Version: 0.1.5 · Updated: 2026-10-05 · Audience: daily users · Covers both the CLI and the menu-bar panel

## 1. Install

```bash
git clone https://github.com/sp/tokenme.git
cd tokenme
cargo install --path crates/usage-cli --bin tokenme   # the CLI
```

Linux machines use the release tarball instead — `tokenme-cli-<v>-<arch>-linux-musl.tar.gz` (fully static, no Rust/Python needed on that machine):

```bash
tar xzf tokenme-cli-<v>-x86_64-linux-musl.tar.gz && cd tokenme-cli-<v>-x86_64-linux-musl
./install-linux.sh --every 15 --push you@display-host
```

That single line puts the binary in `~/.local/bin`, installs a timer to export every 15 minutes into `~/tokenme-sync`, and scp's each fresh bundle to the display machine's `~/tokenme-sync`; the panel there merges it on its next pass and shows it in the **Linux 同步 / Linux sync** badge. The timer is a systemd `--user` unit, falling back to an equivalent cron job on boxes without a reachable user manager (containers, bare ssh) — either way it runs without a login session once lingering is on for the user (on a headless server the installer says the one `loginctl enable-linger` line to run). `--uninstall` removes everything (your data stays). Without `--push`, sync that folder however you like (Syncthing, a mount, a nightly rsync) — any copy of the bundles works, imports are idempotent.

macOS menu-bar panel (optional, ad-hoc signed — right-click → Open on first launch):

```bash
./scripts/build-macos.sh          # produces TokenMe.app + a drag-install DMG
```

## 2. First run

```bash
tokenme detect        # 1. which of your tools were found
tokenme daily --days 7
tokenme quota
```

## 3. The menu-bar panel (macOS)

Built by `scripts/build-macos.sh`. Left-click the dual-ring icon toggles the panel; right-click opens the menu (refresh, data dir, autostart, quit).

Display modes — cycle them in the panel settings:

| Mode | Menu bar shows |
| :--- | :--- |
| 仅托盘 (tray) | icon only |
| 仅Token | token total, no icon |
| 仅花费 | cost, no icon |
| 托盘·Token | icon + tokens (default) |
| 托盘·花费 | icon + cost |
| 托盘·Token·花费 | icon + tokens + cost |

Refresh cadence, theme, money display and the desktop bubble are in panel settings. The cadence controls index re-scanning; vendor quota answers are cached for 5 minutes so probes stay polite. The status-bar refresh button is a full refresh: it re-scans logs, forces a fresh quota probe for every vendor (bypassing that cache) and pulls a new price table. Settings live in `settings.json` under the OS config directory.

Some tools store their login as a short-lived access token — MiniMax Code's file lasts about an hour, and tokenme renews it in place as it expires: it exchanges the same file's refresh token and writes the rotated pair back in MiniMax Code's own format (their lock file, their generation bump, atomic writes), so the app and the panel stay signed in together and the quota rows stay live without you opening the app. A failed exchange changes nothing — whether a login is dead is the app's call, not a probe's — and the row then keeps the last real number for up to six hours (the probe cache's grace) before disappearing until you sign in again.

A report older than five cadences (5 minutes at the default 30 s) puts **数据已停止更新 · Updates stopped** on the header's figure line with its age — frozen numbers and quiet numbers look identical otherwise, and this panel once served a six-hour-old "today" without saying so.

Once a Linux machine syncs in, a **Linux 同步 / Linux sync** badge joins that line: newest bundle per machine, its age and row count, warning-hued if the freshest merge is over 24 h old. Hover it for every machine's window and file name. No bundles have been imported → no badge; existing machines see nothing change.

## 4. Cross-machine sync (Linux collector → macOS/Windows panel)

Numbers from any machine can meet in one panel. A **collector** — the fully static Linux CLI — exports a window of its index into a folder; every **display machine** (macOS or Windows panel) merges that folder automatically. The transport is any file copy you already trust: tokenme never opens a port and has no accounts of its own.

Collector side, the minimal path (one line — same as §1):

```bash
./install-linux.sh --every 15 --push you@display-host
```

The timer (systemd `--user`, cron fallback) exports `~/tokenme-sync/tokenme-<host>.jsonl.gz` every 15 minutes and scp's each fresh bundle to the display machine's `~/tokenme-sync`. No push target? Drop `--push` and move that folder your own way (Syncthing, a mount, a nightly rsync) — or skip the installer entirely and run `tokenme export` by hand (defaults: last 30 days into `~/tokenme-sync`). A quiet or logless machine still produces a valid bundle (`rows 0`) — the timer never fails on a slow day.

Display side needs no setup: every panel pass merges any `tokenme-*.jsonl.gz` it finds in `~/tokenme-sync`, and the badge (above) shows the newest merge per machine. Merges are idempotent — re-importing the same file changes nothing. To rehearse or go manual: `tokenme import <bundle>` (add `--dry-run` to parse and reconcile, then roll back).

**The panel can do the SSH half for you.** Click the server icon in the panel footer and add a machine: address, user, and the SSH password once (a private-key file works too, and both accept `~`). The wizard pins the host's SSH fingerprint on first connect, generates a dedicated key pair, plants its public half in the remote `~/.ssh/authorized_keys`, uploads the static collector to `~/.tokenme/bin/`, runs the first export, and pulls + merges the bundle — the whole install is one guided flow with a per-step log. After that the panel re-pulls every 15 minutes (configurable per server), and each fresh merge shows up like any file-based source, per machine. The password is used for that single setup connection and is never written anywhere. Removing a server cleans up both ends: the collector binary and the key line are deleted on the remote machine, and the pinned fingerprint and history are dropped locally.

**Security & limits**

- No listener, no cloud, no sync account: authentication is entirely the channel you pick (ssh keys for scp, Syncthing TLS + device IDs, …). tokenme stores no sync credentials. The in-panel SSH flow keeps that property: one password authenticates one setup connection, the host key is pinned on first sight (shown in the sheet), and steady-state trust is a single dedicated key line in the remote `authorized_keys` — revocable by deleting that line or removing the server in the panel.
- Every import is validated before a single row lands: the bundle's sha256 against its manifest, the batch's token sums against the manifest, and a format + schema-version gate. The merge runs in one transaction — any failure rolls back whole, the file is kept on disk, and nothing merges.
- Row counts are capped (the manifest's count plus slack), so a corrupt file can't exhaust memory. A bundle can only move a row forward — the growth predicate refuses lower counts — and imports never delete: rows a collector purged later are reported as stale, not removed.
- On unix, bundles and manifests are written `0600`. A bundle carries what the index already carries — token counts, model/session/project names, local log paths — never code, prompts or credentials. Treat `~/tokenme-sync` like the index itself: whoever can write into that folder (your account, or anyone you share it with) can inject usage rows — merges are duplicate-safe, not signed.

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
| Cline | Tokens | `~/.cline/data/sessions/**` |
| ZCode | Tokens | `~/.zcode/cli/db/db.sqlite` |
| Antigravity CLI | Tokens | `~/.gemini/antigravity-cli/conversations/*.db` |
| Qoder | Credits + Tokens | `~/.qoder/projects/**/*.jsonl` (credits) · `~/Library/Application Support/Qoder{,CN}/SharedClientCache/cache/db/local.db` (tokens) |
| WorkBuddy AI | Tokens + credits | `~/.workbuddy-ai/projects/**/<session>.jsonl` |
| AgnesCode / AtomCode / Crow5 / Mimocode / Cola / DSH | Tokens | auto-discovered, same shape as their parent tools |
| Kimi Code | Tokens | `~/.kimi-code/sessions/**/wire.jsonl` (legacy `~/.kimi`; the desktop app's embedded runtime home under `~/Library/Application Support/kimi-desktop/daimon-share/daimon/runtime/kimi-code/home` is read too) |
| MiniMax Code | Tokens | `~/.minimax/v2/sessions/**/messages.jsonl` (legacy `~/.mavis`) |

Cloud-only tools without local per-request logs (Cursor, VS Code Copilot) are not supported locally.

## 7. Privacy

- Only per-request token counts, model names and project paths are indexed — never your code or prompts.
- Scanning is read-only. No telemetry, no third-party proxies.
- The only network calls: live quota probes against your own vendor accounts, the models.dev price list, and (if you click it) the mail/release links in the panel.
- Cross-machine sync (§4) moves only that same metadata between your machines — bundles carry no code, prompts or credentials — over a channel you own (ssh/scp, Syncthing, a mounted disk). tokenme opens no ports, stores no sync credentials, and validates every bundle (sha256 + sums + schema) before merging.

## 8. Troubleshooting

| Symptom | Fix |
| :--- | :--- |
| `no supported AI tool logs found` | Run one of your tools once, then `tokenme detect` |
| Costs look wrong | `tokenme pricing explain <model>` — check which listing won; override with `--pricing-override` |
| Numbers seem stale | `tokenme index --status`; another tokenme process may hold the ingest lease — `tokenme index --force` |
| Header reads **数据已停止更新 / Updates stopped** | The engine is not publishing, so the figures are a snapshot of the last pass. The panel restarts the loop itself (backoff, reason in `panel.log`); press refresh to force a pass, and read `panel.log` if the badge outlives a minute |
| Index corrupted | `tokenme index --rebuild` (quarantines the old file automatically) |
| Panel shows no data | Check `tokenme detect` finds sources; the panel reads the same index |
| Sync badge is old or absent | On the collector: `systemctl --user list-timers tokenme-export.timer` (is the timer alive?) and `tokenme export` by hand; on the display machine the bundles must land in `~/tokenme-sync` — `tokenme import <file> --dry-run` shows what a merge would do |
| A bundle was rejected | The import message names the reason (sha256 mismatch = corrupted or truncated in transit; format/schema mismatch = version skew between the machines). The file is kept, nothing was merged — fix the transport or update the older side, then it retries by itself |

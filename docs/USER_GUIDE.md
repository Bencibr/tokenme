# TokenMe User Guide

> Version: 0.1.0 · Updated: 2026-09-29 · Audience: daily users · Covers both the CLI and the menu-bar panel

## 1. Install

```bash
git clone https://github.com/sp/tokenme.git
cd tokenme
cargo install --path crates/usage-cli --bin tokenme   # the CLI
```

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

## 4. Budgets

For tools whose vendor publishes no limit (Cline, ZCode, …), set your own caps — tokenme measures them against its own cost math and raises a quota bar:

```bash
tokenme budget set zcode --daily 5 --monthly 50
```

## 5. Supported tools & data sources

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

## 6. Privacy

- Only per-request token counts, model names and project paths are indexed — never your code or prompts.
- Scanning is read-only. No telemetry, no third-party proxies.
- The only network calls: live quota probes against your own vendor accounts, the models.dev price list, and (if you click it) the mail/release links in the panel.

## 7. Troubleshooting

| Symptom | Fix |
| :--- | :--- |
| `no supported AI tool logs found` | Run one of your tools once, then `tokenme detect` |
| Costs look wrong | `tokenme pricing explain <model>` — check which listing won; override with `--pricing-override` |
| Numbers seem stale | `tokenme index --status`; another tokenme process may hold the ingest lease — `tokenme index --force` |
| Header reads **数据已停止更新 / Updates stopped** | The engine is not publishing, so the figures are a snapshot of the last pass. The panel restarts the loop itself (backoff, reason in `panel.log`); press refresh to force a pass, and read `panel.log` if the badge outlives a minute |
| Index corrupted | `tokenme index --rebuild` (quarantines the old file automatically) |
| Panel shows no data | Check `tokenme detect` finds sources; the panel reads the same index |

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

Refresh cadence, theme, money display and the desktop bubble are in panel settings. Settings live in `settings.json` under the OS config directory.

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
| Qoder | Credits | `~/.qoder/projects/**/*.jsonl` |
| WorkBuddy AI | Credits | `~/.workbuddy-ai/workbuddy.db` |
| AgnesCode / AtomCode / Crow5 / Mimocode / Cola / DSH | Tokens | auto-discovered, same shape as their parent tools |

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
| Index corrupted | `tokenme index --rebuild` (quarantines the old file automatically) |
| Panel shows no data | Check `tokenme detect` finds sources; the panel reads the same index |

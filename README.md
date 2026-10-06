<p align="center">
  <img alt="TokenMe Hero Banner" src="assets/tokenme-hero-dark.svg" width="100%">
</p>

<p align="center">
  <a href="https://github.com/Bencibr/tokenme"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-1.75%2B-orange.svg" alt="Rust Version"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-green.svg" alt="Platform Support">
  <img src="https://img.shields.io/badge/index-local--only-success.svg" alt="Local Index">
</p>

<p align="center">
  <b>One menu bar dashboard for every AI coding tool's token usage, spend, and remaining quota — so you stop checking each vendor's console.</b>
</p>

<p align="center">
  <b>English</b> | <a href="README_CN.md">简体中文</a>
</p>

## What is TokenMe?

TokenMe is a menu bar app that tracks token usage, spend, and subscription quota across the AI coding tools you already run. Native installers for macOS and Windows, plus a CLI. It answers three questions that usually have no good home:

- How many tokens did I burn today, and what did they cost?
- How much is left on each subscription, and when does it reset?
- Which tool owns this month's spend — and how much did caching actually save me?

Those numbers normally live in each vendor's own console and local logs, and none of them speak the same language. TokenMe reads the logs and databases your tools already write to disk and folds them into one local index.

## Sound familiar?

Working across Claude Code, Codex, ZCode and the rest, this tends to happen:

- You want to know what today cost, so you open three vendor consoles — and their numbers don't agree.
- A 5-hour rolling window runs dry right when the work is hottest.
- Points, credits, dollar windows — every subscription ships its own dashboard, and none of them talk to each other.
- At month's end you want the per-tool breakdown, and the only way to get it is parsing logs by hand.

TokenMe puts all of it in one panel, a click away.

## Screenshots

<p align="center">
  <img alt="TokenMe panel — Overview tab, light" src="docs/screenshots/panel-en-overview.png" width="24%">
  <img alt="TokenMe panel — Tools tab, light" src="docs/screenshots/panel-en-tools.png" width="24%">
  <img alt="TokenMe panel — Overview tab, dark" src="docs/screenshots/panel-en-overview-dark.png" width="24%">
  <img alt="TokenMe panel — Tools tab, dark" src="docs/screenshots/panel-en-tools-dark.png" width="24%">
</p>

## Inside the panel

The panel opens with four pages, each one drilling deeper:

- **Overview** — today's tokens, spend, and cache hit rate; an hour-by-hour usage chart; and each vendor's quota bar with remaining share and reset countdown.
- **Tools** — one row per tool: requests, sessions, tokens, cache rate — with each tool's share painted into the row itself.
- **Rankings** — sessions ranked, so the expensive ones surface immediately.
- **Details** — per-request records, filterable by model and project.

The UI follows the system language: English and Chinese. Costs are converted, not billed — token counts times the [models.dev](https://models.dev) price list. It won't match a vendor invoice to the cent, but it answers where the money went and whether it was well spent. The index handles hundreds of thousands of events, builds in seconds, and each incremental scan after that takes tens of milliseconds.

## The CLI

When the panel stays closed, the CLI reads the same index:

```text
$ tokenme daily --days 3

┌────────────┬──────────┬──────────┬────────┬────────────┬────────┬────────┬─────────┬────────┐
│ Day        │ sessions │ requests │  input │ cache read │ output │  total │    cost │ cached │
├────────────┼──────────┼──────────┼────────┼────────────┼────────┼────────┼─────────┼────────┤
│ 2026-09-25 │      216 │   12,900 │  56.2M │      1.68B │   4.5M │  1.74B │  $99.15 │  96.8% │
│ 2026-09-26 │       45 │    4,592 │  30.5M │      704.9M │   1.8M │ 737.1M │  $54.53 │  95.9% │
│ 2026-09-27 │        6 │    2,534 │  25.1M │      377.1M │   1.1M │ 403.4M │  $46.49 │  93.8% │
├────────────┼──────────┼──────────┼────────┼────────────┼────────┼────────┼─────────┼────────┤
│ total      │      256 │   20,026 │ 111.8M │      2.76B │   7.4M │  2.88B │ $200.18 │  96.1% │
└────────────┴──────────┴──────────┴────────┴────────────┴────────┴────────┴─────────┴────────┘
```

## Install

**Homebrew (macOS)** — tap once, then short names:

```bash
brew tap Bencibr/tokenme https://github.com/Bencibr/homebrew-tokenme
brew trust bencibr/tokenme        # once — brew 6 asks before trusting third-party casks
brew install --cask tokenme       # the menu-bar panel
brew install tokenme-cli          # the CLI
```

The tap is bumped automatically on every release, so `brew upgrade` keeps you current, and cask installs carry no quarantine flag — Gatekeeper never steps in.

**Installers**: grab one from [Releases](https://github.com/Bencibr/tokenme/releases/latest) — a macOS menu bar app (drag to Applications) and a Windows setup are both there.

> **First launch on macOS** (manual DMG only): the bundle is ad-hoc signed (no developer certificate), so Gatekeeper may step in. Right-click → Open works, or clear the quarantine flag:
>
> ```bash
> sudo xattr -rd com.apple.quarantine /Applications/TokenMe.app
> ```

Build from source instead:

```bash
git clone https://github.com/Bencibr/tokenme.git
cd tokenme
cargo install --path crates/usage-cli --bin tokenme   # CLI
./scripts/build-macos.sh                              # macOS panel
```

Once it's in, run a detection pass and look at the last week:

```bash
tokenme detect          # find every installed tool and where its logs live
tokenme daily --days 7  # the last week's daily usage and spend
tokenme quota           # live quotas and reset countdowns
```

## Common commands

| Command | What it does |
| :--- | :--- |
| `tokenme daily --days 30` | Day-by-day throughput, cache rate, and cost |
| `tokenme report --window week --group model` | One window, broken down every which way |
| `tokenme quota` | Live quotas with reset countdowns |
| `tokenme budget set zcode --monthly 50` | Put a monthly cap on a tool that has none |
| `tokenme export` / `tokenme import` | Ship a window of the index to another machine and merge it back — file-based, idempotent, no accounts |
| `tokenme pricing explain <model>` | Audit where a price comes from |

## Supported tools & live quota

The quota bars come from each tool's own API or local credentials — read-only probes that never refresh or stand in for your login. When a host app quits, its probes pause and the last numbers stay on screen; relaunch the app and they resume. The "pause quota on exit" setting controls this.

Every tool below ships as a built-in adapter — twenty-two of them, indexed straight from the logs and databases each one writes to disk:

| Tool | Live quota | Fixtures | Verified on |
| :--- | :--- | :--- | :--- |
| <img src="crates/usage-core/assets/claude.png" width="20" alt=""> **Claude Code** | 5-hour / cycle windows — official backend | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/codex.png" width="20" alt=""> **Codex** | ChatGPT rate windows — official backend | ✅ | macOS ✅ · Windows ✅ |
| <img src="crates/usage-core/assets/opencode.png" width="20" alt=""> **OpenCode** | Go subscription, three dollar windows | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/pi.png" width="20" alt=""> **Pi** | — | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/cline.png" width="20" alt=""> **Cline** | Account window limits — Cline account API | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/zcode.png" width="20" alt=""> **ZCode** | 5-hour/cycle windows + session budgets — the IDE's own quota API + local cookie decryption | ✅ | macOS ✅ · Windows ✅ |
| <img src="crates/usage-core/assets/qoder.png" width="20" alt=""> **Qoder** | Subscription windows — vendor usage API, local encrypted snapshot fallback | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/antigravity.png" width="20" alt=""> **Antigravity** | Self-computed — runs the CLI's `/usage` | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/agnes.png" width="20" alt=""> **AgnesCode** | Membership points — needs a one-time token (see below) | ✅ | macOS ✅ · Windows ✅ |
| <img src="crates/usage-core/assets/atomcode.png" width="20" alt=""> **AtomCode** | CodingPlan quota — local daemon | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/workbuddy.png" width="20" alt=""> **WorkBuddy** | Member credits — the desktop app's billing API / local broker | ✅ | macOS ✅ · Windows ✅ |
| <img src="crates/usage-core/assets/hermes.png" width="20" alt=""> **Hermes** | — | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/funide.png" width="20" alt=""> **FunIDE** | GLM-plan points — cloud points API | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/catpaw.png" width="20" alt=""> **CatPaw** | Points balance — points portal API | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/dsh.png" width="20" alt=""> **DSH** | DeepSeek account balance — official user/balance + desktop credential file | ✅ | macOS ✅ · Windows ✅ |
| <img src="crates/usage-core/assets/crow5.png" width="20" alt=""> **Crow5** | — | ✅ | macOS ✅ · Windows ⏳ |
| **Mimocode** | — | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/cola.png" width="20" alt=""> **Cola** | Plan quota — vendor billing API, local credential decryption | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/joycode.png" width="20" alt=""> **JoyCode** | IDE points — reads the IDE's own login state | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/trae.png" width="20" alt=""> **Trae** | Subscription quota — vendor v1 API, local credential decryption | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/kimicode.png" width="20" alt=""> **Kimi Code** | Plan windows — 5h / weekly / monthly via the vendor's `/usages`; usage read from the CLI **or** the desktop app's embedded runtime; credentials are read-only (never refreshed) | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/minimaxcode.png" width="20" alt=""> **MiniMax Code** | Plan windows — 5h / weekly — plus the credit balance (purchased + check-in wallets); the ~1 h access token is renewed in place from the app's own refresh token | ✅ | macOS ✅ · Windows ⏳ |

> **Fixtures**: every adapter carries a fixture test suite, run in full on CI for each commit, and every integration was verified against a real machine's data before it landed.
>
> **Verified on**: updated as real-machine verification progresses. ⏳ means the paths are implemented but not yet exercised on that platform.
>
> Quota-only sources: **Copilot** (premium-request allowance, official backend) probes quota only and indexes no usage; probing for **Gemini CLI** is deliberately off — its on-disk OAuth token expires, and refreshing credentials is outside a read-only probe's line.

> **AgnesCode token**: its login token lives only in the app's memory and can't be read silently. Sign in at agnescode.agnes-ai.cn, copy the `access_token` request header, and write it to `~/.config/tokenme/agnes.token` (or set `AGNES_TOKEN`).

Full reference: **[docs/COMMANDS.md](docs/COMMANDS.md)** · Task guides: **[docs/USER_GUIDE.md](docs/USER_GUIDE.md)** · Architecture: **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** · Adapter notes: [docs/internal/ADAPTERS_DESIGN.md](docs/internal/ADAPTERS_DESIGN.md)

## Data & privacy

Only counts get indexed, never content — your code and prompts are not stored. What lands in the index is each request's token counts, model name, and project path. Scanning is read-only throughout; no telemetry, no third-party proxies. The index lives in your system data directory (`~/Library/Application Support/tokenme/` on macOS), and each tool's log paths are listed in the [user guide](docs/USER_GUIDE.md).

Machines can sync their indexes to each other — a file-based `tokenme export` / `tokenme import` over a channel you own (ssh/scp, Syncthing), no accounts and no cloud. Every merge is sha256- and manifest-validated before a single row lands, in one all-or-nothing transaction, and bundles are written `0600` on unix.

## Community

| | |
| :--- | :--- |
| Bug reports | [GitHub Issues](https://github.com/Bencibr/tokenme/issues) |
| WeChat user group | <img src="docs/wechat-group.png" width="180" alt="TokenMe WeChat group"> |
| Email | Panel → Settings → Contact us (address in `apps/tokenme-bar/src/lib/about.ts`) |

## License

Released under the [MIT](LICENSE) license.

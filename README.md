<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/tokenme-hero-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/tokenme-hero-light.svg">
    <img alt="TokenMe Hero Banner" src="assets/tokenme-hero-dark.svg" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/your-org/tokenme"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg" alt="License"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-1.75%2B-orange.svg" alt="Rust Version"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-green.svg" alt="Platform Support">
  <img src="https://img.shields.io/badge/privacy-100%25%20Local-success.svg" alt="Privacy First">
</p>

<p align="center">
  <b>Cross-tool AI coding token, cost, and real-time quota monitor.</b>
  <br>
  Zero telemetry · Zero proxy · Instant scanning · 100% Local privacy
</p>

<p align="center">
  <b>English</b> | <a href="README_CN.md">简体中文</a>
</p>

---

## 💡 Why TokenMe?

When developing with multiple AI coding tools (**Claude Code**, **Codex**, **OpenCode**, **Cline**, **ZCode**, etc.), you often face a visibility black box:
- *How many tokens did I actually consume today? How much did it cost?*
- *How much quota remains in my 5-hour rolling pool or weekly plan?*
- *What is my real prompt cache hit rate across projects?*

**TokenMe** solves AI coding usage and billing anxiety. It reads native logs and local databases directly from your tools, builds an instant local SQLite index, and gives you a single dashboard with live quotas.

---

## ✨ Features

- 🔒 **100% Local Privacy**: Runs entirely offline. No telemetry, no remote proxies, and zero credential tampering.
- ⚡ **Instant Scanning**: High-performance Rust incremental index scans hundreds of thousands of events in seconds.
- 🎯 **15+ Tools Supported**: Out-of-the-box recognition for Claude Code, Codex, OpenCode, Cline, ZCode, Qoder, Pi, and more.
- 📊 **Token & Cost Analytics**: Clear breakdown of input, cache read, output, and costs calculated via [models.dev](https://models.dev).
- ⏱️ **Live Quota Probes**: Real-time read-only probes for rolling 5-hour windows, weekly quotas, and MCP allowances.

---

## ⚡ Preview

```text
$ tokenme daily --days 3

┌────────────┬──────────┬──────────┬────────┬────────────┬────────┬────────┬─────────┬────────┐
│ Day        │ sessions │ requests │  input │ cache read │ output │  total │    cost │ cached │
├────────────┼──────────┼──────────┼────────┼────────────┼────────┼────────┼─────────┼────────┤
│ 2026-09-25 │      216 │   12,900 │  56.2M │      1.68B │   4.5M │  1.74B │  $99.15 │  96.8% │
│ 2026-09-26 │       45 │    4,592 │  30.5M │     704.9M │   1.8M │ 737.1M │  $54.53 │  95.9% │
│ 2026-09-27 │        6 │    2,534 │  25.1M │     377.1M │   1.1M │ 403.4M │  $46.49 │  93.8% │
├────────────┼──────────┼──────────┼────────┼────────────┼────────┼────────┼─────────┼────────┤
│ total      │      256 │   20,026 │ 111.8M │      2.76B │   7.4M │  2.88B │ $200.18 │  96.1% │
└────────────┴──────────┴──────────┴────────┴────────────┴────────┴────────┴─────────┴────────┘
last 3 days · 2026-09-25 → 2026-09-27
prices: cached models.dev snapshot, 2,037 models
```

---

## 🚀 Quick Start

### Installation

Requires [Rust 1.75+](https://rustup.rs/):

```bash
git clone https://github.com/your-org/tokenme.git
cd tokenme
cargo install --path crates/usage-cli --bin tokenme
```

### 3-Step Walkthrough

```bash
# 1. Discover local installed AI tools and logs
tokenme detect

# 2. View token usage and cost for the past 7 days
tokenme daily --days 7

# 3. Check live subscription quotas and reset timers
tokenme quota
```

---

## 🛠️ Essential Commands

| Command | Description | Example |
| :--- | :--- | :--- |
| `tokenme detect` | Scan and list all detected AI tool directories | `tokenme detect` |
| `tokenme daily` | Daily token throughput, cache rate & estimated cost | `tokenme daily --days 30` |
| `tokenme weekly` | Weekly usage trends | `tokenme weekly --weeks 8` |
| `tokenme monthly` | Long-term monthly billing summaries | `tokenme monthly --months 6` |
| `tokenme quota` | Pull live quotas and window reset countdowns | `tokenme quota` |
| `tokenme budget` | Set custom daily or monthly spending limits | `tokenme budget set zcode --monthly 50` |
| `tokenme pricing` | Audit pricing sources and provider price diffs | `tokenme pricing explain deepseek-v4-flash` |

---

## 🧩 Supported Ecosystem

| Tool | Metric | Data Source |
| :--- | :--- | :--- |
| **Claude Code** | Tokens | `~/.claude/projects/**/*.jsonl` |
| **Codex** | Tokens | `~/.codex/sessions/**/rollout-*.jsonl` |
| **OpenCode** | Tokens | `~/.local/share/opencode/opencode.db` |
| **Pi** | Tokens | `~/.pi/agent/sessions/**` |
| **Cline** | Tokens | `~/.cline/data/sessions/**` |
| **ZCode** | Tokens | `~/.zcode/cli/db/db.sqlite` |
| **Antigravity CLI** | Tokens | `~/.gemini/antigravity-cli/conversations/*.db` |
| **Qoder** | Credits | `~/.qoder/projects/**/*.jsonl` |
| **WorkBuddy AI** | Credits | `~/.workbuddy-ai/workbuddy.db` |
| *Others* | Tokens | AgnesCode, AtomCode, Crow5, Mimocode, Cola... |

> 📌 *Note: Cloud-only tools without local per-request logs (e.g. Cursor, VS Code Copilot) are not supported locally.*

---

## 🔒 Security & Data Paths

- **Zero-Data-Exfiltration**: Your code, prompts, and tokens never leave your computer.
- **Read-Only**: Existing tool files are inspected in read-only mode to prevent corruption.
- **Local Storage**:
  - SQLite Index: `~/Library/Application Support/tokenme/index.db` (macOS) / `~/.local/share/tokenme/` (Linux) / `%LOCALAPPDATA%\tokenme\` (Windows)
  - Settings: `settings.json` in standard OS config directory.

---

## 📄 License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).

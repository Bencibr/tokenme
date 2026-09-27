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
  <b>跨平台本地 AI 编码 Token 消耗、成本账单与实时配额监视器</b>
  <br>
  零上传 · 零代理 · 秒级扫描 · 精准对账
</p>

<p align="center">
  <a href="README.md">English</a> | <b>简体中文</b>
</p>

---

## 💡 为什么需要 TokenMe？

当你同时使用 **Claude Code**、**Codex**、**OpenCode**、**Cline** 等多个 AI 编程助手时，经常面临用量“黑盒”：
- *今天究竟消耗了多少 Token？花了多少钱？*
- *各厂商的 5 小时滚动窗口或周度配额还剩多少？*
- *跨项目的 Prompt 缓存命中率到底高不高？*

**TokenMe** 专为消除 AI 编码用量与账单焦虑而生。它直接读取本机各工具已落盘的本地日志与数据库，建立极速 SQLite 增量索引，为你呈现全景式的实时消耗大盘与配额预警。

---

## ✨ 核心特性

- 🔒 **100% 本地隐私安全**：完全离线运行。**不上传、不经过任何第三方代理、不修改原工具凭据**。
- ⚡ **秒级极速扫描**：高性能 Rust 增量索引引擎，几十万条事件秒级处理，极低内存与 CPU 占用。
- 🎯 **开箱即用支持 15+ 款工具**：自动识别 Claude Code、Codex、OpenCode、Cline、ZCode、Qoder、Pi 等。
- 📊 **多维度成本与用量洞察**：清晰展示输入、缓存命中、推理、输出 Token 及折算金额（对接 [models.dev](https://models.dev) 价格清单）。
- ⏱️ **实时配额只读探针**：直连厂商接口拉取 5 小时池、周度额度与 MCP 额度健康度。

---

## ⚡ 效果预览

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

## 🚀 快速上手 (Quick Start)

### 编译与安装

前置要求：本地已安装 [Rust 1.75+](https://rustup.rs/)：

```bash
git clone https://github.com/your-org/tokenme.git
cd tokenme
cargo install --path crates/usage-cli --bin tokenme
```

### 常用操作三板斧

```bash
# 1. 自动探测本机已存在的工具与日志
tokenme detect

# 2. 查看最近 7 天的每日消耗与账单大盘
tokenme daily --days 7

# 3. 实时查询各厂商订阅额度与配额使用率
tokenme quota
```

---

## 🛠️ 常用命令速查

| 命令 | 说明 | 示例 |
| :--- | :--- | :--- |
| `tokenme detect` | 扫描本机所有支持的 AI 工具日志目录与状态 | `tokenme detect` |
| `tokenme daily` | 按天汇总 Token 吞吐、缓存率与折算费用 | `tokenme daily --days 30` |
| `tokenme weekly` | 按周查看用量趋势 | `tokenme weekly --weeks 8` |
| `tokenme monthly` | 按月查看长周期账单与消耗 | `tokenme monthly --months 6` |
| `tokenme quota` | 实时拉取各厂商配额、窗口重置时间与已用百分比 | `tokenme quota` |
| `tokenme budget` | 为工具设定/查看自定义成本预算上限 | `tokenme budget set zcode --monthly 50` |
| `tokenme pricing` | 查询指定模型的定价来源与各供应商价格对比 | `tokenme pricing explain deepseek-v4-flash` |

---

## 🧩 支持的工具生态矩阵

| 工具名称 | 计量单位 | 本地数据源路径 |
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
| *其他适配* | Tokens | AgnesCode, AtomCode, Crow5, Mimocode, Cola 等 |

> 📌 *注：Cursor / VS Code Copilot 等工具本地未保存每请求原始日志（仅云端统计），暂不支持离线解析。*

---

## 🔒 隐私与本地存储位置

- **安全承诺**：你的代码、Prompt 与 Token 数据**永不出本机**。所有扫描均为只读模式。
- **本地数据路径**：
  - SQLite 索引数据库：macOS: `~/Library/Application Support/tokenme/index.db`；Linux: `~/.local/share/tokenme/index.db`；Windows: `%LOCALAPPDATA%\tokenme\index.db`
  - 配置文件：系统标准 Config 目录下的 `tokenme/settings.json`

---

## 📚 开发者深入文档

- [适配器底层架构与逆向工程备忘](docs/internal/ADAPTERS_DESIGN.md)
- [数据对账与验证脚本](scripts/)

---

## 📄 开源许可证

本项目基于 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双重许可开源。

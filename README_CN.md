<p align="center">
  <img alt="TokenMe Hero Banner" src="assets/tokenme-hero-dark.svg" width="100%">
</p>

<p align="center">
  <a href="https://github.com/sp/tokenme"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-1.75%2B-orange.svg" alt="Rust Version"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-green.svg" alt="Platform Support">
  <img src="https://img.shields.io/badge/index-local--only-success.svg" alt="Local Index">
</p>

<p align="center">
  <b>跨平台本地 AI 编码 Token 消耗、成本账单与实时配额监视器</b>
  <br>
  零遥测 · 零第三方代理 · 秒级扫描 · 精准对账
</p>

<p align="center">
  <a href="README.md">English</a> | <b>简体中文</b>
</p>

## 💡 为什么需要 TokenMe？

同时使用 Claude Code、Codex、OpenCode、Cline、ZCode 等多个 AI 编程工具时，用量是个"黑盒"：今天花了多少钱？5 小时滚动池还剩多少？跨项目缓存命中率到底高不高？TokenMe 直接读取各工具落盘的本地日志与数据库，建立秒级 SQLite 增量索引，在 CLI 和菜单栏里一次性回答这三个问题。

## ✨ 核心特性

- 🔒 **本地索引，只读扫描**：用量数据在本机建立索引。无自有遥测、无第三方代理；实时配额探测与价格清单是仅有的联网环节。
- ⚡ **秒级极速扫描**：高性能 Rust 增量索引引擎，几十万条事件秒级处理，极低内存与 CPU 占用。
- 🎯 **开箱即用支持 16 款工具**：Claude Code、Codex、OpenCode、Cline、ZCode、Qoder、Pi、Antigravity、WorkBuddy、AgnesCode、AtomCode、Crow5、Mimocode、Cola、DSH、Hermes。
- 📊 **多维度成本与用量洞察**：输入、缓存命中、推理、输出 Token 及折算金额（对接 [models.dev](https://models.dev) 价格清单）。
- ⏱️ **实时配额只读探针**：5 小时滚动池、周度额度、Credit 计划与重置倒计时。

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
```

## 🚀 快速上手

**直接下载**安装包即可使用：到 [Releases](https://github.com/sp/tokenme/releases/latest) 获取 macOS 菜单栏应用（拖入 Applications）与 Windows 安装程序。也可从源码构建：

```bash
git clone https://github.com/sp/tokenme.git
cd tokenme
cargo install --path crates/usage-cli --bin tokenme   # CLI
./scripts/build-macos.sh                              # macOS 面板
```

然后：

```bash
tokenme detect          # 1. 自动探测本机工具与日志
tokenme daily --days 7  # 2. 最近一周的每日消耗与账单
tokenme quota           # 3. 实时查询订阅额度与重置倒计时
```

## 🛠️ 常用命令

| 命令 | 说明 |
| :--- | :--- |
| `tokenme daily --days 30` | 按天汇总吞吐、缓存率与费用 |
| `tokenme report --window week --group model` | 单窗口多维拆分与环比 |
| `tokenme quota` | 实时配额与重置倒计时 |
| `tokenme budget set zcode --monthly 50` | 为无限额工具设定成本上限 |
| `tokenme pricing explain <model>` | 审计定价来源 |

完整参考：**[docs/COMMANDS.md](docs/COMMANDS.md)** · 场景指南：**[docs/USER_GUIDE.md](docs/USER_GUIDE.md)** · 架构总览：**[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** · 适配器逆向备忘：[docs/ADAPTERS_DESIGN.md](docs/ADAPTERS_DESIGN.md)

## 🔒 隐私与数据

只索引计数，不碰内容——你的代码与 Prompt 不会被存储，入库的只有每次请求的 token 计数、模型名与项目路径。扫描全程只读；无遥测、无第三方代理。索引库位于系统数据目录（macOS 为 `~/Library/Application Support/tokenme/`）；各工具日志路径见[用户指南](docs/USER_GUIDE.md)。

## 💬 社区

| | |
| :--- | :--- |
| 问题反馈 | [GitHub Issues](https://github.com/sp/tokenme/issues) |
| 微信交流群 | <img src="docs/wechat-group.png" width="180" alt="TokenMe 微信交流群"> |
| 邮箱 | 面板「设置 → 联系我们」直达（地址在 `apps/tokenme-bar/src/lib/about.ts`） |

## 📄 开源许可证

基于 [MIT](LICENSE) 协议开源。

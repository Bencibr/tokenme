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
  <b>同时用着好几个 AI 编程工具？这里能看清每个工具花了多少、还剩多少额度——菜单栏点开就有。</b>
</p>

<p align="center">
  <a href="README.md">English</a> | <b>简体中文</b>
</p>

## 这是什么

TokenMe 是一个统计 AI 编程工具 token 用量、费用与订阅额度的菜单栏应用，提供 macOS 和 Windows 的安装包，并具备命令行工具。它回答三个平时没地方看的问题：

- 今天用了多少 token、折合多少钱？
- 各家订阅的额度还剩多少、什么时候重置？
- 这个月每个工具各占了多少开销，缓存到底省了多少？

这些数字平时散在各家自己的控制台和本地日志里，口径互不相通。TokenMe 直接读取这些工具写在磁盘上的日志和数据库，在本机汇成一份索引。

## 解决什么问题

同时用 Claude Code、Codex、ZCode 和其他 AI 编程工具时，这些场景大概不陌生：

- 想知道今天花了多少钱，得挨个打开各家的控制台，口径还对不上；
- 5 小时滚动窗口快用完了才发现，正赶上活儿最紧的时候；
- 点数、credits、美元窗口——每种订阅一个看板，各看各的；
- 月底想复盘：哪个工具是大头、缓存命中率省了多少钱，只能翻日志自己算。

TokenMe 把这些收进同一个面板，菜单栏随时点开。

## 运行截图

<p align="center">
  <img alt="TokenMe 面板 — 概览页，浅色" src="docs/screenshots/panel-zh-overview.png" width="24%">
  <img alt="TokenMe 面板 — 工具页，浅色" src="docs/screenshots/panel-zh-tools.png" width="24%">
  <img alt="TokenMe 面板 — 概览页，深色" src="docs/screenshots/panel-zh-overview-dark.png" width="24%">
  <img alt="TokenMe 面板 — 工具页，深色" src="docs/screenshots/panel-zh-tools-dark.png" width="24%">
</p>

## 面板里有什么

菜单栏点开后分四页，一层层往下钻：

- **概览**——当天的 token、费用与缓存命中率，按小时分布的用量柱状图，下面是各家配额条：剩余比例和重置倒计时；
- **工具**——每个工具的请求数、会话数、token 量与缓存率，一行一个，用量占比就画在行底色里；
- **排行**——按会话排名，一眼找出最贵的那几个；
- **明细**——单次请求级的记录，可以按模型、项目筛选。

界面语言跟随系统，中英双语。费用是折算出来的：token 计数乘以 [models.dev](https://models.dev) 的模型单价——和官方扣款记录未必分毫不差，但足够回答钱花在哪、花得值不值。几十万条事件的索引在本机秒级建好，之后每次增量扫描只需几十毫秒。

## 命令行

不开面板时，CLI 读的是同一份索引：

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

## 安装

到 [Releases](https://github.com/sp/tokenme/releases/latest) 下载安装包，里面有 macOS 菜单栏应用（拖入 Applications）和 Windows 安装程序。

> **macOS 首次打开**：安装包是 ad-hoc 签名（没有开发者证书），Gatekeeper 可能拦一下——右键打开即可，或者清掉隔离标记后正常启动：
>
> ```bash
> sudo xattr -rd com.apple.quarantine /Applications/TokenMe.app
> ```

想从源码构建：

```bash
git clone https://github.com/sp/tokenme.git
cd tokenme
cargo install --path crates/usage-cli --bin tokenme   # CLI
./scripts/build-macos.sh                              # macOS 面板
```

装好先跑一次探测，再看最近一周的账：

```bash
tokenme detect          # 自动探测本机装了哪些工具、日志在哪
tokenme daily --days 7  # 最近一周的每日消耗与账单
tokenme quota           # 各家订阅的实时额度与重置倒计时
```

## 常用命令

| 命令 | 说明 |
| :--- | :--- |
| `tokenme daily --days 30` | 按天汇总吞吐、缓存率与费用 |
| `tokenme report --window week --group model` | 单窗口多维拆分与环比 |
| `tokenme quota` | 实时配额与重置倒计时 |
| `tokenme budget set zcode --monthly 50` | 为无限额工具设定成本上限 |
| `tokenme pricing explain <model>` | 审计定价来源 |

## 支持的工具与实时配额

配额条来自各工具自己的接口或本地凭据（只读探测，绝不刷新或代替你的登录态）。宿主应用退出后自动暂停对应工具的探测、保留最后数值，重新启动即恢复——设置里的"退出后暂停配额"开关可控制这一行为。

以下工具全部内置适配器——共 20 款，直接从各工具自己落盘的日志与数据库建立索引：

| 工具 | 实时配额 | 兼容性测试 | 平台验证 |
| :--- | :--- | :--- | :--- |
| <img src="crates/usage-core/assets/claude.png" width="20" alt=""> **Claude Code** | 5 小时/周期窗口 — 官方后端 | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/codex.png" width="20" alt=""> **Codex** | ChatGPT 速率窗口 — 官方后端 | ✅ | macOS ✅ · Windows ✅ |
| <img src="crates/usage-core/assets/opencode.png" width="20" alt=""> **OpenCode** | Go 订阅三窗口（美元额度） | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/pi.png" width="20" alt=""> **Pi** | — | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/cline.png" width="20" alt=""> **Cline** | 账号窗口限额 — Cline 账号 API | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/zcode.png" width="20" alt=""> **ZCode** | 5 小时/周期窗口 + 会话预算 — IDE 同款配额接口 + cookie 本地解密 | ✅ | macOS ✅ · Windows ✅ |
| <img src="crates/usage-core/assets/qoder.png" width="20" alt=""> **Qoder** | 订阅额度 — 官方用量接口，本地加密快照兜底 | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/antigravity.png" width="20" alt=""> **Antigravity** | 自算配额 — 调用其 CLI 的 `/usage` | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/agnes.png" width="20" alt=""> **AgnesCode** | 会员点数 — 需注入一次令牌（见下） | ✅ | macOS ✅ · Windows ✅ |
| <img src="crates/usage-core/assets/atomcode.png" width="20" alt=""> **AtomCode** | CodingPlan 配额 — 本地守护进程 | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/workbuddy.png" width="20" alt=""> **WorkBuddy** | 会员点数 — 桌面 app 同款计费接口 / 本地 broker | ✅ | macOS ✅ · Windows ✅ |
| <img src="crates/usage-core/assets/hermes.png" width="20" alt=""> **Hermes** | — | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/funide.png" width="20" alt=""> **FunIDE** | GLM 套餐点数 — 云端点数接口 | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/catpaw.png" width="20" alt=""> **CatPaw** | 积分余额 — 积分门户接口 | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/dsh.png" width="20" alt=""> **DSH** | DeepSeek 账户余额 — 官方 user/balance + 桌面端凭据文件 | ✅ | macOS ✅ · Windows ✅ |
| <img src="crates/usage-core/assets/crow5.png" width="20" alt=""> **Crow5** | — | ✅ | macOS ✅ · Windows ⏳ |
| **Mimocode** | — | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/cola.png" width="20" alt=""> **Cola** | 套餐配额 — 官方 billing 接口，本地凭据解密 | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/joycode.png" width="20" alt=""> **JoyCode** | IDE 点数 — 读取 IDE 自身登录态 | ✅ | macOS ✅ · Windows ⏳ |
| <img src="crates/usage-core/assets/trae.png" width="20" alt=""> **Trae** | 订阅配额 — 官方 v1 接口，本地凭据解密 | ✅ | macOS ✅ · Windows ⏳ |

> **兼容性测试**：每款适配器都带夹具测试套件，CI 每次提交全量运行；每项接入落地前都用真实机器的数据验证过。
>
> **平台验证**：随实机验证进度更新。⏳ 表示该平台的路径已实现、但尚未在实机上跑通；验证通过后更新标记即可。
>
> 仅配额的来源：**Copilot**（高级请求额度，官方后端）只探测配额、不索引用量；**Gemini CLI** 的探测刻意关闭——其落盘 OAuth 令牌会过期，而刷新凭据超出了只读探测的边界。

> **AgnesCode 令牌注入**：其登录令牌只存在于应用内存，无法静默读取。登录 agnescode.agnes-ai.cn 后从请求头取 `access_token`，写入 `~/.config/tokenme/agnes.token`（或设置 `AGNES_TOKEN`）即可启用。

完整参考：**[docs/COMMANDS.md](docs/COMMANDS.md)** · 场景指南：**[docs/USER_GUIDE.md](docs/USER_GUIDE.md)** · 架构总览：**[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** · 适配器逆向备忘：[docs/internal/ADAPTERS_DESIGN.md](docs/internal/ADAPTERS_DESIGN.md)

## 数据与隐私

只索引计数，不碰内容——你的代码和 Prompt 不会被存储，入库的只有每次请求的 token 计数、模型名与项目路径。扫描全程只读，无遥测、无第三方代理。索引库位于系统数据目录（macOS 为 `~/Library/Application Support/tokenme/`）；各工具日志路径见[用户指南](docs/USER_GUIDE.md)。

## 社区

| | |
| :--- | :--- |
| 问题反馈 | [GitHub Issues](https://github.com/sp/tokenme/issues) |
| 微信交流群 | <img src="docs/wechat-group.png" width="180" alt="TokenMe 微信交流群"> |
| 邮箱 | 面板「设置 → 联系我们」直达（地址在 `apps/tokenme-bar/src/lib/about.ts`） |

## 许可证

基于 [MIT](LICENSE) 协议开源。

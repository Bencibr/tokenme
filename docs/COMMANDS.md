# TokenMe CLI 参考

> 版本：0.1.8 · 更新：2026-10-13 · 读者：CLI 用户 · 事实来源：`crates/usage-cli/src/args.rs`（已对照 `tokenme --help` 核实）· [English](COMMANDS_EN.md)

所有命令共享以下全局选项（在命令行上任意位置皆可）：

| 选项 | 作用 |
| :--- | :--- |
| `--tool <ID>` | 只处理指定来源，可重复（`claude`、`codex`、`cline`…） |
| `--json` | stdout 输出机器可读格式 |
| `--offline` | 完全不碰网络，用缓存或内置价格表 |
| `--pricing-override <MODEL=IN/OUT/CC/CR>` | 强制指定价格（可重复），如 `glm-5.2=2/8/2.5/0.4`，单位美元 / 1M tokens |
| `--since <YYYY-MM-DD>` / `--until <YYYY-MM-DD>` | 裁剪时间窗 |
| `--db <PATH>` | 指定索引数据库 |
| `--no-ingest` | 只读现有索引出报告，不重新扫描 |
| `-q` / `-v` | 安静 / 详细 |

## detect

列出本机找到的每一个受支持的 AI 工具、它的日志根目录与索引状态。

```bash
tokenme detect
```

## daily / weekly / monthly

按日/周/月汇总：会话数、token 分级、费用与缓存命中率。

```bash
tokenme daily --days 30
tokenme weekly --weeks 8
tokenme monthly --months 6
```

| 参数 | 默认 | 说明 |
| :--- | :--- | :--- |
| `--days/--weeks/--months <N>` | 14 / 12 / 12 | 最多 90 期，超出自动截断 |

## report

单个时间窗按工具、模型、项目、MCP 服务器或技能拆分，并带上与上一窗的差值。

```bash
tokenme report --window week --group model
```

| 参数 | 取值 |
| :--- | :--- |
| `--window` | `day`（默认）、`week`、`month` |
| `--group` | `tool`（默认）、`model`、`project`、`mcp`、`skill` |

## sessions

最近的会话列表，逐会话汇总。

```bash
tokenme sessions --limit 20
```

## quota

各来源最近一次上报的实时配额样本：滚动 5 小时池、周窗口、积分套餐与重置倒计时。

```bash
tokenme quota
```

应答由一个共享的 5 分钟 TTL 缓存（`crates/usage-quota/src/lib.rs::TTL`；OS 配置目录下每个工具一个 JSON 文件）。这条命令最多等 30 秒拿实时探测结果——面板只等 5 秒——超时的探测仍会在 6 小时宽限窗内贡献上一次的答案。宿主应用退出后暂停其探测，是面板的 `host_exit_pause` 设置，CLI 不做这件事。

OpenCode 配额只认官方 Go 端点。密钥查找顺序：`OPENCODE_API_KEY`；其次 `opencode.json` 里的 `provider.<id>.options.apiKey`（依次尝试 `$XDG_CONFIG_HOME/opencode/opencode.json`、`~/.config/opencode/opencode.json`，macOS 上还有 `~/Library/Application Support/opencode/opencode.json`，且该 provider 的 `baseURL` 必须是 `https://opencode.ai/zen/go/v1` 才算数）；最后是 `~/.local/share/opencode/auth.json` 里的 API 条目。自定义 provider 地址不作为此配额的凭据。结果包含滚动 5 小时、周与月三个窗口。

Open Design 配额读 `~/.amr/config.json` 的 `profiles.prod`（`OPEN_DESIGN_HOME` 可改）：控制密钥 `vela_ctrl_…` 依次请求 `/api/v1/workspaces/current` 与 `/api/v1/workspaces/<id>/billing/preflight`，得到 Design Plan 的 5 小时与 7 天额度。运行时密钥 `sk-amr-…` 不能读控制面；钱包余额不是窗口，不进入结果。

大多数探测只读凭据。恰好三家会把轮换后的令牌原子性写回各自的存储：**MiniMax Code**（存住的 access token 距过期不足 2 分钟时，用 `auth.json` 里的 refresh token 换新，遵守厂商自己的锁）、**Kimi Code**（只在 `/usages` 拒绝存量令牌后才换新）、**Cline**（在 gateway 令牌到期前轮换）。其余探测一律不写——磁盘上的 WorkBuddy 凭据来自你亲自跑过的 `tokenme workbuddy-login`，不是探测写的。

## dsh-doctor

DSH 精度审计：把桌面应用自己的投影台账（`tokenUsage`）与索引里的数字并排打印，逐会话给出 `ok`、`MISMATCH`、`v3 stream`、`not indexed`、`indexed but no projection` 之一的 `status`。备注里是如实偏差的结构性原因——继承的 v4 之前事件、活在 `tokenUsage` 之外的子代理开销。无参数。

```bash
tokenme dsh-doctor
```

## codex-doctor

Codex 精度审计：每份 rollout 从头重析（游标归零、不用 manifest、不去重）后按会话聚合，与索引里的总数并排。Codex 没有厂商侧台账，重放就是独立的事实——一行 `MISMATCH` 意味着索引器漏记或重复计数。无参数。

```bash
tokenme codex-doctor
```

## workbuddy-login

为配额探测登录国内版 WorkBuddy：向厂商要一个 state，在浏览器里走腾讯 SSO，然后轮询最多 5 分钟等令牌。存下的凭据落在 OS 配置目录的 `workbuddy-auth.json`（探测只读它，绝不刷新）；超时则什么都不存。无参数。凭据的域决定版本——国内登录永远不会喂给国际版 WorkBuddy AI 行。装了桌面客户端的机器通常无需额外登录。

```bash
tokenme workbuddy-login
```

## pricing

价格出处：models.dev 上同一个模型常由多家 provider 出售，费用数字只有连同胜出的那条挂牌一起看才有意义。

```bash
tokenme pricing explain deepseek-v4-flash   # 谁的挂牌定了价，输家是谁
tokenme pricing contested --limit 25        # 多家在卖、钱数可观的模型
```

## budget

针对 tokenme 自己算出的费用设支出帽，给厂商不公布上限的工具用。

```bash
tokenme budget list
tokenme budget set zcode --daily 5 --monthly 50
tokenme budget rm zcode
```

单位是美元。`0` 只清空对应的那个窗口。

## index

显式驱动索引。

```bash
tokenme index --status              # 只看状态，不扫描
tokenme index --rebuild             # 丢弃并重读保留窗口
tokenme index --prune               # 清掉早于保留窗口的事件
tokenme index --force               # 从死进程手里夺回入库租约
```

每条命令打开索引时都会校验镜像（`PRAGMA quick_check`）。损坏的镜像不是一个要你处理的错误：它被挪成 `index.db.corrupt-<timestamp>`（连同 `-wal` / `-shm`，重建不会继承），原地换上一个全新空索引，stderr 打一行 `warning:`，下一轮入库从工具自己的日志重新灌满。完全打不开的文件同样被就地隔离，静默进行，不挡命令。

## icons

各工具行在 macOS 上会显示的应用图标，以 data URL 形式输出（面板经 IPC 索要的就是它），写入 `icons.json` 供审阅。

## export

把本机一段窗口的事件导出成同步 bundle（gzip 压缩的 JSONL + 一份带 sha256 与逐工具合计的清单），供别的机器导入。默认落在 `~/tokenme-sync`——菜单栏引擎自动合并的目录——所以 Linux 采集端的全部写入就是一行定时器。导出前先入库最新事件（与所有上报命令相同；`--no-ingest` 可跳过）。安静或无日志的机器也会得到一个合法的空 bundle（`rows 0`）——采集端的定时器绝不在产出少的日子里失败。

```bash
tokenme export                          # 最近 30 天 → ~/tokenme-sync/tokenme-<host>.jsonl.gz
tokenme export --days 400               # 首次回填：整个保留窗口
tokenme export --out /tmp/share         # 放到别处（Syncthing 目录、U 盘）
tokenme export --origin workstation-01  # 固定身份（默认：主机名）
```

`.gz` 经 `*.tmp` + rename 写出，清单最后落盘，传输永远看不到写了一半的 bundle。重复导出同一窗口是安全的：导入幂等。unix 上两个文件一律 `0600`——bundle 记着会话、项目与本地日志路径，默认私有。

## import

把别的机器导出的 bundle 合并进本地索引。先对照清单验证 sha256 与每个批次的合计，任何失败整体回滚、旧数字原样保留。重复导入同一文件不产生任何变化。

```bash
tokenme import ~/tokenme-sync/tokenme-build-01.jsonl.gz
tokenme import bundle.jsonl.gz --dry-run   # 解析 + 合并 + 对账，然后回滚
```

菜单栏这边通常用不着它：面板引擎每轮自动合并 `~/tokenme-sync`，并按机器显示 **多端同步 / Multi-device sync** 徽标与最近一次合并（超过 24 小时转警示色）。

限制与信任：payload 行数有上限（清单计数加余量），坏文件撑不爆内存；bundle 只能让行向前走（增长谓词拒绝更低的计数），从不删除（采集端后来清理的行会被报告为 stale）。bundle 只经验证、不签名：能写进同步目录的人就能添加行——把它当索引本身对待。任务指南：[USER_GUIDE.md](USER_GUIDE.md) §4。

## 存储位置

| 数据 | macOS | Linux | Windows |
| :--- | :--- | :--- | :--- |
| SQLite 索引 | `~/Library/Application Support/tokenme/index.db` | `~/.local/share/tokenme/index.db` | `%LOCALAPPDATA%\tokenme\index.db` |
| 设置 | `~/Library/Application Support/tokenme/settings.json` | `~/.config/tokenme/settings.json` | `%APPDATA%\tokenme\settings.json` |
| 配额缓存 | `<config dir>/tokenme/quota/*.json` | 同一模式 | 同一模式 |

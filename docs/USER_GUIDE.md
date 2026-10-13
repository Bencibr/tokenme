# TokenMe 使用指南

> 版本：0.1.8 · 更新：2026-10-13 · 读者：日常用户 · 面板与 CLI 都在覆盖范围内 · [English](USER_GUIDE_EN.md)

## 功能清单

| 能做什么 | 在哪用 |
| :--- | :--- |
| 看今天/本周/本月/今年的 token、费用、缓存率 | 面板 → 概览（右滑翻上周、上月、去年；「活动」页签是全年热力图） |
| 看每款 AI 编程工具的用量明细 | 面板 → 工具（27 款，一行一个，行配色取自应用图标） |
| 找出最贵的会话 | 面板 → 排行 |
| 按模型、项目查单次请求 | 面板 → 明细 |
| 测 TTFT（首 token 延迟）与 TPS | 面板 → 速度（被动测量；Codex / Claude Code 可主动发起一次短请求实测） |
| 看各家订阅剩余额度与重置倒计时 | 面板 → 概览下方的配额条（窗口跨 80% 或耗尽时弹系统通知） |
| 每日签到一键代领 | 面板 → 工具页行头（Trae CN、Qoder、Qoder CN、MiniMax Code、WorkBuddy 国内版） |
| 把服务器、容器上的用量汇总进来 | 面板底栏「服务器」一键部署，或 Linux 采集端手动安装（见 §4） |
| 给无限额的工具设成本上限 | `tokenme budget set`（见 §5） |
| 在菜单栏常驻数字或图标 | 托盘显示模式，六种任选（见 §3） |
| 数据不出本机 | 全程只读索引，无遥测、无第三方代理（见 §7） |

## 1. 安装

macOS（Homebrew，推荐）：

```bash
brew tap Bencibr/tokenme https://github.com/Bencibr/homebrew-tokenme
brew trust bencibr/tokenme        # 一次即可——brew 6 对第三方 cask 有信任确认
brew install --cask tokenme       # 菜单栏面板
brew install tokenme-cli          # CLI
```

Windows：到 [Releases](https://github.com/Bencibr/tokenme/releases/latest) 下载 `TokenMe_<v>_x64-setup.exe`。安装器同时装面板和 CLI，CLI 位于 `<安装目录>\bin\tokenme.exe`，安装时自动加入 PATH（"只为我"加用户 PATH，"所有用户"加系统 PATH）；装完开一个新终端跑 `tokenme --help` 即可。升级沿用同一条 PATH；卸载只删自己这次装的条目。

Linux（无头采集端）：下载 `tokenme-cli-<v>-<arch>-linux-musl.tar.gz`（完全静态，机器上不需要 Rust/Python）：

```bash
tar xzf tokenme-cli-<v>-x86_64-linux-musl.tar.gz && cd tokenme-cli-<v>-x86_64-linux-musl
./install-linux.sh --every 15 --push you@display-host
```

这一行把二进制装进 `~/.local/bin`，并装一个每 15 分钟导出一次的定时器，把 bundle scp 到展示机的 `~/tokenme-sync`；展示机面板下一轮自动合并，显示在「多端同步」徽标里。定时器是 systemd `--user` 单元，在没有 user manager 的机器（容器、裸 ssh）上自动退化为等价 cron——`loginctl enable-linger` 打开后无需登录会话也会跑（无头机器上安装器会把要跑的那行命令打印出来）。`--uninstall` 移除二进制、生成的脚本与定时器，数据（索引、bundle）保留。不加 `--push` 的话，`~/tokenme-sync` 目录随你怎么搬（Syncthing、挂载、每夜 rsync 都行）——bundle 的合并是幂等的。

macOS 面板也可从源码构建（ad-hoc 签名，首次打开右键 → 打开）：

```bash
./scripts/build-macos.sh          # 产出 TokenMe.app 与拖拽安装的 DMG
```

## 2. 首次运行

```bash
tokenme detect        # 1. 看本机装了哪些工具、日志在哪
tokenme daily --days 7
tokenme quota
```

## 3. 菜单栏面板（macOS · Windows）

macOS 由 `scripts/build-macos.sh` 构建，Windows 由 `apps/tokenme-bar` 下的 `pnpm tauri build --bundles nsis` 构建。左键点双环图标开关面板；右键打开菜单（刷新、数据目录、开机自启、退出）。

### 3.1 五个页面

- **概览**——当天的 token、费用与缓存命中率，按小时分布的用量柱状图；「活动」页签是 GitHub 式的年度热力图，右滑翻看上周、上月与去年。下方是各家配额条与通知。
- **速度**——TTFT（首 token 延迟）与 TPS 的被动测量：Codex 从本地 rollout 流读取，Claude Code 走 OpenTelemetry span，DSH 读本地会话日志；Codex 与 Claude Code 还能主动发起一次短请求实测。
- **工具**——每个工具的请求数、会话数、token 量与缓存率，一行一个，行配色取自应用图标；支持签到的工具在行头一键领取。
- **排行**——按会话排名，一眼找出最贵的那几个。
- **明细**——单次请求级的记录，可以按模型、项目筛选。

### 3.2 托盘显示模式

面板底栏的显示芯片每点一次切换一格：

| 模式 | 菜单栏显示 |
| :--- | :--- |
| 仅托盘 | 只显示图标 |
| 仅 Token | token 总数，无图标 |
| 仅花费 | 花费，无图标 |
| 托盘·Token | 图标 + token（默认） |
| 托盘·花费 | 图标 + 花费 |
| 托盘·Token·花费 | 图标 + token + 花费 |

### 3.3 设置

底栏齿轮打开设置面板（固定为面板高度 65%，内部滚动，底部 版本 · 检查更新 · 退出 常驻）。四个页签：**通用**（自动刷新节奏、外观、金额折算、显示零会话工具）、**提醒**（系统通知开关、提醒档位、静默工具）、**高级**（自动探测配额、逐工具探测、开机自启、自动检查更新、服务器与同步）、**关于**。`›` 表示在本面板内展开子页，`↗` 表示跳往别的界面，`‹ 返回` 返回。总闸关闭时，子页里的开关全部置灰并说明原因，而不是摆着看似可用的假开关。

### 3.4 刷新与探测开关

刷新节奏控制索引重扫；厂商配额的应答缓存 5 分钟，探测保持克制。**探测完全由你控制**：自动探测配额是总闸，逐工具探测是分开关。关掉的探测不发请求，但缓存仍被读取——那行保留最后一次答案而不是消失；总闸关闭时配额区直接写明 **已停止探测 · 数据停在 N 分钟前**，不假装数字是新的。逐工具列表默认只列本机真的应答过的探测（显示全部可看从未应答过的）；Copilot、Gemini CLI、Kimi Code、MiniMax Code 这四个没有菜单栏进程可依托，想停它们只能从这里，行上标着 无退出门。两个开关都不会触发强制刷新——强制会扔掉你刚选择保留的答案。重新打开后 60 秒内的下一轮自动恢复，或点刷新立即生效。底栏刷新按钮是全量刷新：重扫日志、绕过缓存强制探测全部厂商、拉取新价格表。底栏左角的 **最后刷新 · N 秒前** 说明屏上数字的新旧。

### 3.5 OpenCode 官方配额

TokenMe 只读取官方 OpenCode Go provider 的配额：优先 `OPENCODE_API_KEY`，其次 `opencode.json` 里的 `provider.<id>.options.apiKey`（依次尝试 `$XDG_CONFIG_HOME/opencode/opencode.json`、`~/.config/opencode/opencode.json`，macOS 上还有 `~/Library/Application Support/opencode/opencode.json`；该 provider 的 `options.baseURL` 必须是 `https://opencode.ai/zen/go/v1`），最后回退 `~/.local/share/opencode/auth.json`。自定义代理地址不会被当作官方配额凭据。OpenCode 行显示 5 小时、7 天与月度三个窗口。行没出现时，依次检查 高级 → 自动探测配额、逐工具探测的 OpenCode 开关，以及 `panel.log` 的 quota poll 记录。

### 3.6 令牌自动续期

MiniMax Code（约 1 小时）、Kimi Code（厂商 15 分钟一轮）、Cline（gateway 令牌一小时）三家把短时效令牌存在磁盘上，tokenme 在其过期时就地续期：拿同一份凭据文件里的 refresh token 去换新对，再按该工具自己的格式原子性写回原处。写回是安全性的前提——没有落盘的轮换等于已被消耗的轮换，工具自己下次刷新就会发现登录死了。每个探测只续自己正在读的那份登录；静态 API key 只读不写。换取失败则什么都不改，该行最多保留六小时（探测缓存的宽限）的最后真实数字。其余所有探测只读凭据、绝不写回——tokenme 存的那份 WorkBuddy 凭据来自你亲自跑过的 `tokenme workbuddy-login`，不是探测写的。

### 3.7 WorkBuddy 双版本

国内版 WorkBuddy（`workbuddy_cn`）读 `~/.workbuddy`，国际版 WorkBuddy AI（`workbuddy`）读 `~/.workbuddy-ai`，用量、配额缓存与宿主退出门控互相独立。国内版轮询会检查每日活动并代领符合条件的日子；开启 高级 → 自动签到 后，关掉 WorkBuddy 客户端、只要 TokenMe 在跑就继续领。总闸与逐工具开关照常止住请求。已确认的日子按账号记住，北京时间零点过期。

### 3.8 额度横幅

窗口用到 **80%** 的那一刻弹一条系统通知（`Qoder · 5 小时` · 已用 83%），用尽（100%）再弹一条。只有厂商真正计量的窗口才有横幅；§5 的自设预算只在面板里显示。**提醒档位**可选 80% 与 100% / 仅 100% / 关闭；**静默工具**单独静音某些窗口——静音期间档位照常记录，取消静音不会补发错过的横幅。横幅标记的是"越过"而非"状态"：每窗口每实例只发一次，事实持久化（索引旁的 `notify-state.json`），窗口重置后静默重新待命。面板退出期间不探测，错过的越过不补发。第一条真正到期的横幅会带出一次系统通知权限请求；之后收不到就去系统设置开（macOS 系统设置 → 通知 → TokenMe）。macOS 上配额区会自己给出引导行：**开启通知** / **去开启** / 忽略。裸 `cargo run` 没有应用身份，永远弹不出横幅（日志里会说明）。

### 3.9 数据的新旧标识

底栏左角常驻 **最后刷新 · N 秒前**；连续五个节奏没有新发布时，它换成 **数据已停止更新** 并注明最后一次的时间——引擎会自行重启循环（退避，原因在 `panel.log`）。面板刚启动、还在显示上一次运行恢复的报告时，同一位置显示的是 **上次扫描的数据** 加数据实际年龄——引擎活着，正在扫，这不是"停了"。

### 3.10 多端同步徽标

有 Linux 机器同步进来后，数字下方出现 **多端同步** 徽标：最新合并的机器名、时间与行数，超过 24 小时未合并转为警示色。悬停可看每台机器的窗口与文件名。没有导入过任何 bundle 就没有徽标。

### 3.11 机器切换

一台以上机器报数时，头部下拉把整个面板——每个周期、模型与项目排行、调用序列、会话列表——折叠到 **全部机器**、**本机** 或按名字选任意一台远程机器。这是对同一份索引的重新汇总，瞬间完成：全部 = 各机之和。触发行显示当前范围：全部机器 带最多三枚机器色点与远程占比（远程 42%），单机带自己的色点、占比与最近合并时间。有机器超期时 全部机器 行出现警示标记。没有任何导入时这个控件整个不渲染——单机面板与从前一模一样。

### 3.12 启动首屏

面板能在可绘制后约一秒出数字：第一帧是上一次运行自己发布的报告（索引旁的 `report.json`，3–22 ms 读回，不扫描、不探测、不查询）。它如实标注身份——左角 **上次扫描的数据** 芯片带数据实际年龄，配额区显示 探测中 而非上一次的窗口。缓存报告只在能如实描述时才被使用（同机器范围、同一本地日、一小时以内、非恢复写入）；全新安装或隔夜间隙没有可恢复的，等待分两层：窗口原生底上的一枚转圈加一行 正在索引本机用量…（颜色随 外观），页面接管后是 `index.html` 自己的加载卡。`panel.log` 把里程碑分得清清楚楚：`the page booted N ms after launch` 是 bundle 跑起来，`the page reported content N ms after launch` 是数字上屏，`boot: restored …` 说明这次启动有没有可恢复的报告。

## 4. 多机汇总（Linux 采集端 → macOS/Windows 面板）

任何机器的数字都能在同一个面板相遇。**采集端**（完全静态的 Linux CLI）把索引的一个窗口导出成文件；每台**展示机**（macOS / Windows 面板）自动合并这个目录。通道是你已经信任的任何文件拷贝——tokenme 不开端口、没有自己的账号。

采集端最短路径（一行，同 §1）：

```bash
./install-linux.sh --every 15 --push you@display-host
```

定时器（systemd `--user`，cron 兜底）每 15 分钟导出 `~/tokenme-sync/tokenme-<host>.jsonl.gz` 并 scp 到展示机的 `~/tokenme-sync`。没有推送目标就去掉 `--push`，目录随你搬（Syncthing、挂载、每夜 rsync）；也可以完全跳过安装器手动跑 `tokenme export`（默认最近 30 天导到 `~/tokenme-sync`）。安静的一天照样产出合法 bundle（rows 0），定时器不会因为没数据而失败。

展示侧零配置：面板每轮自动合并 `~/tokenme-sync` 里的 `tokenme-*.jsonl.gz`，徽标（§3.10）显示每台机器的最新合并。合并幂等——重复导入同一文件不产生任何变化。想预演或手动：`tokenme import <bundle>`（加 `--dry-run` 解析对账后回滚）。

**面板可以替你跑完 SSH 这半边。** 点面板底栏的服务器图标添加机器：地址、用户名、SSH 密码输一次（私钥文件也行，都支持 `~`）。向导首次连接即钉住主机指纹、生成专用密钥对、把公钥装进远端 `~/.ssh/authorized_keys`、上传静态采集器到 `~/.tokenme/bin/`、跑首次导出并拉取合并——整个过程是一条带分步日志的引导流。此后面板每 15 分钟（可按服务器配置）重新拉取，新的合并像任何文件来源一样按机器出现。密码只用于那一次安装连接，永不落盘。删除服务器时两端一起清理：远端的采集器二进制与密钥行被删除，本地钉住的指纹与历史被丢弃。

**安全与边界**

- 无监听、无云端、无同步账号：认证完全由你选的通道承担（scp 的 ssh 密钥、Syncthing 的 TLS + 设备 ID）。tokenme 不存任何同步凭据。面板内 SSH 流程保持同一性质：一次密码只认证一次安装连接，主机指纹首连即钉（面板里可见），长期信任只是远端 `authorized_keys` 里的一行专用公钥——删掉那行或在面板移除服务器即吊销。
- 每次导入先验证后落库：bundle 的 sha256 对清单、批次 token 和对清单、格式与 schema 版本门。合并在单一事务里——任何失败整体回滚，文件保留在磁盘，什么都不进。
- 行数有上限（清单计数加余量），坏文件撑不爆内存。bundle 只能让行向前走（增长谓词拒绝更低计数）；导入从不删除——采集端后来清理的行会被报告为 stale，不会消失。
- unix 上 bundle 与清单一律 `0600`。bundle 携带的与索引一致——token 计数、模型/会话/项目名、本地日志路径——绝无代码、Prompt 或凭据。把 `~/tokenme-sync` 当索引本身对待：能写进这个目录的人（你的账号，或你共享给的任何账号）就能注入用量行——合并去重，但不签名。

## 5. 预算

厂商不公布上限的工具（Cline、ZCode 等），自己设帽——tokenme 按自己的费用数学衡量并画出一根配额条：

```bash
tokenme budget set zcode --daily 5 --monthly 50
```

## 6. 支持的工具与数据源

| 工具 | 计量 | 本地数据源 |
| :--- | :--- | :--- |
| Claude Code | Tokens | `~/.claude/projects/**/*.jsonl` |
| Codex | Tokens | `~/.codex/sessions/**/rollout-*.jsonl` |
| OpenCode | Tokens | `~/.local/share/opencode/opencode.db` |
| Pi | Tokens | `~/.pi/agent/sessions/**` |
| Oh My Pi | Tokens | `~/.omp/agent/sessions/**`（`OMP_AGENT_DIR` / `OMP_CODING_AGENT_DIR` 可改） |
| Cline | Tokens | `~/.cline/data/sessions/**`（桌面版把子代理的转录写在父会话目录内、元数据写在旁边的 `<session>__agent_<uuid>/`，各自归到自己的工作区） |
| ZCode | Tokens | `~/.zcode/cli/db/db.sqlite` |
| Antigravity CLI | Tokens | `~/.gemini/antigravity-cli/conversations/*.db` |
| Qoder CN | Credits + Tokens | `~/.qoder-cn/projects/**/*.jsonl`（credits）· `~/Library/Application Support/QoderCN/SharedClientCache/cache/db/local.db`（tokens）；独立账号、配额与每日签到 |
| Qoder | Credits + Tokens | `~/.qoder/projects/**/*.jsonl`（credits）· `~/Library/Application Support/Qoder/SharedClientCache/cache/db/local.db`（tokens） |
| WorkBuddy AI | Tokens + credits | `~/.workbuddy-ai/projects/**/<session>.jsonl` |
| WorkBuddy（中国版） | Tokens + credits | `~/.workbuddy/projects/**/<session>.jsonl` |
| AgnesCode / AtomCode / Crow5 / Mimocode / Cola / DSH | Tokens | 自动发现，与其同形 |
| Kimi Code | Tokens | `~/.kimi-code/sessions/**/wire.jsonl`（旧 `~/.kimi`；桌面端内嵌 runtime 的 `~/Library/Application Support/kimi-desktop/daimon-share/daimon/runtime/kimi-code/home` 也在读取范围） |
| MiniMax Code | Tokens | `~/.minimax/v2/sessions/**/messages.jsonl`（旧 `~/.mavis`） |

只有云端记录、本地没有逐请求日志的工具（Cursor、VS Code Copilot）本地不支持。

## 7. 隐私

- 入库的只有每次请求的 token 计数、模型名与项目路径——绝不碰你的代码和 Prompt。
- 扫描全程只读：探测读凭据，会把续期后的凭据写回的只有 §3 点名的那三家，且各自只写自己的存储。无遥测、无第三方代理。
- 网络调用只有三处：对你自己厂商账号的实时配额探测、models.dev 价格表、以及你亲自点击的面板内邮件/发布链接。
- 多机同步（§4）在你的机器之间搬运的也只有这些元数据——bundle 里没有代码、Prompt 或凭据——通道由你选择（ssh/scp、Syncthing、挂载盘）。tokenme 不开端口、不存同步凭据，每个 bundle 合并前都过验证（sha256 + 计数 + schema）。

## 8. 故障排查

| 症状 | 处理 |
| :--- | :--- |
| `no supported AI tool logs found` | 先跑一次你的工具，再 `tokenme detect` |
| 费用不对 | `tokenme pricing explain <model>` 看哪条价格记录赢了；`--pricing-override` 可覆盖 |
| 数字像是旧的 | `tokenme index --status`；可能有另一个 tokenme 进程占着 ingest 租约——`tokenme index --force` |
| 某条配额不动了 | 设置 → 高级 → 自动探测配额（总闸）或 逐工具探测 关掉了它：闸关闭时该行显示的是缓存的最后答案；总闸关闭时整个区域显示 **已停止探测 · 数据停在 …**。重新打开开关，60 秒内的下一轮或刷新按钮即恢复 |
| 底栏左角显示 **数据已停止更新** | 引擎没有发布，屏上是上一轮的快照——五个节奏没有发布会自动换上并带时间。面板会自行重启循环（退避，原因在 `panel.log`）；点刷新可强制一轮，徽标超过一分钟还在就读 `panel.log`。别与 **上次扫描的数据** 芯片混淆：那颗说明引擎活着还在扫，显示的是上一次运行发布的报告 |
| 索引损坏 | `tokenme index --rebuild`（旧文件自动隔离） |
| 面板没数据 | 先确认 `tokenme detect` 能找到来源；面板读的是同一份索引 |
| 启动后白卡几秒 | 通常不会：第一帧读的是 `report.json` 里的上次报告，页面能画时数字就在。真看到白卡——全新安装，或没有可恢复的报告——那是窗口的**原生底**（浅色下白、深色下黑），不是页面没画出来；`panel.log` 会写明耗时在哪一段 |
| 面板出现在屏幕角落而不是菜单栏下 | 那次打开既没有托盘矩形、会话也没暴露显示器——锁屏或显示器睡眠时的重启（`panel.log` 写着 `no monitor to anchor to`）。下次打开会正常锚定 |
| 额度横幅收不到 | 先允许 TokenMe 发通知（macOS 系统设置 → 通知 → TokenMe；Windows 设置 → 系统 → 通知）——被拒会在日志留一次 `notify: [<window>] blocked`。用 `TOKENME_TEST_NOTIFY=1` 重启面板可验证链路：`panel.log` 答 `notify test: posted` / `blocked` / `permission was just requested` |
| 同步徽标过期或缺失 | 采集端：`systemctl --user list-timers tokenme-export.timer` 看定时器活着没，手动 `tokenme export`；展示机：bundle 必须落在 `~/tokenme-sync`——`tokenme import <file> --dry-run` 可预演合并 |
| bundle 被拒 | 导入消息里写明原因（sha256 不符 = 传输损坏或截断；格式/schema 不符 = 两端版本差）。文件保留、什么都没合——修好通道或升级旧的一端，它会自己重试 |

## 桌面宠物问候（Windows）

通用设置里有宠物问候、休息提醒与动作的独立开关，外加提醒间隔和 打招呼 预览。问候跟随本地时间：早晨 06:00–11:00、中午 11:00–14:00、晚间 18:00–22:00、22:00–06:00 是轻声的夜间问候。每个时段每个本地日只问候一次；午夜前后算同一个夜晚。

休息提醒默认 60 分钟活跃使用。三分钟空闲、锁屏、宠物隐藏或睡眠都会重置计数。全屏时自动投放等待，22:00–06:00 休息提醒保持安静。休息 5 分钟暂停计数；10 分钟后再提醒推迟下一次。问候卡自动关闭，待在宠物旁边不抢焦点；既有的点击打开、拖拽与边缘停靠都还在。打招呼是显式交互，全屏下也能用；锁屏与宠物总开关照常生效。

现有可见皮肤都有瞬态的问候、跳跃、伸懒腰、犯困与落地反应。小猫另有四姿态反应图集：活力打招呼、贴心端茶、困倦哈欠与举手伸展。图集按皮肤可选，由 `pet-skins.json` 的 `reactionAtlas` 声明。动作尊重系统"减弱动态效果"偏好。所有计时与提示都在本地运行，没有任何语言模型调用。

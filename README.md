# tokenme

跨工具的 AI 编码 token / 成本 / 配额监视器：**macOS + Windows 菜单栏程序**（Tauri v2），**Linux 命令行**（`usage-cli`，类 ccusage）。所有数据都从本机工具自己的日志/数据库读取，不上传、不代理、不写回。

```
crates/
  usage-core/          共享契约：Semantics / UsageEvent / TokenCounts / 定价 / 报表
  usage-index/         SQLite 增量索引 + 文件监听
  usage-quota/         配额探针（厂商接口 + 本机凭据 + 本地预算表）
  usage-cli/           detect / daily / report / quota / index 子命令
  adapters/            一源一 crate，静态注册在 adapters/all
apps/tokenme-bar/      Tauri v2 菜单栏程序（React + TS）
```

## 支持的数据源

`cargo run -p usage-cli -- detect` 会逐个探针列出实际路径。语义（per-call 还是累计、token 还是 credits、模型是否随行）写在每个 adapter 的 `semantics()` 里，索引层据此决定如何去重与合并。

| id | 工具 | 本机数据源 | 计量 |
| --- | --- | --- | --- |
| `claude` | Claude Code | `~/.claude/projects/**/*.jsonl` | tokens |
| `codex` | Codex | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`（含 `rate_limits`） | tokens |
| `opencode` | OpenCode | `~/.local/share/opencode/opencode.db` | tokens |
| `pi` | Pi | `~/.pi/agent/sessions/**` | tokens |
| `cline` | Cline | `~/.cline/data/sessions/**` | tokens |
| `zcode` | ZCode | `~/.zcode/cli/db/db.sqlite` → `model_usage` | tokens |
| `qoder` | Qoder | `~/.qoder/projects/**/*.jsonl` → `usage.credits` | **credits** |
| `antigravity` | Antigravity CLI | `~/.gemini/antigravity-cli/conversations/*.db`（protobuf blob） | tokens |
| `ccswitch` | CC Switch 网关 | `~/.cc-switch/cc-switch.db` → `proxy_request_logs` | tokens |
同方言不同产品坐标的三个源：`crow5`（`~/.local/share/crow5/` 下**每个**可读的 `*.db` 都是源：`crow5.db` 是历史库、`opencode-powerformer-v<版本>.db` 是升级后的在用车，两边 message id 不相交，按 mtime 只挑一个会静默丢掉 99% 历史）、`mimocode`（`~/.local/share/mimocode/mimocode.db`）都走 OpenCode 的 `message.data` 方言；`cola`（`~/.cola/sessions/*/*.jsonl`）走 Pi 的 `message.usage` 方言。另有两个独立格式：`agnes`（`~/.agnes/data/sessions/sessions.db` → `usage_ledger`，成本列全空、时间戳是秒）、`atomcode`（`~/.atomcode/sessions/*/*.jsonl` → 顶层 `usage.{prompt,completion,cached}`，`prompt` 已含 `cached`）。

### 统计不了的（已实测，不是没做）

- **Cursor / Antigravity IDE / VS Code Copilot**：本地无每请求 token。Cursor 的 `state.vscdb` 会话气泡是加密的、`usageData` 为空，`ai-code-tracking.db` 全表 0 行；VS Code 的 `agentSessionData/*/session.db` 有 `turn_usage` 表但 0 行。这几家的数字只在各家**云端**计量。
- 目录存在但零 usage 载荷：Gemini CLI、grok、hermes、kimi-code、factory(droid)、devin、amp、codebuddy、CatPaw、JoyCode、Tutti。

## 单位口径

库里**只存 token/credits，不存金额**——价格表会变。金额由 `usage-core/pricing.rs` 统一按 `https://models.dev/api.json` 计算（回退到内置快照）。

四个互斥阶段：`input`（净输入，不含缓存）、`cache_creation`、`cache_read`、`output`；`reasoning` 是 `output` 的子拆分，**永不叠加**。各源"input 是否已含缓存"的约定不同，逐个源在 adapter 的模块文档里用数据自身的内部恒等式（累计器 Δ、`totalTokens` 恒等式、裸加总上界）判定，而不是照抄第三方工具。

只报 credits 的源（Qoder）走 `Meter::Credits`，并且按厂商公布的**方案价**折算进同一个金额列：官方三档 Pro $20/2,000、Pro+ $60/6,000、Ultra $200/20,000 都指向 **1 credit = $0.01**（`pricing.rs::CREDIT_USD` 带出处）。这部分金额单列在 `Summary.credit_cost`，UI 用 `≈` 与 hover 标注"由 credits 折算"。Qoder 不公开 credits↔token 的比例（它自己的新库快照里写 `tokenCountsAvailable:false`），所以这里不编造 token 数；它的真实 per-request token 若在 IDE 聊天路径下存在，来自 `SharedClientCache/cache/db/local.db` 的 `chat_message.token_info`。

## 配额

`cargo run -p usage-cli -- quota`。两类来源：日志里自带的窗口（`origin = log`，如 Codex `rate_limits`）与实时探针（`origin = probe`，见 `crates/usage-quota/src/providers/`）。探针全部只读、并行、带 5 分钟缓存与 5 秒预算，任何一个卡住都不会冻住面板；拿不到就不显示，**不会画出 0% 的假条**。

- `codex`：官方接口 + 日志窗口。
- `qoder`：**活接口优先**。`GET https://openapi.qoder.sh/sash/api/v2/me/usage` 就是"我的用量"面板自己发的请求（拆 `app.asar` 可见 `account.getQuotaUsage`，实测 200 / 6 ms），头为 `Authorization: Bearer <token>` + `Cosy-ClientType: 10` + `User-Agent: Qoder`。回答按面板自己的 zh locale 命名四类表：`userQuota` **套餐内 Credits**、`addOnQuota` **资源包**、`dedicatedResourcePackages[]` **专属资源包**（`available:false` 的不画）、`orgResourcePackage` **共享资源包**；`percentage` 在线上是 0..1 的分数，≤1 就 ×100（与厂商自己的归一化一致），标签带 `剩 N/总N`。token 解自 `secret://aicoding.auth.userInfo`（与快照同一个 safeStorage 信封），`expireTime` 过期就放弃活读、**绝不刷新**；0/0 的表（免费档套餐内）不画。快照 `secret://aicoding.auth.creditUsage` 降为离线回退且只认 `total>0`——它**只装套餐内一项**，本机实测还是过期值（`total:0, isQuotaExceeded:true`），而活接口答 `isQuotaExceeded:false` + 资源包 600/已用 130：只读快照时这账号被画成"已用完"，就是改活读的原因。
- `antigravity`：跑它自己的 CLI `agy -p /usage --output-format json` 读 `command.data.groups[].buckets[]`（周/5 小时两组，Gemini 与 Claude/GPT 各一组）。**故意不自己刷 refresh_token**——Google 会轮换它，探针不写回就会把用户登录作废。
- `ccswitch`：网关自己的预算表 `providers.limit_daily_usd` / `limit_monthly_usd`，用量按 `proxy_request_logs` 的本地零点/月首求和。没设限额就没有条。
- `claude`：官方 OAuth 接口。**若 Claude 走 CC Switch 之类的本地网关托管认证，`accessToken` 在本机就是空的**，此时查不到属正常。
- `cline`：对照 cline/cline 源码校准过。套餐名来自 SDK 自己调的 `GET /api/v1/users/me/plan`（`fetchCurrentUserPlan`）：有套餐用套餐名（如 ClinePass Pro），`404 {"error":"no plan history found for user"}`（本机实测，非订阅账号两个 plan 端点都这么答）或 `plan:null` 就显示为 **Free**；窗口来自 `GET /api/v1/users/me/plan/usage-limits`——**注意这个端点不在任何现行 Cline 客户端源码里**（Cline 客户端根本不轮询限额，ClinePass 限额只在撞限时以 `ClinePassLimitError` 之类的错误消息出现），它是第三方（Javis603/token-monitor）实测的口径，解析保持宽容（`data.limits`/`limits` 两种封装都收）。没有窗口数据时出一条 `{套餐名} · 无可查限额` 的状态条（free 档有限额但只在请求时上报，无处可查），而不是让 cline 从面板上消失。鉴权头必须带 `workos:` 前缀——其 vscode account-service 源码注释原话（后端靠它路由 WorkOS 验证）。token 有效性判定同 SDK 的 `deriveCredentialExpiry`：显式 `expiresAt`（毫秒）→ JWT `exp` → 都没有视为失效；`accessToken` 缺失/失效回退 `apiKey`（同 `resolveLocalClineAuthToken`）；路径解析同 `shared/src/storage/paths.ts`（`CLINE_PROVIDER_SETTINGS_PATH` > `CLINE_DATA_DIR` > `~/.cline/data`）。**过期就用它自己的 refresh token 走 Cline 网关轮换**（`POST /api/v1/auth/refresh`，载荷/响应/写回字段与其 SDK 的 `refreshClineToken`+`saveOAuthCredentials` 逐字段一致）：access token 只有 1 小时寿命且 refresh token 轮换式更新，所以**写回 providers.json 是安全的前提**——只写 `accessToken/refreshToken/expiresAt` 三个字段、原子 tmp+rename、保留 0600 权限与其余内容；轮换失败（refresh token 被吊销）不写任何东西，去 Cline 重新登录即可。tokenme 是唯一会写别的工具凭据文件的例外，规则从"绝不写回"改为"**写回才许刷新**"。本机实测：过期 19.5 天全端点 401（旧版只读不刷的现状）；2026-09-24 晚真实过期后自动轮换成功，providers.json 写回新过期时间（+1h），Free 条不再随过期消失。
- `zcode`：**全部走 ZCode app 自己在用的那组活接口，不再读任何缓存**。拆它的 `app.asar` 可见 usage-stats 服务就是三个 GET（它自己的 locale 把这个面板标注为"来自当前供应商额度接口"）：
  ① `GET https://bigmodel.cn/api/monitor/usage/quota/limit`（Z.ai 家族账号是 `https://api.z.ai` 同路径；`ZCODE_BIGMODEL_USAGE_QUOTA_URL`/`BIGMODEL_USAGE_QUOTA_URL` 覆盖，与 app 同名同优先级）→ `{code,data:{level,limits[]}}`，5 小时 prompt 池、每周额度、工具调用月额度全在这里。实测 200 / 亚秒，且 5 小时窗口带真实 `percentage` 与 `nextResetTime`（毫秒）。鉴权用 `~/.zcode/v2/credentials.json` 里 `account-provider:coding-plan:account:<plan>:account:<id>:api-key` 的 coding-plan API key（同 `enc:v1:` 信封），账号的 `oauth:<family>:access_token` 实测同样可用作回退。
  ② `GET {base}/api/biz/subscription/list` → 套餐显示名（`GLM Coding Lite`，取 `status=="VALID"` 的那条）。
  ③ `GET https://zcode.z.ai/api/v1/mcp/usage`（实测 200 / 63 ms）→ `{level, server_time, next_refresh_at, total_usage{used,limit,remaining}}`，"ZCode MCP" 调用表；窗口长度按厂商自己给的刷新间隔算（本机 522 分钟），不猜成"月"。
  窗口命名**跟着厂商文档与 locale 走**（zcode.z.ai/cn/docs/usage-stats：5 小时 prompt 池 / 每周额度 / 工具调用（MCP 每月额度）；locale `entitlementFiveHourUsage`="5 小时剩余"、`entitlementMonthlyMcpUsage`="工具调用"、`entitlementServerMcpUsage`="ZCode MCP"）。ZCode 没有日窗口、也没有泛化的"月 token 窗口"，所以一个都不多画：`TOKENS_LIMIT/CREDIT_LIMIT (unit 3,number 5)`→5 小时、`unit 6`→每周（付费档才有）、`TIME_LIMIT (unit 5,number 1)`→**工具调用**（月度工具/MCP 调用额度，**不是 token 窗口**——旧版把它画成"1 月"就是本条修掉的错误标注）。字段语义（来自 `Resources/app.asar` 的 `JH()`/`XH()`）：`percentage` 已是**已用**份额，IDE 的"剩余环"再拿 100 减它；`usage` 不能当分子分母（工具调用记录里=额度，MCP 记录里=0），兜底只用 `currentValue/(currentValue+remaining)`。认不出的 `(type,unit,number)` 形状宁可不出条。
  历史：这组窗口早先被误认为"没有可查的接口"，于是去扫 IDE 的 Local Storage 快照（`zcode:usage-entitlement:subscription-v2:*`）——那其实是**同一个接口的过期副本**，5 小时窗口的 percentage 常年停在旧值，还把工具调用量错标成"1 月"。现在直读活接口，三条全实时（实测 5 小时窗口在一次会话里从 21% 涨到 43%），`· 缓存 45m` 这类标注随之消失。
  凭据信封：AES-256-GCM，key = sha256(`ZCODE_CREDENTIAL_SECRET` ?? `zcode-credential-fallback:darwin:<home>:<user>`，注意必须是 Node 口径的 `darwin`，不是 Rust 的 `macos`）——同一用户同一台机器读它自己面板已经显示的数，属于静态混淆而非权限边界。**绝不调 `/api/v1/oauth/token`**：刷新会轮换 refresh_token，取到不回写就把用户从自己账号上踢下线；实测两把都是无 `exp` 的长会话 JWT，读表不需要刷新。解不开/被拒（HTTP 200 + `{"code":401}` 无 `data`）就静默返回空，不报错也不假 0。另：`~/.zcode/cli/db/db.sqlite` 的 `session_target(token_budget,tokens_used)` 是每会话 token 预算，NULL 预算不算配额。
- `copilot` / `gemini`：凭据不存在或已过期时明确返回空，原因写在模块文档里。

### 自己设的预算（`tokenme budget`）

厂商不给限额的工具（Cline、ZCode、AgnesCode…）用**你自己设的美元上限**出配额条，钱由 tokenme 自己按同一张价格表算：

```bash
tokenme budget set zcode --daily 5 --monthly 50
tokenme budget list
tokenme budget rm cline
```

写进 `~/Library/Application Support/tokenme/settings.json` 的 `budgets` 键（菜单栏程序读同一个文件的同一个键，下一轮刷新就生效）。这类条目标 `origin = budget`、**每一行**自带 **预算** 角标（组头的角标只说这个工具有哪些来源，行上的角标才回答"这一条是谁"），和 **实测**（厂商接口）、**日志**（记录里自带）三种来源区分开；超过上限就显示真实百分比（如 `540%`），不夹到 100%。

### 配额口径

- **`--until` 回看过去时不跑实时探针**：今天的 5 小时窗口不是那天的窗口，而且探针的采样时间会晚于报表所描述的那一刻，会把"未来的样本"塞进自己的报表里。回看只展示日志里自带的窗口。
- 报表的时间戳取在**探针跑完之后**（`report::instant_after_polling`）：`agy /usage` 可能要 17 s 才答，报表不能比它自己展示的数字更老。

- 窗口 `resets_at_ms` 已过的样本直接丢弃：一条旧日志记录曾让 Codex 的"月 0%"条永远挂在面板上。**不宣传重置时间的日志行**（`resets_at_ms==0`）无法用这个规则判死，改按年龄判：最后一次被提到超过 24 小时就退场——否则探针改了标签（Qoder 的"已用完"退役时）会留一条永远擦不掉的鬼条。
- **同一长度的不同窗口是两个窗口**：窗口身份是 `(工具, 长度, 名字)`。只按长度去重时，ZCode 的"1 月工具调用"被"月 MCP"覆盖，Antigravity 的 Gemini 两组被 Claude/GPT 两组覆盖——四个窗口只剩两条。没有名字的日志行（Codex 就是这么写的）仍与同名的探针行合并，所以不会出现双胞胎条。
- 厂商语义不同：Cline 给 `percentUsed`（已用），Antigravity 给 `remaining_fraction`（剩余，取反），Qoder 给 0..1 的 `percentage` 分数或 `used/total`。标签只写窗口名（Qoder 的资源包带 `剩 N/总N`，因为 credits 的绝对量比百分比有用），不写"剩余"，因为条上显示的是已用。
- `agy /usage` 一次要 10–17 s，所以探针内部预算 25 s、CLI 等 30 s；菜单栏只等 5 s——**超预算的探针不会让它的条从面板上消失一个周期**：预算到点时该工具若还没答，就用磁盘上 6 小时宽限期内上一次的答案补位（cline 一次探测最多三个串行 HTTPS 请求，正是它先撞上的这个问题——条目反复消失又出现，看起来就是"位置在跳动"），迟到的那轮自己落缓存、下一轮起就是新值。

## 面板结构

菜单栏下拉出来的是**四个页面**，不是一条长滚动：`概览`（活动热力 + 配额条）、`工具`（每工具的用量与花费）、`排行`（模型 / 项目 / MCP / Skill）、`明细`（最近会话 + 数据来源）。每页里被截断的列表都有"查看全部 N 项"，折叠时就把隐藏数量写在按钮上。

这么分是因为全塞一页时面板实测 2,102 px 高、视口只有 593 px，每个 section 都要滚过六个体温才能看到。

配额区的顺序是**固定的，不是排行**：报表侧按名义窗口长度定档（5 小时 → 日 → 周 → 月，隐含长度与无窗口长度的条排其后），面板不再按"已用百分比"重排——百分比每次刷新都变，zcode MCP 条的窗口长度是不断缩小的重置间隔，按它们排序条目就会在眼前跳动。顺序归用户管：**长按**任意一条或某个工具的标题行即可拖动（移动超过 8px 视为滚动，不触发拖拽），排列写进 `settings.json` 的 `quota_tools`/`quota_rows`，键是探针给的稳定窗口 id（label 里带实时金额、长度会漂移的条都带 id）；没拖过的条按报表默认序，新出现的窗口排在已保存顺序之后。

条的配色表达**"距离不可用还有多远"**（红绿灯语义）：填充是一条锚定整条轨道的渐变（`--ok` 绿 → 62% 起 `--warn` 橙 → 100% `--bad` 红），填充只负责露出渐变的前段——条的前缘颜色就是剩余量，**满条必然以红色收尾，"满"只会被读成"用完"，不会被读成"还没用"**；超 100%（预算超支）整条红色斜纹、数字显示真实百分比。数字本身在 ≥80% 变橙，色弱用户不依赖颜色也能读。

工具行的徽标用**该工具自己 app 的图标**：`usage-core/src/icons.rs` 直接解析 `/Applications`（和 `~/Applications`）里 bundle 的 `Contents/Resources/*.icns`，按 `IHDR` 宽度挑"不小于显示尺寸 2× 的最小那张"（不放大，宁可缩小），原样取内嵌 PNG 编成 data URL。只有确实有 bundle 的才用图标——Codex、OpenCode、Pi、Cline、AtomCode、Mimocode 是命令行工具，**没有 app**，就退回彩色首字母；借一个邻近产品的图标会把 token 到底出自哪个账号标错。图标是装饰：行上的名字照旧，读不出 PNG 也只是掉成字母，任何数字都不经过它。`tokenme icons` 看这台机器上哪些源能拿到图标（`--json` 是面板与 `?real=1` 预览读的数据）。

## 数字怎么验证的

token 数必须是确定的，金额则依赖价格表的选择，所以这两件事分别有各自的门禁。

**两个独立重写的对账脚本**（`scripts/`，Python 实现，故意不复用 Rust 代码——同一套逻辑自己验自己不算验证）：

```bash
python3 scripts/verify-totals.py          # 原始日志/SQLite → 索引：每个源逐字段对账，容差 0.5%
python3 scripts/verify-report.py --freeze # 索引 → 报表：窗口/分组/热力/预算/配额全部重算
```

- `verify-totals.py` 除了对账总量，还断言每个源自己的**恒等式**（`input+output==total`、`totalTokens` 恒等、Antigravity blob 的 `#4.3==#4.9+#4.10`、"Qoder 记录里 tokens>0 的必须为 0"）——阶段的归属是用数据自身的身份判定的，不是照抄 ccusage（它在 Claude 的流式记录上会被同一 prompt 的多次写入重复计费，实测高估 1,432,576 tokens）。
- **zcode 的对账按行不按和**：厂商会删掉整个会话的 `model_usage` 行（实测一个会话 32 行 / 60,004 out tokens 被删、rowid 无缺口——是重建不是轮换），只增不删的索引把这些当历史保留，"索引==当前表"这个和式恒等从此不成立。改为更严的**逐行身份**：表里每一行都必须在索引里以 `{session}#{request}#{attempt}` 出现一次且五个数字完全一致（全零行除外——适配器本来就不为它建事件）；只存在于索引一侧的行被单独量化报告（"vendor-deleted, kept as history"），从和式比较里扣除而不是装作没看见。
- `--freeze` 先用 sqlite `.backup`（不是 `cp`，`.backup` 会把未 checkpoint 的 WAL 折进来）把索引复制一份，两侧都读这一份快照；菜单栏程序在实时写索引，不冻结就会拿移动靶当 bug。
- 本机现状（2026-09-24）：14 个源、60 项逐字段对账 + zcode 逐行身份 18,106 行（3 行厂商已删、留作历史），`0 mismatched field(s)`；报表侧 `every report number reproduced independently: PASS`。`ccswitch` 那一项的独立结论是"0 条"——这台机器上它的 92,754 行网关账本每一行都是别的源的副本（85,011 行是它回灌的 session 日志、7,743 行是 Claude 走网关的流量），两边同时算出 0 才算对上门。
- 单测门禁是 `cargo test --workspace --all-targets`（本机 429 passed / 27 ignored）：解析用例读 `crates/adapters/*/tests/` 下真实记录切片的 fixture，`tests/real_*.rs` 那几条读这台机器的真实日志（本机没有对应工具就自动 skip），索引层的增量/去重/WAL/回合完整性行为在 `crates/usage-index/tests/ingest.rs` 与 `watcher.rs`，CLI 的黑盒契约在 `crates/usage-cli/tests/cli.rs`。

**价格的选择要能查。** models.dev 上同一个模型常有几十家报价，`usage-core` 的优先级是"厂商自己的报价 > 该厂商的 coding-plan 克隆 > 聚合平台"，这是一个**决定**，所以必须可审计：

```bash
tokenme pricing explain deepseek-v4-flash  # 谁定的价、还有谁在卖、比它便宜/贵的各是谁
tokenme pricing contested --limit 15       # 索引里每一个多方报价的模型 + 这个选择能挪动多少钱
```

`contested` 表里的 `Δ next` / `Δ worst` 就是金额列的误差棒——token 是精确的，`cost` 只在 Δ 为 0 的行上是精确的。要把某个模型钉死用自己的价：`--pricing-override 'model=in/out/cache_write/cache_read'`（`pricing explain` 会把它的来源标成 `override`）。

## 构建

前置：Rust 1.97+、pnpm 9、Xcode（macOS 打包）。

```bash
cargo build --release -p usage-cli        # CLI：target/release/tokenme
cargo test --workspace --all-targets      # 全量门禁
./scripts/build-macos.sh                  # 菜单栏程序 → .app + .dmg
./scripts/build-macos.sh --dev            # 开发模式（含前端热更新）
```

面板可以脱离菜单栏程序看：`pnpm dev` 后打开 `http://127.0.0.1:1420/?real=1`，它会读 `apps/tokenme-bar/report.json`（用 `tokenme report --json > apps/tokenme-bar/report.json` 生成，已 gitignore）——这样能拿这台机器的真实数字审布局；不带 `?real=1` 走 fixture。要看真实 app 图标再补一份 `tokenme icons --json > apps/tokenme-bar/icons.json`（同样 gitignore；没有它就全部退成首字母）。

未做代码签名（自用/内部分发）：首次打开需右键 → 打开以越过 Gatekeeper；`xattr -dr com.apple.quarantine /Applications/tokenme.app` 亦可。菜单栏程序要求 macOS 12+。

DMG 由脚本用 `hdiutil` 生成，而不是 Tauri 的 `bundle_dmg.sh`：后者要靠 AppleScript 驱动 Finder 摆图标，在没有"辅助访问"授权的环境（CI、远程会话）里必然失败，所以 `tauri.conf.json` 的 `bundle.targets` 只列 `app` 与 `zip`。

## 数据位置

索引库 `~/Library/Application Support/tokenme/index.db`（Linux: `$XDG_DATA_HOME/tokenme/`），配额缓存 `~/Library/Application Support/tokenme/quota/`，设置 `settings.json` 同目录。索引只增不删，源文件被截断时按 `source` 键清除该文件的事件。

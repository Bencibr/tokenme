# TokenMe 适配器设计与逆向工程备忘录 (Internal Dev & Reverse Engineering Notes)

> 本文档用于归档 TokenMe 对各 AI 编码工具适配器实现的底层机制、逆向分析、实测表现与协议细节。供开发者维护与扩展新适配器查阅。

---

## 1. 计量单位与定价口径

- **核心原则**：索引层内部只存储确定的 Token / Credits 原始计数值，**不持久化金额**（价格表会动态变动）。
- **统一计价**：金额由 `usage-core/pricing.rs` 统一按 `https://models.dev/api.json` 价格清单计算（断网时自动回退到内置离线快照）。
- **四个互斥阶段**：`input`（净输入，不含缓存）、`cache_creation`、`cache_read`、`output`；`reasoning` 作为 `output` 的子拆分，**永不叠加**。
- **Credits 计量与折算**：
  - 按 Credits 计量的**事件**，按厂商公布的方案价统一折算（例如 Qoder 官方 1 credit = $0.01）。
  - 在报表中单独列出 `credit_cost`，界面用 `≈` 标识“由 credits 折算”，绝不虚构 Token 数量。
  - 计量单位是**逐事件**的属性（`UsageEvent::meter`），**不是工具的属性**：同一工具可以两种计量并存。索引层一律读事件上的 `meter`，绝不读工具的 `Semantics`，混合来源才能各自正确计价。目前 `Meter::Credits` 只由 Qoder 的转录树通道产生（见 §2 Qoder）。
  - WorkBuddy 同时具备两类信号：逐请求 `rawUsage` 给 Token，逐请求 `rawUsage.credit` 给厂内积分；两者都入索引，金额仍走中心计价，credits 只作 `credit_cost`。

---

## 2. 实时配额探针实现细节 (Quota Probes)

配额探针分为两类：
1. **日志自带窗口 (`origin = log`)**：如 Codex 日志中自带的 `rate_limits`。
2. **实时探针 (`origin = probe`)**：并行探针，内置 5 秒超时预算（`usage_quota::BUDGET`；一次性的 CLI 命令自己传 30 秒）与 5 分钟安全缓存（`usage-quota::TTL`，全厂商共用同一时长，按工具各存一份缓存文件），超时自动降级，绝不卡死主进程。
   - **写回特例**：绝大多数探针只读凭据、绝不落盘。唯三例外是 **MiniMax Code**、**Kimi Code** 与 **Cline**——这三个工具在盘上存的是短时效访问令牌，不续期额度条就会长期空白；它们各自只拿同一份凭据文件里的 refresh token 去换，并把轮换后的新对子按该工具自己的格式原子写回（细节见下三节）。换取失败则什么都不改。WorkBuddy 盘上的凭据来自 `tokenme workbuddy-login`（用户主动登录），不是探针写的。

### 各厂商适配细节：

#### Codex
- 走官方接口与本地日志窗口结合。

#### Qoder
- **双通道用量数据源**（同一 tool id 下的两个 `SourceFile`，共享 `qoder#<request_id>` 去重命名空间）：
  1. **转录树** `~/.qoder/projects/**/*.jsonl`：Claude-Code 形状，但**所有 token 字段恒为 0**，唯一活数是 `message.usage.credits` → 逐事件 `Meter::Credits`。
  2. **IDE 缓存库** `<App Support>/Qoder{,CN}/SharedClientCache/cache/db/local.db`，表 `chat_message` 的 `token_info` 列 → 逐事件 `Meter::Tokens`。仅 IDE 的聊天/Quest 路径写入。
  - **字段映射**：`token_info` = `{prompt_tokens, cached_tokens, completion_tokens, max_input_tokens}`。**`prompt_tokens` 已含 `cached_tokens`**，必须净出 `input = max(0, prompt − cached)`、`cache_read = min(prompt, cached)`、`cache_creation = 0`，否则缓存前缀被重复计价；`max_input_tokens` 是配置的上下文上限（同一 db 的 `chat_record.extra` 里另有 `ideModelConfigOverride.max_input_tokens = 200000`），**不是用量阶段，必须丢弃**——`TokenCounts` 下游会乘单价，200k 会被当 200k token 计费。该映射由 4 个 MIT 开源实现交叉验证（TokenTracker / vibe-usage / token-monitor / tokei），四者对 `cached` 超出 `prompt` 的行一律 clamp 而非信任。
  - **`token_info` 不保证是合法 JSON**：实测表中存在 `not-json` 与空串，SQL 侧 `json_extract` 会以 `malformed JSON` 让整批失败并把游标永久卡住，因此**逐行在 Rust 侧解析**，解析失败的行计为 `Census::invalid_json` 后跳过。
  - **明确无 Token 的两处**（均刻意不读）：转录树本身（token 恒 0）；桌面 app 的 `~/Library/Application Support/com.qoder.app.stable/main.sqlite`（`chat_session_context_usage` 的快照全为 `tokenCountsAvailable:false`）。
- **实时接口**：`GET https://openapi.qoder.sh/sash/api/v2/me/usage`（头为 `Authorization: Bearer <token>` + `Cosy-ClientType: 10` + `User-Agent: Qoder`，实测约 200 / 6 ms）。
- **凭据解密**：Token 解密自 `secret://aicoding.auth.userInfo`（经 safeStorage 信封解密）。`expireTime` 过期则放弃活读，绝不刷新。
- **回退机制**：快照 `secret://aicoding.auth.creditUsage` 作为离线回退（注意本地快照在某些场景下容易滞后，活接口优先）。

#### WorkBuddy
- **凭据**：读取桌面端 OAuth 记录 `CodeBuddyExtension/Data/Public/auth/workbuddy-desktop-ai.info`。
- **实时请求**：`POST https://<domain>/billing/meter/get-user-resource`（CN 域走 `/v2`，Global 失败自动回退）。按包名聚合成单条展示。

#### Antigravity CLI
- 通过调用本地 CLI `agy -p /usage --output-format json` 读取分组 Bucket。
- 保持只读：不主动触发 refresh_token 刷新，避免与 Google 官方轮换冲突导致用户登出。

#### Cline
- 对齐 cline/cline 源码。
- 套餐名通过 `GET /api/v1/users/me/plan` 获取；配额窗口通过 `GET /api/v1/users/me/plan/usage-limits` 解析。
- **凭据写回特例**：Access Token 有效期约 1 小时。不是等它过期——剩余寿命不足 `REFRESH_BUFFER_MS`（`5 * 60 * 1000`，即**到期前 5 分钟**）就走 Cline 网关轮换，并把新对子原子写回 `providers.json`（tmp+rename，沿用文件自身的 0600 权限），确保配额窗口不失效；等过期才换会让额度条空白一个回合。

#### Kimi Code
- **用量**：CLI 自己的事件日志 `sessions/**/wire.jsonl`（v1 与 v2 两种布局，`usage.record` 行给四个阶段），外加桌面端内嵌 runtime 的 home；根按 `KIMI_CODE_HOME` → `KIMI_DATA_DIR` → `~/.kimi-code` → `~/.kimi` 解析（`crates/adapters/kimicode/src/paths.rs`）。
- **凭据**：CLI 写的 `~/.kimi-code/credentials/<name>.json`（0600，snake_case 线格式，对齐厂商 `packages/oauth/src/storage.ts` 的 `access_token`/`refresh_token`/`expires_at`），凭据目录走同一条根阶梯，取 `expires_at` 最新的那份。
- **实时请求**：`GET {base}/usages`，base 依次是 `https://api.kimi.com/coding/v1` 与 `https://api.kimi.ai/coding/v1`（`KIMI_CODE_BASE_URL` 可钉死其一）；窗口 5h / 7d / 月，`limit_month_code` 只是月窗口里的 kimi-vs-code 拆分，**不是第四行**。
- **令牌时效与写回特例**：访问令牌只有 15 分钟，所以先用盘上那份打 `/usages`；**只有 `/usages` 全部拒绝**才走厂商刷新契约 `POST https://auth.kimi.com/api/oauth/token`（`grant_type=refresh_token`），把返回的 `access_token` 与**必定轮换的** `refresh_token` 连同 `expires_at` 一起 tmp+rename 原子写回同一份文件，然后重试。刷新是最后一步、不是第一步；换取失败（401/403/invalid_grant）文件一字不动。
- **桌面端 key 只读**：CLI 无可用凭据时退到 Kimi.app 的 `<userData>/kimi-desktop/daimon-share/daimon/config.json` → `credentials.kimiCode.{apiKey,baseUrl}`，该 key 只读、永不刷新，且只回一条 `totalQuota` 汇总窗口而非分级桶。

#### MiniMax Code
- **用量**：会话转录 `v2/sessions/**/messages.jsonl`（assistant 记录给四个阶段；用量阶段来自 vendored `pi-mono`，信封与按日布局是 MiniMax 自己的）。根按 `MINIMAX_DATA_DIR` → `MAVIS_DATA_DIR` → `~/.minimax[-profile]`（含厂商留作软链的遗留 `~/.mavis[-profile]`）解析。
- **凭据**：应用自己写的 `<dataDir>/auth/<buildEnv>/<region>/<client>/auth.json`（本机 `~/.minimax/auth/prod/en/mcode-public/auth.json`），`records` 每条带 `accessToken`/`refreshToken`/`expiresAtMs`，取剩余寿命最长的那条（登录过多次就有不止一条）。
- **实时请求**：两条资源路径、同一份凭据——`GET {platform}/v1/api/openplatform/coding_plan/remains`（5h / Week 两窗口）与 `GET {gateway}/minimax-cloud/api/v1/credit/details`（购买与签到两种钱包）；`status == 3` 的无限窗口**省略而非画 0%**。
- **令牌时效与写回特例**：访问令牌约 1 小时。剩余寿命不足 `REFRESH_MARGIN_MS`（120 秒）才换：`POST {accountOrigin}/oauth2/token`（`client_id=mcode-public`、scope `agent.default`、audience `agent-backend`）。**轮换是一次性的**（2026-10-05 实测：复用已轮换的 refresh token 答 `400 invalid_grant — this refresh token can no longer be used`），不把新对子写回去就等于把桌面端登出，所以按厂商自己的格式写回：先取它的 `auth.lock` 目录锁（mkdir 协议，30 秒视为遗弃、最长等 5 秒），锁内重读盘上记录——**refresh token 已不是自己手里那份就放弃写入**，绝不覆盖更新的凭据；再 `generation + 1`、`auth.json` 与 `auth-state.json` 依次 tmp+fsync+rename 原子落盘并保持 0600。换取失败什么都不改，盘上那份照旧一试（服务器才是令牌生死的裁判）。
- **团队套餐不探**：`X-Group-Id` 那条路要先过厂商的签名归因接口，个人套餐不需要它，因此只读个人账号、不伪造第一方请求。

#### ZCode
- 直读 App 活接口，彻底摆脱 LocalStorage 过期副本：
  1. `GET https://bigmodel.cn/api/monitor/usage/quota/limit`（获取 5 小时 Prompt 池、周额度、工具调用限额）。
  2. `GET {base}/api/biz/subscription/list`（获取有效套餐名称）。
  3. `GET https://zcode.z.ai/api/v1/mcp/usage`（获取 MCP 调用配额）。
- 凭据解密：采用 AES-256-GCM 本地解密。

---

## 3. 对账与验证门禁 (Verification & Integrity)

- `python3 scripts/verify-totals.py`：对账原始日志/SQLite 到索引库的每一个字段，断言内部恒等式（容差 0.5%）。
- `python3 scripts/verify-report.py --freeze`：基于 SQLite 快照，对报表窗口、分组、热力和配额进行全量重算比对。

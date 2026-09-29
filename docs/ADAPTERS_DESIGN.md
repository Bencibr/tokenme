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
2. **实时只读探针 (`origin = probe`)**：并行探针，内置 5 秒超时预算与 5 分钟安全缓存，超时自动降级，绝不卡死主进程。

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
- **凭据写回特例**：Access Token 有效期仅 1 小时，过期走 Cline 网关轮换并原子安全写回 `providers.json`，确保配额窗口不失效。

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

# TokenMe 适配器设计与逆向工程备忘录 (Internal Dev & Reverse Engineering Notes)

> 本文档用于归档 TokenMe 对各 AI 编码工具适配器实现的底层机制、逆向分析、实测表现与协议细节。供开发者维护与扩展新适配器查阅。

---

## 1. 计量单位与定价口径

- **核心原则**：索引层内部只存储确定的 Token / Credits 原始计数值，**不持久化金额**（价格表会动态变动）。
- **统一计价**：金额由 `usage-core/pricing.rs` 统一按 `https://models.dev/api.json` 价格清单计算（断网时自动回退到内置离线快照）。
- **四个互斥阶段**：`input`（净输入，不含缓存）、`cache_creation`、`cache_read`、`output`；`reasoning` 作为 `output` 的子拆分，**永不叠加**。
- **Credits 计量与折算**：
  - 对仅报告 Credits 的工具（如 Qoder、WorkBuddy），按厂商公布的方案价统一折算（例如 Qoder 官方 1 credit = $0.01）。
  - 在报表中单独列出 `credit_cost`，界面用 `≈` 标识“由 credits 折算”，绝不虚构 Token 数量。

---

## 2. 实时配额探针实现细节 (Quota Probes)

配额探针分为两类：
1. **日志自带窗口 (`origin = log`)**：如 Codex 日志中自带的 `rate_limits`。
2. **实时只读探针 (`origin = probe`)**：并行探针，内置 5 秒超时预算与 5 分钟安全缓存，超时自动降级，绝不卡死主进程。

### 各厂商适配细节：

#### Codex
- 走官方接口与本地日志窗口结合。

#### Qoder
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

# 生产协议与状态合同 0.5

日期：2026-10-20。适用范围：F01—F12 的实现输入，**尚未实现的生产合同**。当前可运行的 alpha 仍以参考工程 WIT/schema 为准；此文中的生产字段、服务、表和能力不得直接写入 alpha 清单冒充支持。

本文固定原方案中只有职责、没有行为的部分。实施者据此生成类型、schema、绑定和合同用例；需要更改语义时提交 ADR 和兼容测试，不在实现中自行采用另一口径。验收追踪见 [审计与验收矩阵](AUDIT.md)。

0.5 的修订语义和可执行设计反例见[补充合同](DESIGN_HARDENING.md)与[第二轮审计](AUDIT_ROUND2.md)。本文和补充合同是生产设计输入；完整生产 Schema/WIT/SDK 未在 F01 完成前，不宣称协议已经冻结。review 模型不是生产运行器。

入站 Agent 代理是新增可选扩展，见[网关草案](AGENT_GATEWAY.md)。本文的 http.request/process.invoke 不支持入站监听、长会话或流式代理；网关能力、request 触发、GatewayContext、权限/设置与流服务由 F13/G01 独立 profile 生成，不能扩写当前严格 v1 后假称兼容。本文的普通任务预算与未知字段拒绝规则继续成立。

## 1. 协议版本、扩展与兼容

生产目标 profile 为 `tokenme.plugin/1.0`，WIT 包为 `tokenme:plugin@1.0.0`、world 为 `adapter`；当前 alpha `0.1` 的包不能自动视为生产包。F01 生成并提交 `spec/plugin/v1/` 的 WIT、清单、任务、服务、结果、事件、设置、视图、诊断 schema 与正反样例，冻结为同一个合同版本。生产 crate 的 API 包提供唯一校验器，SDK、宿主、CLI 和作者工具使用同一版本，不能各写宽松解析器。

采用 JSON UTF-8、字段 snake_case、机器枚举固定英文、未知字段拒绝。声明式清单沿用当前 camelCase；两者通过生成类型映射，禁止隐式大小写转换。数组有上限，字符串按 UTF-8 字节计；字典 key 另行校验。所有整数在 `[0, 9007199254740991]`；时间为 UTC Unix ms，日界由独立的时区/周期字段决定。缺失和 `null` 只在声明为 optional 的字段允许；缺失数据不转为 0。浮点仅允许有限的比例/测量值，金额与原生 credits 使用十进制字符串。

基础别名固定：Ref=宿主生成的 32 位小写十六进制随机标识；Hash=64 位小写十六进制 SHA-256；Hex32=32 位小写十六进制；Int=上述安全整数。普通 ID 为 1..128 bytes 的 ASCII `[a-zA-Z0-9_.:-]+`，各子类采用更严格约束（插件 ID 沿用 dotted lowercase ≤96、普通任务 ID 禁冒号）。Range/Coverage 均为 `{since_ms:Int,until_ms:Int}`、左闭右开且 since<until。ISO4217 是三个大写 ASCII 字母，必须在宿主版本化币种目录中存在。TypedMap 最多 16 字段，只接受目录声明的 bool、有界数值、有限 enum 或宿主验证的 Ref，不接受自由文本。

能力名和版本分别存储，例如 `{id:"quota.read", major:1, minor:0}`。安装先求宿主支持与清单要求的交集：必需能力/服务未知或 major 不兼容，拒绝激活；可选能力缺失则不注册对应任务/视图，并解释原因。一个任务的全部依赖服务必须满足后才可执行。未知字段不会因 minor 较新而被忽略；新增字段必须同时发布声明其支持的 schema/profile。无依赖的普通工具插件可以独立升级，不加载其他插件的代码或动态库。

可复用估算/价格实现通过宿主管理的 capability provider 引用。消费者指定 provider ID、能力版本和输入 scope；宿主执行、校验后交付数据。禁止同步递归互调；构建依赖 DAG，拒绝环，最长链 4，因果事件深度 8、每根事件派发最多 64 次，按根事件/任务去重。新增指标或视图算子若宿主无法表达，须升级宿主。

## 2. 包、身份、安装与信任

生产包固定为 `.tokenme-plugin`，规范 ZIP_STORED、排序路径、1980 时间、普通文件 0644、无 comment/extra/目录项，拒绝大小写别名、Windows 保留名、穿越、链接与重解析点。单文件 32 MiB、整包及解包总量 64 MiB、最多 64 文件。载荷严格为清单、组件、清单引用资源、LICENSE、build-info、checksums、signature；设置和视图**内嵌 manifest**，不另设二选一的 schema 文件。若以后资源数不足，升 profile 修改限制。

清单在 alpha 基础上增加确定区块：`publisher`、`hostRequirements`、`platforms`、`permissionProfile`、`capabilities`、`auth`、`events`、`notifications`、`views`、`stateSchemaVersion`。permissionProfile 为 standard-v1 或 credential-maintenance-v1，权限不因签名自动扩大。tool 改为非空 tools 数组，各 task/source/endpoint/setting/view 明确所属 tool ID。ID/任务/资源包内唯一；SemVer 拒绝前导零和空 prerelease。国内外版独立 ID/授权/账号/路径/缓存/周期。字段映射与可选能力语义见补充合同 §8。

`plugin_id` 是包身份；`publisher_id` 是信任库绑定的发布者；`tool_id` 是稳定产品身份。官方保留 tool ID 的归属保存在宿主信任元数据，不能由包抢占。社区新工具内部键为 `(publisher_id, tool_id)`；显示名相同不合并。替换既有产品适配实现必须通过显式映射和来源交接，不能靠名字匹配。插件版本不进入历史事实主键。

`build-info` 必含源树摘要、源码修订（不可用时明确 local）、SDK/API/profile、编译器/依赖锁摘要、模块摘要和构建模式。`pack` 只使用成功构建凭据对应的源树与组件；本地摘要不能证明发布者诚实编译，官方发布还需构建流水线记录和可下载源码修订。重复打包通过只证明同一组件的归档确定性。

生产签名为 Ed25519。消息固定为 UTF-8 `tokenme.plugin.package/v1`、一个 LF 字节、checksums 原始字节的 SHA-256 二进制值。checksums 精确覆盖所有载荷（包含 manifest 的 publisher、API 和资源），不包含自身和 signature。信封固定字段 `schema, publisher_id, key_id, algorithm, integrity_sha256, signature_base64`，algorithm 只能为 ed25519。签名发布者必须与被签清单一致，密钥绑定来自本地信任库，包内公钥不是信任根；生产禁用 alpha 的元数据变体标记。

生产策略：预装官方包使用应用内信任根；第三方本地包先展示发布者、摘要和权限差异，用户信任指定发布者密钥后安装；无签名包仅开发 profile 的明确本地开关可安装，不能进入正常 profile。初次信任未知发布者本来就是用户操作，框架不能代用户扩大信任范围。开发者机器的本地构建不等于发布签名完成。

密钥轮换须由旧可信密钥或应用更新携带的可信根签署新 key 绑定和单调递增 revision；已经撤销的 key 不能签署恢复自身的更新。撤销记录本地持久化，拒绝低 revision/过期在线更新元数据。离线只按已取得的撤销与信任状态判断，不承诺实时撤销。手动回退须明确选择旧摘要，仍须通过当前撤销/权限规则；签名有效但已撤销的包不得运行。

安装步骤固定为 `verified → staged → awaiting_grants → migrating → active`，失败进入 rejected/rollback_required。校验、资源限制和签名先于执行；迁移只在隔离的私有状态副本中运行。每插件独占安装锁；不可变目录 `<profile>/plugins/store/<id>/<version>/<digest>`；运行前重新校验完整包身份/版本/摘要/资源。源包只读一次建立验证快照，后续不再次读取可被替换的源路径。

生产文件操作使用平台原生 handle 相对遍历、禁止跟随重解析点/符号链接、校验最终文件身份，不能只依赖字符串 resolve。临时文件使用不可预测名称+exclusive create，激活在同文件系统完成；系统允许时 flush 文件和父目录。目录权限仅当前用户/应用；安装失败保留原活动版本。并发安装返回 `INSTALL.BUSY`，不偷走活锁。

激活的唯一真源是索引数据库中的 `plugin_active`（与租约/接收 fencing 同库）。文件 active.json 只是镜像。先完成不可变文件与迁移，再在一个 DB 事务注册元数据、grants revision、递增 generation、安装事件 outbox 和 active；最后镜像指针。崩溃在事务前清理/保留可回收 staging，事务后修复镜像；绝不从不完整镜像恢复到未验证目录。

## 3. 稳定数据类型与调用上下文

以下为应生成类型的精确定义；`?` 表示该字段可为 null，其他字段必需。`Ref` 为宿主产生的不可猜测引用，不能用真实路径、用户名、凭据或账号邮箱代替。

| 类型 | 字段 / 语义 |
| --- | --- |
| Scope | tool/account/source 三种互斥结构，字段及能力约束见补充合同 §1；发现可在没有账号/来源时运行，不填虚构默认账号 |
| TaskContext | `invocation_id:Ref, output_batch_ref:Ref?, operation_ref:Ref?, plugin_id:ID, package_sha256:Hash, generation:Int, task_id:ID, handler_id:ID, capability_id:ID, capability_major:Int, scope:Scope, trigger:Trigger, settings_snapshot_revision:Int, execution_revision:Int, grant_revision:Int, deadline_ms:Int, budget:Budget, trace_id:Hex32`；output_batch_ref 仅 usage.collect 非 null，operation_ref 仅 action.run 非 null |
| Trigger | 枚举 startup/interval/source_change/manual/event/reconcile；写任务的 manual 来源只能由宿主 UI/CLI action 创建 |
| Budget | `fuel:Int, guest_memory_bytes:Int, input_bytes:Int, result_bytes:Int, io_calls:Int, io_bytes:Int, log_events:Int`；取清单申请、用户配置、平台硬上限的最小值 |
| Cursor | `schema_version:Int, source_epoch:Ref, position_json:string`；position 最大 8 KiB，宿主不解释插件私有内容，epoch 由宿主确认来源身份后生成 |
| BatchHeader | `schema_version:Int, batch_id:Ref, source_revision:Ref, expected_cursor_revision:Int, completeness:complete|partial, next_cursor:Cursor?` |
| CommitReceipt | `batch_id:Ref, accepted:Int, duplicate:Int, corrected:Int, retracted:Int, rejected:Int, cursor_revision:Int, report_revision:Int` |
| PluginError | `code:ID, stage:ID, retry_class:never|read_backoff|reconcile_only, safe_fields:TypedMap`；字段必须在错误目录声明，禁止自由异常正文 |
| Decimal | 正则 `^(0|[1-9][0-9]{0,17})(\.[0-9]{1,12})?$`，非负，最多 18 位整数+12 位小数；整数运算/定点库实现，拒绝指数形式、逗号、NaN/Inf |

SDK 上下文由宿主在 Store 中绑定。生产 WIT 保留 `execute(task-id,input-json)->result<string,string>`，host imports 固定为 `context()->string`、`call(service-id,request-json)->result<string,string>`、`log(event-id,fields-json)->result<_,string>`。字符串值分别使用本 profile 的精确 JSON schema；错误分支为序列化 PluginError，经宿主重新校验。F01 将该签名生成到 v1 WIT；当前 alpha 不接受这三个新方法的组合。

跨设备 identity 不直接使用各设备独立随机的 account_ref：显式配对时由 primary 分配 canonical tool/account Ref，按 publisher/tool/edition 和已确认的供应商账号身份建立本地映射；换凭据不换 canonical Ref。非配对/身份无法确认时保持 unlinked，不按邮箱、显示名或 Token 猜测合并。同步只发送配对域的 canonical Ref 和来源归属，不发送原始供应商账号标识/凭据；导出诊断还须另换包内临时 Ref。旧历史无账号证据时保存 legacy-unassigned scope 和原 dedupe，不编造账号；切换点后的确定归属单独入库，历史归属调整必须做显式对账事务。

manifest 负责静态注册，没有任意 guest init/unload hook。execute 仅处理已注册 task ID，保留的 `host:migrate` 是独立生命周期操作，普通任务 ID 不允许冒号。每次实例化禁止宿主 call/log 在 task context 未开启时调用；迁移只将 state.get/state.cas 绑定到 staged 私有副本，无网络、认证、主 profile 状态或签到权限。context 的身份即使返回给插件，也不能被改写后拿到更高授权。Ref 的分配、候选账号确认和 output_batch_ref 来源见补充合同 §1。

## 4. 能力输入、输出与接收规则

每个输出由对应 schema 和语义校验器共同验收。宿主添加 Scope/插件版本/接收时间，插件不能返回其他账号或伪造 origin。批次最多 1000 条、结果 UTF-8 最大 1 MiB；超限整批失败，游标不动。跨 capability 返回错误类型则 `DATA.CAPABILITY_MISMATCH`。

### 4.1 发现与用量

`source.detect` 输入为 `allowed_root_refs:Ref[], previous_snapshot_ref:Ref?, limit:Int`；输出 `candidates:[{source_definition_id:ID, root_ref:Ref, relative_path:string, account_hint_ref:Ref?, kind:jsonl|sqlite|json, revision_hint:string?}]`。路径只能落在本次宿主授权根，真实 source_ref/epoch/账号归属由宿主确认。账号 hint 只是候选，不能自动把两个已登录账号合并。

`usage.collect` 输入为 `source_ref, cursor:Cursor?, cursor_revision:Int, since_ms:Int?, until_ms:Int?, limit:Int`；输出 `header:BatchHeader, facts:UsageFact[], retractions:Retraction[]`。retractions 的范围和原子规则见补充合同 §2。完整 UsageFact 字段固定如下：

| 字段 | 类型/含义 |
| --- | --- |
| identity_domain | vendor_request / session_call / source_record |
| occurrence_id | 非空、无控制字符、最多 256 bytes；vendor_request 在该工具/账号域稳定唯一；session_call 在 session_ref 内唯一；source_record 在 source_ref/epoch 内唯一 |
| session_ref | 不透明本地 session ID，可 null；session_call 时必需 |
| ts_ms | Timestamp 或 null；null 只允许 partial，持久 pending 后才能推进游标，不进入时间报表 |
| model | 原始规范模型 ID，可 null；不为了统计强行猜模型 |
| usage_form | per_call / cumulative |
| counts | `{input:Int, cache_creation:Int, cache_read:Int, output:Int, reasoning:Int}` 或 null；四阶段总和≤MAX_SAFE_INTEGER，reasoning≤output；null 代表缺少 Token 实测，区别于显式零 |
| cumulative | per_call 时为 null；cumulative 时为 `{series_ref,sample_index,reset_seq,starts_at_zero,reset_evidence_ref}`，约束见补充合同 §2.3 |
| native_credits | `{unit:ID, amount:Decimal}` 可 null；保留供应商原生单位，不换算成 observed Token |
| vendor_cost | `{currency:ISO4217, amount:Decimal}` 可 null；供应商实报费用，区别于估算 |
| completeness | partial / final |
| revision | 该来源/epoch 的 contribution 修订 Int；旧修订 no-op，同修订不同内容整批冲突，final 不退回 partial |
| replaces | 被明确修正的旧 occurrence ID，可 null；不得撤销别的账号/来源事实 |

宿主的事实键为 tool_ref/account_ref/identity_domain/occurrence_id，session_call 另含 session_ref，source_record 另含 source_ref/epoch。请求 ID 跨设备稳定时，origin_device 不加入事实键；没有跨源稳定 ID 时保持独立、标记可能重叠，不能按时间/Token 相近自动合并。

同一请求的多源记录先存 contribution（来源归属和版本），再按宿主配置的字段优先级合成一条标准事实；同级冲突字段进入 conflict 并从当前数值合计排除、修复 rollup，不能任选先/后到者。Token、credits、实报费用可互补，禁止把同一阶段的多源贡献相加。来源截断只撤回自己的贡献；另一个来源的有效数据继续存在。counts/native_credits/vendor_cost 至少一项非 null。

cumulative 按 source/epoch/series/reset_seq/sample_index 的稳定序列求差，不能仅按时间排序。未知起点第一次为基线；有宿主读取证据的零起点才从 0 计数。无证据负差隔离并报 DATA.CUMULATIVE_RESET_UNKNOWN。迟到/修正从相邻区间重算，不直接对快照求和。精确桥接既有模型、credits/累计/同步语义；大总量不可通过旧 f64 近似后宣称精确，见补充合同 §6。

`facts+pending+retractions+contributions+cursor+source_revision+batch_receipt+report_dirty` 同事务提交；generation、execution/grant revision 和 source lease fencing token 在事务内核验。失败整批回滚；partial 只能推进已完整消费/持久保存的前缀，不跨半条 JSONL/未完成 WAL 行。相同 batch_id 只有 scope/规范 payload 摘要相同才返回原 receipt；不同内容必须 DATA.BATCH_ID_REUSED。详见补充合同 §2。

### 4.2 配额

`quota.read` 输入 `account_ref, previous_sample_ref:Ref?, window_ids:ID[]`；输出 `status:ok|unlimited|login_required|unavailable, window_set:complete|partial, sampled_at_ms:Timestamp, valid_until_ms:Timestamp, windows:QuotaWindow[]`。失败/login_required/unavailable 只更新健康状态、保留 last_good；不转空窗口覆盖最后答案。complete/partial、混合无限窗口、自洽/TTL 约束见补充合同 §3。

QuotaWindow 固定为 `id, state:bounded|unlimited|unknown, label_key, unit:percent|tokens|requests|credits|currency, native_unit:ID?, currency:ISO4217?, used:Decimal?, limit:Decimal?, remaining:Decimal?, used_percent:number?, resets_at_ms:Timestamp?, period:{kind:rolling|calendar|vendor, timezone:string?, period_id:string?}, model_scope:string[]`。id 在同一账号稳定且输出中唯一；used_percent 有限 0..10000，允许超额；limit=0/未知不作分母。unlimited/unknown 的数值为 null；credits 带 native_unit、currency 带 currency，其他对应字段为 null。这是配额测量，不创建 Token 事实。

缓存键 `(tool,account,window)`，新样本按 sampled_at_ms+宿主 invocation 序号覆盖旧样本；晚到不回退。effective_valid_until=min(valid_until,sampled_at+300000,observed_at+300000)，插件/vendor 未来时钟不能延长五分钟 TTL；进程内用单调时钟，重启回拨不把旧值恢复 fresh。健康/禁用与 last_good 分开，完整/部分集合按补充合同 §3 执行。今日总 Token 不叠加配额百分比。

### 4.3 签到与动作

`checkin.run` 输入固定为 `action_id, phase:query|claim|reconcile, period:{id,timezone,starts_at_ms,ends_at_ms}, idempotency_key:Ref, previous_attempt_ref:Ref?`；输出 `state:claimable|claimed|already_claimed|not_available|login_required|outcome_unknown|failed, receipt_ref:Ref?, reward:{unit:ID,amount:Decimal}?, retry_after_ms:Int?`。

周期来自插件声明的厂商周期规则/服务端周期 ID，不能用设备本地“今天”直接替代。幂等键绑定 tool/account/action/period，不绑定插件版本或设备名。宿主状态机：`idle → querying → claimable → claim_inflight → confirmed`；请求发出后超时、崩溃、停用或升级均转 `outcome_unknown → reconciling`。只有 query/reconcile 确认尚未领取且供应商允许安全重试才重新 claim；缺少查询/厂商幂等能力时暂停自动重试，显示待人工核对。

新插件自动签到默认关闭。迁移既有官方插件时保留已有显式全局/单工具设置和行为，不靠 SDK 新默认值重置用户选择。手动操作可以绕过 automatic 调度开关，不能绕过插件停用、账号授权、grant 撤销、互斥和结果未知状态。action 参数受 schema 验证，UI/CLI 传递 action ID，禁止传任意 URL、命令或脚本。

桌面与 CLI 同 profile 通过 DB 租约互斥。跨设备选择一个明确的 writer_device，其他设备只读/触发请求；失联期间不自动接管写角色。所有权转移前须 reconcile 未决动作。离线且无厂商幂等/查询支持时，不承诺跨设备 exactly-once。

### 4.4 估算、价格、燃值和扩展测量

| 能力 | 输入 | 输出 |
| --- | --- | --- |
| metric.estimate.tokens | `input_fact_refs:Ref[], quota_sample_refs:Ref[], coverage:{since_ms,until_ms}, model:string?, config_revision:Int` | `estimate_id:Ref, estimated_counts:TokenCounts, coverage, input_revision:Int, algorithm_id:ID, algorithm_version:string, confidence:high|medium|low, limitations:ID[], generated_at_ms:Int` |
| pricing.resolve | `model:string?, meter:tokens|credits, native_unit:ID?, at_ms:Int, currency:ISO4217` | `price_id:Ref, rates:[{stage:input|cache_creation|cache_read|output|native_credits, amount:Decimal, per_units:Decimal}], currency, effective_from_ms:Int, effective_to_ms:Int?, source_kind:official|user|derived, source_ref:Ref, version:string` |
| metric.estimate.value | `fact_refs:Ref[], estimate_refs:Ref[], price_refs:Ref[], fx_ref:Ref?` | `value_id:Ref, amount:Decimal, currency:ISO4217, basis:observed_usage|estimated_usage, input_revision:Int, algorithm_id:ID, algorithm_version:string, generated_at_ms:Int, limitations:ID[]` |
| telemetry.observe | `source_ref, cursor, range, limit` | 有界 `measurements:[{id:ID, ts_ms:Int, metric_id:ID, value:number, unit:ms|tokens_per_second, sample_count:Int, method:passive|active, model:string?}]` 和 next_cursor |
| metric.read | `scope_ref:Ref, range, config_revision:Int` | `metric_id, unit, dimensions, aggregation:sum|latest|max|weighted_mean, value:number|Decimal, weight:Int?, range, input_refs:Ref[]`；weighted_mean 必须携带 weight |

TokenCounts 使用用量节相同的阶段和约束。估算单独保存，不写入 observed；公共今日总量只取 observed，估算以单独标识/选择展示。燃值由价目快照追溯，缺模型价格则 unpriced，不把缺价当 0；按币种分组，无显式 FX 快照不跨币种加总。FX 必含两币种、Decimal rate、有效期、来源和版本，过期或缺失则保留原币种。

自定义指标默认只在插件区域展示；进入公共汇总需宿主已支持的单位、维度和聚合算子，并经明确 metric descriptor 认可。主动模型测速有成本和副作用，单独 action/授权，不因 telemetry 或诊断 doctor 自动触发。

## 5. 宿主服务的请求、边界与限制

host.call 的 service ID 只能来自宿主支持表；参数不携带有效的 plugin/account/grant 冒充字段，绑定 Store context 决定实际 scope。每次请求同时核验活跃 generation、grant revision、任务资源子集、截止期和剩余预算，撤权后已建立句柄下一次使用也失败。

| service ID | 请求字段 | 响应 / 执行约束 |
| --- | --- | --- |
| clock.now | 无 | now_ms；任务内部固定采样时间和真实时钟分开，测试可注入 |
| source.peek / credential.discover / identity.resolve | 发现定义/候选/当前 scope 内键 | 发现时机、hint 确认、Ref 分配与权限边界见补充合同 §1 |
| source.list | root_ref, definition_id, page_cursor?, limit | entries 和 next_page；最多 1000 文件/页，发现最多 10000，超限说明不完整 |
| source.read | source_ref, cursor?, max_bytes | bytes_base64、revision、next_cursor、complete_prefix；1 MiB 按 base64 解码后计，单行最多 256 KiB |
| source.query | source_ref, query_id, typed_params, page_cursor?, limit | rows、snapshot_revision、next_page；只读 SQLite、最多 1000 行/1 MiB，5 秒或任务剩余时限的较小值 |
| http.request | endpoint_id, path_params, query_params, body_json?, credential_handle? | status、headers_allowlist、body、sampled_at_ms；响应最多 1 MiB，连接 3 秒、请求 10 秒且受剩余 deadline 限制 |
| broker.request | broker_definition_id, connection_ref?, operation_id, params | 已归属的本地连接与有界响应；不能把任意 localhost URL 当可信 endpoint |
| credential.acquire | auth_definition_id, slot_id, account_ref | 不透明 handle、expires_at_ms?、credential_revision、login_state；无明文 |
| credential.exchange | auth_flow_id, handle, expected_revision, phase | 新 handle/revision 和经允许公开的元数据；并发刷新同账号单航班，先比较 revision 再写回 |
| process.invoke | program_id, typed_args, input_ref? | exit_code、截断标志、受限 stdout；无 shell，最长 10 秒或剩余 deadline，输出 1 MiB，进程树取消 |
| state.get / state.cas | key、expected_revision / 新 typed value | value、revision；每 key 64 KiB、每 plugin/account 10 MiB，不能访问核心索引表 |
| facts.query | query_id, allowed_scope_refs, range, limit, cursor? | 已授权标准事实引用/分页；无 SQL 接口，跨插件数据须显式数据订阅 grant |

文件根由宿主把平台 root token（app_data/user_home/configured_directory/env_override）解析成受限 handle；插件提供相对 glob，不能直接读取任意环境变量。用户覆盖路径优先于默认路径；宿主使用文件 ID/大小/mtime/WAL revision 判定来源 epoch。截断/轮转产生新 epoch，并按来源 contribution 对账，不能简单删全部工具历史。

HTTP endpoint 固定 scheme=https、method、origin、路径模板、每参数类型与白名单、允许查询/响应头、auth 绑定。禁止整条 URL 参数、userinfo、fragment、模板目录跳转、双重编码绕过；query 值按编码传递。重定向最多 3 跳，逐跳重新授权，跨 origin 不转发认证；TLS 正常验证。解析 DNS 和实际连接地址均校验，默认拒绝 private/link-local/loopback/云 metadata 地址，DNS rebinding 不得绕过；本地 broker 走独立能力和本机归属检查。HTTP 429 按 Retry-After 在最大退避内处理，读写重试规则分离。

SQLite 使用 native readonly/open flags、禁用扩展加载、authorizer 拒绝 ATTACH/写/危险 PRAGMA，固定快照并通过 progress handler/interrupt 取消；不复制一个正在写入的 DB 主文件而遗漏 WAL。query_id 对应插件声明的 SELECT 与参数 schema，执行前准备/授权，原始路径/SQL 不写日志。

credential 来源列为 secret resource，不能通过 source.read 绕过。宿主提取、DPAPI/钥匙串解密、注入绑定 endpoint，捕获刷新 secret、不交给 guest。standard-v1 不写第一方凭据；credential-maintenance-v1 对绑定的目标/账号/endpoint 显式授权后，由独立 auth.refresh/credential_write 任务执行维护，不向 guest 提供任意明文导出。锁、网络后 CAS、第一方竞争和 AUTH.OUTCOME_UNKNOWN 的恢复见补充合同 §5。

程序身份绑定已确认的安装源/可执行文件路径与校验策略，参数为数组，cwd/环境为最小白名单；禁止通过 program_id=任意路径逃逸。普通插件不获得 shell/任意 exec。网络/SQLite/子进程均须实现真正中断；guest 的 fuel/epoch 不承担取消 native I/O 的职责。

## 6. 调度、门控、并发与恢复

TaskDefinition 完整字段为 `id, tool_id, capability, handler_id, effect:local_read|remote_read|remote_write|credential_write, trigger, interval_ms?, event_ids:ID[], enabled_setting, host_presence:required|independent, concurrency_key, timeout_ms, budget, retry_policy, resource_ids, account_scope`。interval 最小 15 秒；quota 默认 300 秒，增量用量默认 15 秒；source/event 触发去抖 500 ms。插件可申请更慢频率，不能突破宿主硬下限。

优先级为手动查询、可见配额、增量采集、后台估算；最多全局 4 runner、每插件 1、每账号写任务 1。初始 guest memory 总额 64 MiB、fuel 1000 万，默认任务 10 秒、硬上限 30 秒；写任务默认 15 秒。每 invocation 最多 32 次宿主 I/O、总 8 MiB、1000 条日志；单 plugin 日志/磁盘规则见诊断规范。编译并发固定为 1、编译截止期 30 秒、单次 runner 整体截止期 60 秒；TaskContext 的 task deadline 在编译结束后设定并覆盖构造与执行。初始 supervisor 进程内存预算每 runner 512MiB、全局 2GiB，平台实现须说明所用内存计量和强制终止方式。F03/P1 实测校准首次加载、JIT/内存与首屏回归，未通过不能发布；这些初始预算不是性能达标结论。

生产 guest_memory_bytes 是 Store 内**全部线性内存之和**，另限 memory/table/instance 数量和 table 元素总量。Wasmtime memory_size 只限制每个 memory，不能单独实现该总额；F03 使用累计 ResourceLimiter 并以多 memory/table 的恶意组件验证，native RSS/JIT 仍由进程级监督约束。参考 alpha 只设置每 memory 64MiB，不能将其报告成整个 Store 或进程 64MiB。

有效门按顺序计算：插件 active、平台兼容、能力可用、用户 grants、全局能力开关、插件/账号/任务开关、host_presence、writer/lease、deadline/budget。读取暂停只跳过任务并继续读最后缓存；不得触发 Msg::Refresh/clear_cache。签到 host_presence 可 independent，仍受签到开关和写角色控制；不能因关闭 IDE 停止所有已授权独立任务。

任务状态为 queued/running/succeeded/failed/gated/cancel_requested/cancelled/outcome_unknown。每次触发先落 invocation+lease，token 单调递增、每 5 秒续租、15 秒失活（写未决状态不因此清除）。接收事务验证 token，只有影响该任务的 execution_revision、grants/generation 变化使旧结果失效；通知/展示变化不拒绝合法采集结果。已发出的写请求进入 reconcile，不假装已撤销远端操作。

只读暂态失败退避 5/15/60/300 秒并加 0..20% jitter，最多 5 次，之后等待下个正常周期；登录失败等待认证变化，权限失败等待 grants 变化，解析失败等待版本/来源变化。写任务不按只读退避盲重放。休眠恢复按当前状态补一次最新读取，不重放错过的每个 tick；周期跨越的签到先 query 当前周期。

主应用只持有 registry、调度和接收，不执行 guest/JIT。独立 runner 接受长度前缀 IPC（4 bytes little endian，JSON frame ≤4 MiB，先验长度），每次 invocation 新 Store；WASI 不继承目录、环境、stdio 或网络，导入仅本 profile 允许集合。宿主监督编译/加载和 task 分别计时；task 的 epoch 截止期覆盖构造与调用，fuel/线性内存同时生效，主进程兜底杀死 runner 进程树。崩溃/IPC 断裂产生 RUNNER.INTERRUPTED，其他任务/报表持续运行。

## 7. 设置、品牌、语言、视图与通知

SettingDefinition 固定为 `key, tool_id, type:boolean|integer|number|string|enum|directory_ref|secret_ref, default, scope:global|plugin|tool|account|device, title_key, description_key?, constraints, effect:task_gate|notification_gate|parser_config|presentation, target_ids:ID[], restart_required:bool`。tool scope 绑定稳定 tool_ref，面向用户称“应用”；一个插件声明多个应用时，各应用独立设置，不能用 plugin scope 代替。key 以 plugin ID 命名空间存储，账号设置不隐式漫游；secret_ref 通过宿主认证 UI，不回显值；directory_ref 通过宿主目录选择器取得 root grant，不能用普通字符串设置扩大文件权限。constraints 只允许 min/max/min_length/max_length/enum_values/pattern，按类型检查无关字段拒绝；pattern 使用无回溯正则引擎、最长 256 bytes。按 scope 分表，每次修改比较 expected revision，成功增加 revision 并发布配置事件。可见性/通知条件表达式仅允许 all/any/not/eq/exists/lt/lte/gt/gte/in、深度 ≤8、节点 ≤64；比较须类型/单位兼容，字段只可引用已声明的 event/config 路径，不 eval 代码。

ViewDefinition 为 `id, tool_id, slot:overview|quota|models|detail|settings, component:quota_gauge|usage_table|stat|status|action|settings_group, title_key, binding:{query_id,field_paths}, visible_when?, order, action_id?`。只绑定宿主已验证数据和受控查询，不渲染任意 HTML/JS/CSS/远程图片；错误绑定显示标准 unavailable 和诊断 ID。插件图标为受限静态 SVG/PNG，颜色为 light/dark accent/background/foreground 的六位 hex 令牌；宿主校验对比度并回退主题默认值，不让插件改公共页面样式。自定义 WebView 是后续独立隔离 profile，v1 不提供。

Locale 键在插件命名空间，语言选择顺序为精确 locale → base language → defaultLocale → 宿主的“文案缺失”标识；宿主公共文案仍由框架提供。默认资源必须覆盖所有可达语言键，翻译 placeholder 集合一致；复数/数字/日期在宿主采用固定格式器，插件模板只使用声明字段。语言包随签名包离线提供；切语言不重跑任务/清缓存。卸载后可安全展示的名称/品牌/单位快照保留，不执行卸载插件代码。

NotificationRule 固定为 `id, tool_id, policy_kind:quota_percent_low|usage_threshold|checkin_result|custom, event_id, condition, config_schema, default_config, title_key, body_key, fields_schema, action_ids, severity:info|warning|error`。插件声明可配置字段、合法范围、默认值和文案；公共 policy_kind 的字段/语义由宿主 v1 schema 固定，custom 配置仅作用于插件自己的规则，不自动进入全局通用模板。TypedConfig 是通过该规则受限 config_schema 校验的配置对象，禁止远程 schema 引用；cooldown_ms 只存在于有效配置中，避免与规则元数据双重定义。condition 引用宿主解析后的有效配置，不将阈值写死在处理器里。事件由接收成功后的 outbox 发布；事件键为 `(scope,event_id,semantic_state_revision)`，通知键加 rule ID，不用轮询时间生成新身份。condition 使用相同受限表达式，禁止自由脚本；只能读取自己获准事件字段。

宿主 Gate 先更新观察，再决定投递，静音/关闭/系统拒绝均不补发历史。冷却默认 30 分钟，更严重的新阈值可绕过较轻档冷却；当前周期 seen_tiers/watermark 不随 7 日投递 dedupe 记录一起过期。队列最多 1000，业务时间 TTL 5 分钟。OS 回执丢失为 delivery_unknown，不无条件重发；点击 action 重验当前权限，详见补充合同 §4。

### 7.1 通知的统一设置与按应用设置

用户可以在 TokenMe“通知”页统一设置默认规则，也可以在某个应用详情中单独设置。例如全局为“剩余额度 ≤20% 提醒”，Devin 可单独改为 ≤10%，其他应用继续跟随 20%。应用指 tool_ref，不等同于插件包；同包多个应用不相互覆盖。

设置分为两个层面：

- **全局投递总控**：通知总开关、系统许可、免打扰。总开关关闭或免打扰生效时，应用自定义不能绕过。
- **可继承规则**：配额提醒开关、阈值、冷却时长、已支持的投递方式等。全局提供默认值，应用选择“跟随全局”或“单独设置”；应用可单独关闭某类提醒。

固定存储为 `global_defaults[policy_kind]` 与 `tool_overrides[tool_ref,policy_kind_or_custom_rule_id] = {mode:inherit|override, values:TypedConfig}`。inherit 时 values 必须为空；override 只保存用户明确覆盖的字段，未覆盖字段继续继承。false、0 和合法的空数组是显式值，不能用 truthy/falsy 判定是否覆盖。恢复跟随全局清除该规则的覆盖值；切回 override 时 values 初始为空，界面展示当前继承所得值，用户实际修改字段后才写入覆盖，不能复活隐藏旧值或自动复制全量快照。

上述逻辑 key 在实际存储时按补充合同 §4.1 使用 standard/custom 标签及 plugin/rule 复合键，避免公共 policy 名与自定义 rule ID 冲突。

解析顺序固定为：插件合法默认值 → 适用的全局默认字段 → 应用显式覆盖字段。全局标准字段不得改写不兼容的 custom schema。最终再经过全局投递总控、应用/插件启用与系统投递门；全局“默认配额提醒关闭”可被应用明确开启，全局“通知总开关关闭”则阻断所有应用。UI 必须区分“总开关”和“默认规则”，展示每字段来源及最终生效值。普通用户首版只暴露全局/应用两层通知编辑；同应用多个账号使用同一策略，告警状态/去重仍按账号和窗口分别保存，未来账号覆盖须显式扩展。

quota_percent_low 字段为 `{enabled:bool, thresholds_remaining_percent:number[], cooldown_ms:Int, channels:Channel[]}`；阈值 0..100 有限、唯一降序、≤8 项，空数组无阈值提醒。初始 enabled=true、[20,5]、1800000、[system]。v1 Channel 仅 system；合法设置和平台当前投递可用性分开，未授权/平台未实现时保留设置并解释原因。其余公共策略、迁移/保存校验和渠道边界见补充合同 §4，旧用户已有设置不重置。

百分比规则只匹配规范、单位自洽的 used_percent（或可由正 limit/used 推导的值），remaining_percent=clamp(100-used_percent,0,100)。无上限的余额/credits、unknown/unlimited 不套百分比；有合法限额百分比的原生单位可以使用，不凭单位名字一概排除。跨阈值 previous>threshold 且 current≤threshold，一次跨多档只发最严重的一档；不同应用百分比不相加。

修改全局默认立即影响继承字段、保留应用覆盖；只改 notification_policy_revision，不改采集 execution_revision，不刷新/清缓存/签到。有效值未变的应用不重建基线；仅渠道/冷却变化不清阈值历史；阈值/enable 变化才按现有数据无投递地重建基线。无缓存时下一个有效样本建立该次变更基线。投递前重验最新门；已交给 OS 的调用不能撤回，回执不确定规则见补充合同 §4。

F01/F07/F08 必须同时验收：

| 场景 | 期望 |
| --- | --- |
| 全局阈值 20%，A 跟随，B 单独 10% | 剩余从 25% 到 15% 时仅 A 达到阈值；B 后续从 15% 到 8% 才达到阈值 |
| 全局阈值改为 30%，B 显式覆盖 10% | A 生效为 30%，B 仍 10%；未覆盖的冷却/投递方式继续继承 |
| B 恢复跟随全局 | 清除覆盖并显示当前全局值；不因恢复动作补发提醒 |
| 全局默认提醒关闭、B 单独开启 | 总控开启时只有 B 可投递；总控关闭时所有应用均不可投递 |
| A 单独关闭，同插件另一个应用 C 跟随 | A 不投递，C 不受影响；插件启用/停用仍共同受运行门管理 |
| 修改规则、切换模式、通知排队后关闭 | 不请求供应商、不清缓存、不补历史；排队通知按最新门重验 |

## 8. 升级、停用、卸载和迁移

升级：下载并验证 → 计算授权差异 → 等待新增授权 → 暂停新调度 → 取消只读/核对未决写 → 私有状态副本迁移 → 校验 → 单事务激活新 generation → 重新调度。任何一步失败恢复旧 active，保留可解释的失败记录；旧 runner 的提交被 fencing 拒绝。授权差异包含 origin/path/method、来源根、secret/写能力、程序身份和跨插件数据订阅，不能只比较一个 permissions 数量。

每个包声明 state_schema_version 和 migration_from 版本集合；迁移函数输入 `{from_version,to_version,state_snapshot_ref,cursor_snapshot_ref}`，返回新状态/游标副本和校验摘要，无外部副作用。无兼容迁移路径则阻止升级，而不是清状态继续。插件私有状态与公共事实分离，公共索引采用宿主 additive DB migration；禁止插件执行任意 SQL 修改核心表。

回退必须验证旧代码能读取当前私有 schema，否则恢复该次升级前的私有状态与游标快照；公共标准事实保留，并按 occurrence/contribution 身份重放去重。若新插件已做不可逆外部写入，先 reconcile，不能以回退文件声称远端操作已回退。保留至少前一个可恢复版本与状态快照；显式清理前说明能否回退。

停用撤销新调度/句柄，取消读取，保留缓存、设置、历史和未决写记录。卸载移除代码和注册元数据，历史事实/安全展示快照保留；账号秘密由独立选择“移除该插件凭据”处理，不能删除第一方 IDE 的登录状态。删除历史为单独高影响操作，不与卸载绑定；不提供自动删除用户原始日志/数据库的插件生命周期钩子。

迁移顺序固定 Codex（JSONL/累计/模型上下文）→ Qoder CN（独立账号、配额、签到）→ SQLite/WAL + Windows 凭据代表样例。先 shadow 只读事实/报表对账，禁止双跑真实签到或凭据轮换；达到字段与报表差异 oracle 后以工具/账号逐个切换。source_ref、cursor、dedupe_key 与旧工具 ID 建映射，保留原来源 history，禁止用简单新 ID 重新导入全部历史。正式使用前新 profile 演练，再验证已有数据的副本迁移/回退。

## 9. 数据持久化、事件与可诊断性

在现有 usage-index SQLite 中增加 host 表：plugin_installs、plugin_active、plugin_grants、plugin_settings、plugin_tasks、task_invocations、task_leases、source_cursors、fact_contributions、batch_receipts、event_outbox、notification_state、action_attempts、plugin_private_state、migration_journal。表名是实施目标，F02/F06 负责具体迁移和索引；外键均含 scope，不使用“当前默认账号”代替归属。secrets 仅系统凭据库/受保护文件句柄，表里存引用/revision。

task_invocations 记录身份/generation/grants/settings/lease、阶段和开始结束，不存原始参数正文；batch_receipts 关联报表 revision；event_outbox 与事实提交同事务，投递至少一次、消费按事件键去重。日志落盘失败不回滚已经提交的事实，记录 dropped/health 并能从 receipt/outbox 解释；禁止把“没有 end 日志”直接判为“没有入库”。

诊断 scope 分 query_status / read_logs / export；默认本地只读，不允许 execute/credential/raw_file。生产只读接口固定为 `plugins.status(id)`, `plugins.logs(id,range,cursor,limit)`, `plugins.explain(fact_ref|view_ref|invocation_ref)`, `plugins.diagnostics_export(ids,range,redaction_profile)`；CLI/MCP 只是这些接口的适配，不读取任意文件路径。分页默认 100、最大 1000，时间范围默认 24 小时、最大 7 日；导出包默认 20 MiB 上限，超限列出截断与丢失计数。

执行链 `package → registry → invocation → host call → parse → receipt → report_revision → view/notification` 用引用连接；日志的完整字段/预算见 [日志规范](LOGGING_DIAGNOSTICS.md)。UI 以本地化文案解释错误，machine code 不随语言改变。默认不记录 header/body/query/token/文件全文/账号标识/绝对路径。SDK 安全字段校验、宿主补充上下文和导出 allowlist 共同执行；用户提供匿名复现样本时另走明确选择的本地文件，不自动上传。

## 10. 实施交付门

F01 必须把本合同的每个 type/capability/service 对应到 v1 schema/SDK/绑定及正反样例；缺失项是合同缺陷，不能只生成清单后进入生产支持列表。F02/F03 的信任和隔离门通过前，任何第三方包不得加载进真实 TokenMe profile。F07/F08 完成前，不能宣称品牌、开关、语言、通知已动态接入。F10 的写操作与估算门通过前，平台支持列表只列已验收的读取能力。

每票需提交实现、类型/schema、作者样例、拒绝用例、最终组件运行、宿主接入、诊断证据和兼容说明。Windows/macOS/Linux、桌面/CLI/collector、真实设备互通分别标记 validated / build_only / unverified；不将一种环境的成功外推给其他环境。

本文件消除已发现的合同选择空白，允许分票开发；它不代替尚未编写的生产实现、代表性样例和实际安装验收。

## 11. 对现有静态代码的实际切换

| 当前切面 | 必须交付的修改 / 防漏验收 |
| --- | --- |
| `usage-core/src/adapter.rs::SourceAdapter` 的静态 id/name 签名 | 动态代理持有 Arc 元数据；统一接口改为借用 `&str` 或结构化元数据引用，不对每次安装/升级 Box::leak 字符串。现有纯解析 adapter 保留兼容实现，按注册表所有权管理生命周期 |
| `adapters/all::TOOL_IDS/builtin_adapters`、engine 的 builtin_adapters 调用 | 生产统一读 RegistrySnapshot；legacy bridge 只在迁移开关下使用，正式移除后无逐工具 path dependency |
| `usage-quota` 的 registry/inventory/CHECKIN_TOOLS | 同一动态 registry 派生探针/签到能力/设置列表，真实 SDK 服务执行不走旧静态函数表 |
| `usage-index` 的读取入口/游标/同步来源 | 接收事务增加 scope、epoch、contribution 和 fencing 校验；旧 ReadCursor 包为兼容 cursor schema，不把所有新来源强制转换成 u64 |
| 前端 `lib/format.ts::TOOL_ORDER`、`ToolIcon.tsx`、core icons | 名称、品牌和顺序读取 metadata snapshot；资产以宿主 plugin-asset 协议按 digest+资源 ID 提供，只能读取已验证包文件，禁止传绝对路径。任意新 ID 可渲染，未知/缺失资源有通用回退 |
| settings/commands/bridge/types/i18n | 注册式设置和 action command；共享类型使用经过校验的动态 ID，不封死成内置工具 union。插件语言键不并入宿主静态字典，主题/语言切换不触发供应商请求 |
| notify/diagnostics/Report | 只消费标准事件/事实和 metadata，缺失插件仍能显示历史。UI 收到 report_revision 后用同一 snapshot 渲染，避免品牌新版本与旧数据错配 |
| CLI / collector / device sync | 共享 registry/接收合同；远端只导入标准事实和安全元数据，不因同步包出现插件 ID 自动下载安装执行代码；当前机器的 grants/秘密不跨设备复制 |

F11/F12 的硬门为：在禁用 legacy bridge 的构建中安装未知 ID，真实索引→今日汇总→卡片/设置→语言/主题→通知/诊断全部可用；同时搜索并解释剩余静态工具表，历史兼容映射不能重新成为新增工具入口。新增一个插件不得要求修改上述主程序切面。

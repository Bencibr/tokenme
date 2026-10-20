# 生产设计补充合同 0.5

日期：2026-10-20。对象：尚未实现的生产 v1。此文与 [生产合同](PRODUCTION_CONTRACTS.md)共同作为 F01—F12 输入；当前 alpha 不因此获得新能力。审计证据见 [第二轮对抗审计](AUDIT_ROUND2.md)。review 中的 schema/模型仅验证设计约束，不能替代生产 SDK、Wasm/原生服务、UI 或安装验收。

## 1. 上下文、发现和引用的来源

Scope 是带 kind 的互斥结构，不再强制所有调用都有账号/来源：

| kind | 必需字段 | 允许的任务 |
| --- | --- | --- |
| tool | tool_ref、origin_device_ref | source.detect、auth.discover、pricing.resolve；获准的跨来源估算/metric.read/action.run |
| account | tool_ref、account_ref、origin_device_ref | quota.read、checkin.run、auth.refresh；该账号范围的估算/action.run |
| source | tool_ref、account_ref、source_ref、origin_device_ref | usage.collect、telemetry.observe |

禁止填随机“默认账号”绕过发现阶段。没有账号时 quota/checkin 不调度；先以 tool scope 发现候选，再由宿主认证/关联流程确认 account/source，之后创建相应任务。tool scope 不能调用需要账号或来源的服务；source scope 的账号必须与宿主来源登记一致。跨来源 provider 只消费明确订阅的标准数据，tool scope 不是跨账号通行证。

root grant 不等于该目录下任意文件的授权：source 定义、固定 matcher 和本次 task 资源子集共同限制读取。宿主已知或 auth 定义登记为 secret 的文件/DB/slot，其最终文件身份优先保护，不能另造 usage definition/路径别名后用 source.read/peek 读取；root 也不能替代单独 credential grant。框架不宣称能从任意未知文件内容自动识别全部秘密，安装 UI 必须展示具体读取范围，F03/F05 测试 secret-source 别名绕过。

新增通用服务（均在服务调用/字节预算内）：

| 服务 | 请求 | 响应与限制 |
| --- | --- | --- |
| source.peek | definition_id、root_ref、relative_path、metadata_fields | 声明白名单的非秘密元数据、临时 candidate_ref 和 source_revision；只允许 source.detect、授权根和已登记定义，不接受任意文件正文/路径外读取 |
| credential.discover | auth_definition_id | 最多 100 个候选 hint_ref、非秘密别名和登录状态；只允许 auth.discover，秘密由宿主保留，hint 不直接成为 account_ref |
| identity.resolve | kind:session\|series\|derived_record、local_key:string | 当前插件/已授权 scope 命名空间中的稳定 Ref；最多 128 项/次、local_key≤256 bytes。不能指定别的插件/账号命名空间，原始 key 不写日志 |

Ref 是宿主数据引用，**不是授权凭据**。不同于带运行实例/权限/generation 绑定的 resource handle；拿到 Ref 仍须逐次 scope/grant 检查。插件输出的 session/series/price/estimate/value Ref 来自 identity.resolve 或输入中的已授权 Ref；不可自行生成其他账号的 Ref。source_revision 由读取服务给出；TaskContext 新增 output_batch_ref，由宿主为本次逻辑读取批次分配。重试同一已持久任务可复用它，新内容必须获得新批次 Ref。

header.batch_id 必须等于 context.output_batch_ref，否则 DATA.BATCH_ID_INVALID；不是插件可任意选择的另一个 Ref。所有 receipt 规则均在此绑定之后检查。

读取服务响应统一附 HostObservation `{observation_ref,resource_ref,source_revision,observed_at_ms,payload_sha256}`，均由宿主签发/计算并绑定授权 scope；source/query/http/broker 的业务响应之外都带此封套。pricing 的 source_ref 和派生输入来源引用该 observation 或已登记的标准事实，不能凭空制造来源。原始 secret/正文及其摘要不进入正式日志/导出，日志只关联临时/授权 Ref；pricing 的规范化价格快照另行保存。

TaskContext 同时携带已验证的 handler_id 和 operation_ref:Ref?（action.run 时非 null）；SDK 根据清单 task_id→handler_id 映射分派，不能让 handler_id 成为传入任意函数名的入口。action.run 输入 `{action_id,params,user_gesture_ref:Ref?}`，params 通过清单 action schema 校验；需要人工触发的动作必须有宿主 UI/CLI 生成、绑定 action/scope/参数摘要且单次使用的 user_gesture_ref。输出 `{status:succeeded|failed|needs_login|queued|outcome_unknown,operation_ref:Ref,message_key,fields:TypedMap}`；operation_ref 复制该宿主上下文值，排队不伪称已经成功，远端写的不确定状态延续 check-in/credential 的核对规则。

source.detect 的 hint 只是候选；认证/用户关联由宿主决定 canonical account。新账号凭据槽在 account 确认后创建。初始化只有宿主元数据登记，没有任意 guest init/unload hook；guest 每任务新 Store，监听、定时、连接均为宿主租约资源，停用时由宿主释放。私有状态首次通过 state.get/cas 创建；除 host:migrate 外不执行隐藏生命周期任务。

## 2. 修订、批次、待补全和累计值

### 2.1 不可混淆的缺失值

UsageFact 的 counts 改为 TokenCounts|null；credits-only/cost-only 事实不得填全零 counts 冒充实测 Token。counts、native_credits、vendor_cost 至少一项非 null。显式全零 counts 表示供应商确实报告零 Token。TokenCounts 的四阶段总和≤MAX_SAFE_INTEGER，reasoning≤output。

ts_ms 为 Timestamp|null；null 仅允许 completeness=partial，进入 pending，不进入时间报表。partial 的其他标准数值持久保存后才可提交游标，后续同一 occurrence 的更高 revision 补齐时间；未知时间不取扫描时刻。final 必须有时间；model=null 仍可代表厂商确实没有模型，按 unpriced/unknown 显示。pending 每 source 最多 10000 条，达到上限返回 DATA.PENDING_LIMIT、整批回滚且游标不动，不静默丢弃。

Timestamp 结构上为 0..253402300799999 的整数 ms。用量发生时间、配额 sampled_at 不得晚于宿主 observed_at+300000；超出进入 CLOCK.SKEW/隔离，不计入未来报表；resets_at/deadline/有效期可为未来但须通过各自关系约束。执行截止期和耗时由宿主单调时钟强制，墙钟变化不延长权限/任务寿命。

### 2.2 contribution 和 batch 的冲突规则

revision 针对同一来源/epoch/occurrence 的 contribution，不是多个来源共享的全局计数。接收按以下顺序执行，所有变更在单事务内：

1. 验证调用者仍有 scope 访问权、包/工具归属和输出类型。
2. 已存在 batch receipt：相同 batch_ref、source scope、规范 payload SHA-256 返回原 receipt，不重新写入；任一内容不同返回 DATA.BATCH_ID_REUSED。此分支不执行新数据写入，不能以旧 receipt 绕过新 grants。
3. 未提交批次检查 generation、execution_revision、grant_revision、lease fencing token 与 expected_cursor_revision；任一过期返回 DATA.STALE_CONTEXT，事实/游标/outbox 均不变。
4. 同 contribution 较低 revision 记 stale/no-op；同 revision 同标准 payload 记 duplicate；同 revision 不同 payload 返回 DATA.REVISION_CONFLICT、整批回滚。更高 revision 才能修正；final→partial 返回 DATA.FINAL_REGRESSION。
5. 按字段优先级合成 occurrence。优先级同级出现不同非空数据时，将该字段标 conflict 并从当前合计排除，保留原贡献和冲突原因；不能任选先到/后到者，也不把未知显示成 0。已汇总值被冲突撤出时同事务修复 rollup，UI 标明 coverage/conflict。

payload 摘要使用生产 API 的规范编码：UTF-8 无 BOM、对象 key 按 Unicode code point 排序、紧凑 JSON、数组保序、所有可空字段显式 null；Decimal 不改写其合法字符串。摘要消息为 tokenme.usage-batch/v1、LF、规范 header/facts/retractions 字节；trace/耗时等非业务诊断字段不入摘要。F01 提交编码的跨语言黄金字节，不能各自使用本地序列化默认值。

usage.collect 输出增加 retractions 数组：每项为 `{identity_domain, occurrence_id, session_ref, target_revision}`，只撤回当前 source/epoch 自己的 contribution，target_revision 必须匹配，不能撤回别的来源/账号。facts+retractions 总计≤1000，同批次重复/相互冲突目标拒绝。来源轮转/截断的批量撤回仍由宿主来源状态机生成。

### 2.3 cumulative 必需证据

usage_form=cumulative 时新增 cumulative 对象 `{series_ref, sample_index:Int, reset_seq:Int, starts_at_zero:bool, reset_evidence_ref:Ref?}`；per_call 时必须为 null。序列按 source/epoch/series/reset_seq、sample_index 排序，禁止仅按 ts_ms 排序（同一毫秒可有多行）。同序号不同快照走 revision 冲突规则。

未知起点的首个样本只建立基线，例如快照 100→150 只产生 50；starts_at_zero=true 或 reset_seq>0 均要求授权读取的非 null reset_evidence_ref，不能只凭 bool 从 0 计算首样本。无证据负差拒绝，不把下降当负消费或新会话。计数以 TokenCounts、原生 credits、实报金额分别差分；迟到快照按相邻区间重新计算并对账，revision 修正不能重复创造同一个差量。F06 需有同毫秒序列、未知基线、跨页重放、reset 和迟到修正 fixture。

## 3. 配额样本、缓存与百分比

quota.read 保留顶层 status，新增 window_set:complete|partial；每个 window 新增 state:bounded|unlimited|unknown。status=ok 可混合窗口；status=unlimited 仅在所有当前窗口无限或供应商确认整体无限时使用；login_required/unavailable 必须 windows=[]，只更新健康状态，不删除最后成功样本。unlimited 不保留有效数字作当前值；旧 bounded 数字仅作历史，不能继续触发通知。

complete 的 ok 样本是已确认完整窗口集：遗漏窗口标 retired，保留历史；partial 只更新给出的窗口，缺失窗口不删除。空窗口 complete 必须是插件确认“当前没有可用窗口”，不能把网络/解析失败包装成该成功结果。

若 used、limit、remaining 可共同计算，则必须自洽：limit>0 时 used_percent=100*used/limit；缺 used 而有 limit/remaining 时可由两者推导。remaining 与 max(limit-used,0) 的误差≤0.000000000001 原生单位；百分比误差≤0.01 percentage point。自报值与计算值冲突整次样本拒绝为 DATA.QUOTA_INCONSISTENT。limit=0 不作分母；金额/积分/请求等单位各自独立，不能盲套“100-used”。只有合法规范 used_percent 才可派生 remaining_percent=clamp(100-used_percent,0,100)。

sampled_at≤valid_until，宿主实际 effective_valid_until=min(valid_until,sampled_at+300000,observed_at+300000)。因此容忍范围内的未来 vendor 时钟也不能延长宿主五分钟 TTL。进程内失效用单调时钟；重启检测墙钟回拨/不可信时只把旧值显示为 stale，不恢复成 fresh。sample time 更老不覆盖 last_good；同时间采用宿主 invocation 序号防晚到。失败/登录状态与最后成功值分别保存，过期样本不触发新告警。

## 4. 配置版本、继承、通道与通知投递

### 4.1 采集和通知不能共用失效版本

TaskContext 分开记录 settings_snapshot_revision（本次配置快照）与 execution_revision（只由影响本任务 parser_config/task_gate/resource 的字段产生）。FactReceiver 只按 execution_revision、grants/generation/lease 失效判断。通知单独使用 notification_policy_revision，主题/语言使用 presentation_revision。修改阈值、通知方式、文案或主题，不令合法采集批次失效，不推进/重置采集游标，也不清配额缓存。

插件只能登记自己命名空间的配置；scope=global 表示该插件自己跨应用的设置，不获得写 TokenMe 全局配置的权利。tokenme.* 保留给宿主；公共通知模板由宿主按 standard policy schema 提供，不能由任意插件抢占。tool scope 目标必须属于该插件获准的 tool_ref。

全局/应用的稀疏覆盖与总控规则延续生产合同 §7.1。通知配置保存采用对应记录 expected_revision 的 CAS；冲突返回 SETTINGS.REVISION_CONFLICT，不整体覆盖全局快照。仅有效字段变化才增加对应策略版本。全局更新后，有效值没变的应用（相应字段已覆盖）不重建观察基线；仅 channels/cooldown 变化也不清阈值历史。阈值/enable 变化才按现有数据无投递地建立新基线。

策略存储键采用带 kind 的复合键：公共规则为 (standard,policy_kind)，custom 为 (custom,plugin_id,rule_id)；不能让一个名为 quota_percent_low 的 custom rule 覆盖公共模板。外层仍按 tool_ref 隔离。

### 4.2 三种公共策略有明确字段

所有公共策略含 enabled:bool、cooldown_ms:0..604800000、channels。v1 Channel 的合法线格式仅 system；结构合法性与操作系统当前支持/授权状态分开。未授权或平台未实现时仍可保存/继承 system，UI 显示不可投递原因；不把默认配置判为非法，也不静默改成其他通道。后续新增通道须升级 host 支持，不接受插件任意 email/webhook URL。

| policy_kind | 追加字段与默认 |
| --- | --- |
| quota_percent_low | thresholds_remaining_percent 为 0..100 有限数、降序唯一、≤8 项；默认 enabled=true、[20,5]、30 分钟、[system] |
| usage_threshold | metric 固定 observed_tokens、thresholds_tokens 为正安全整数升序唯一≤8 项、timezone 为宿主已知 IANA 时区；按该时区本地自然日、不含估算/credits。默认 enabled=false、空阈值、宿主当前时区、30 分钟、[system] |
| checkin_result | states 为 claimed/already_claimed/login_required/outcome_unknown/failed/not_available 的唯一子集；默认 enabled=true、[claimed,login_required,outcome_unknown,failed]、30 分钟、[system] |

custom 规则的 config_schema 限于宿主支持的类型/约束子集：禁止远程 $ref、动态引用、递归引用、任意代码和不受限正则；schema≤64KiB、深度≤16、节点≤1024。公共字段不得缩窄到与宿主默认值冲突的类型。schema/默认值/当前覆盖值在安装、升级、设置保存时均校验；不兼容旧覆盖时保留原记录、禁用受影响规则并提示迁移，不能静默重置为开启。

### 4.3 阈值状态、冷却和轮询周期

观察键至少含 tool/account/window或action/rule/业务cycle；同插件不同应用、同应用不同账号/窗口各自独立。cycle 首选稳定厂商周期 ID；没有该 ID 时由宿主持久维护 cycle_ref，仅在明确窗口重置/新周期证据时递增，不能把每次 rolling resets_at 的轻微变化当新周期。

seen_tiers、last_observed_event_revision、cycle_ref 独立于投递状态保存；静音/关闭/空通道/系统拒绝时仍推进观察。没有 previous 的首个样本建立基线、不追发已有低额度；随后才检测跨档。一次跨多个 quota 档只选最严重的一档，同周期同档只提醒一次。更严重的新档可绕过较轻档 cooldown：25%→15% 发 20% 档，5 分钟后 15%→3% 仍可发 5% 档。同档抖动不重复；明确新周期后重新观察。7 日投递 dedupe 保留不等于删除当前周期 seen_tiers/stream watermark；当前状态保留到周期结束和重放水位安全推进之后。

只有真实新样本进入通知状态机；过期样本、历史导入/rollup 重算、重复 receipt 不产生 live 告警。事件 TTL 5 分钟按宿主确认的业务有效时间计算，不以重新入队时间续命。规则/模式变化引起的 baseline 只影响该有效策略变化的 scope，不补历史。

### 4.4 不能保证 OS 通知的 exactly-once

投递状态为 observed/suppressed/eligible/queued/delivering/delivered/failed/delivery_unknown。投递前用最新策略/总控/grants 检查并原子认领稳定 notification_key；policy revision 变化时重验，未开始的通知不可绕过新关闭状态。进入 delivering 后已提交 OS 的操作不能被撤回；UI/文档不得承诺关闭总开关会撤销已送交 OS 的通知。

OS 调用成功但本地回执落盘前崩溃，恢复为 delivery_unknown。平台支持稳定 ID 查询/替换时按同 ID 核对；不支持时不自动重发，保留可解释状态，避免重复。明确“未投递”的暂态错误才可在原 TTL 和正常门下重试；不能把系统拒绝与用户静音混为可重试网络失败。F08 必测 OS 回执丢失、进程崩溃、队列中关闭、冷却内严重程度升级。

## 5. 凭据维护与读写隔离

v1 注册两种权限 profile：standard-v1（不向 guest 返回秘密、不写第一方登录文件）与 credential-maintenance-v1（增加明确 target/slot/endpoint 的受控刷新/写回）。后者逐账号显式授权，不因 official 签名自动绕过，也不提供任意 secret export、shell 或文件写入。

auth.refresh 是独立 account-scope 的 credential_write 任务；quota.read 不得把隐藏 POST 刷新伪装成 remote_read 重试。credential.acquire 仅读现有槽，过期返回 AUTH.REFRESH_REQUIRED；scheduler 只有在注册/获准且启用的 auth.refresh 下执行刷新，再重新调度读取。doctor 不调度刷新、签到或主动模型测试。

credential.exchange 仅 auth.refresh 可用，输入 flow_id、old_handle、expected_credential_revision；flow 描述绑定的 refresh endpoint、请求 secret 注入、响应 secret selectors、第一方目标格式/锁/CAS 规则。host 在锁内读 revision、释放锁后做网络请求、再拿锁重读/CAS/原子写回；新 secret 被宿主捕获，不出现在 guest 正文或日志。第一方客户端不遵循同锁且供应商刷新会失效旧 token 时，CAS 不能证明没有远端竞争：冲突转 AUTH.OUTCOME_UNKNOWN，重新读取/核对当前凭据，不覆盖、不盲刷新。无安全核对路径时停用该自动维护流程并显示需重新登录。

服务逐次检查 effect 与 grant：source/list/peek/query 为读取；GET/HEAD 默认为 remote_read，POST 等默认为 remote_write。确有非变更 POST 查询时，endpoint 必须显式声明 remote_read、固定 operation_id/参数 schema，且该 method/path/operation 在用户授予的只读请求范围内；不能仅按响应看起来只读降低权限。state.cas 仅写本插件私有状态；credential.exchange 需要 credential_write。host:migrate 只绑定 staged snapshot 的 state.get/state.cas，主 profile 状态和网络均不可见，修正旧版不存在的 state.read/state.write 名称。

## 6. 价格/估算和数值边界

pricing.resolve 的 per_units 必须是正 Decimal；金额可为 0（免费价格），除数不能为 0。FX rate 同样>0，方向、币种和有效期匹配。内部计算精度至少 100 位有效数字，派生金额落盘时按 half-even 量化为 12 位小数，并保留原始价目/输入可重算（例如 1/3 存 0.333333333333）；整数部分超 Decimal 18 位返回 DATA.AMOUNT_RANGE，不截断/溢出/转 float。每币种汇总后再按版本化币种 minor unit 做展示舍入，不对逐行先取整分。实报 vendor_cost 不套估算公式。

derived_record Ref 与 input_revision 共同定位价格/估算版本；较旧输入版本的迟到结果不能覆盖较新派生结果。同一覆盖区间的 observed 与 estimated 记录不相加；Token 估算、价值估算和原生 credits/实报费用的 basis 必须保留，未知价格返回 unpriced。

聚合计数使用精确整数，超过 MAX_SAFE_INTEGER 时线格式为 `{status:out_of_number_range,exact_value:string,numeric_value:null}`，exact_value 为非负十进制整数、最多 38 位；范围内为 `{status:ok,exact_value:string,numeric_value:Int}`。超过该计数上限返回 DATA.COUNT_RANGE，不伪 0/截断。比如 MAX_SAFE_INTEGER+2 的精确值为 9007199254740993；不能经过现有 f64 桥再称精确。F06/F07 必须实现并测试此边界及缺测 coverage；这不要求把超大数作为 JS Number。

查询范围内没有任何可用 Token 实测时返回 `{status:unavailable,exact_value:null,numeric_value:null,reason:NO_TOKEN_MEASUREMENT}`，不是对空列表无条件求和得到 0。只有宿主已确认该范围存在可读 Token 来源、完整扫描且没有事件时，才可返回实测空范围的 0；credits-only/pending/未登录不能提供这项证据。coverage 由来源登记、扫描 receipt 和 pending 状态产生，不能由插件随意声称 known_empty。

## 7. 依赖顺序、全面迁移和冻结门

F03 先于需要实际 host:migrate 的 F02：F01 → F03 → F02 → F04 → F05/F06 → F07 → F08/F10 → F09/F11 → F12。F02 的正反升级测试必须使用真实隔离 runtime；不以模拟迁移函数替代。F05 的写凭据服务依赖 F04 的租约/fencing。F06 可在 F05 mock 只读输入上先开发，其生产完成仍须通过真实来源验证。

F02 实现 staged snapshot 的最小 state.get/cas 绑定并注入 F03 runtime；不依赖尚未交付的 F05 通用网络/认证服务。F05 再提供普通任务的完整服务实现，因此不会形成 F02→F05→F04→F02 的隐性循环。

F11 分为 F11.1 代表性迁移和 F11.2 全量适配/策略迁移。F11.2 必须覆盖 adapter registry ∪ quota probe registry ∪ CHECKIN_TOOLS ∪ 品牌/估算/价格/主动与被动 telemetry/action 的所有现存来源；包含没有 SourceAdapter 的配额产品。逐个记录旧入口、plugin/tool ID、能力、授权、平台、配置/缓存/游标映射、匿名 oracle 和切换/回退证据。只有 F11.1 通过不能删除 legacy bridge，也不能让其余工具继续依赖静态 crate 后称全面迁移完成。清单见 [迁移清册](MIGRATION_INVENTORY.md)。

冻结条件是 F01 同时交付完整 v1 schema/WIT/SDK、跨语言规范编码字节、每能力正反样例和本轮 review 向量的一致实现；contract 覆盖必须逐项可追踪。生成文件缺失时只称“设计输入已明确”，不能称“生产协议已冻结”。review 模型通过只说明所列设计约束可确定执行，不等于 F01 或生产验收已通过。

## 8. 清单映射与演进的精确边界

manifest 元数据字段 camelCase、执行 payload snake_case 的映射由生成类型逐字段声明；不能递归修改字典 key、语言键、JSON selector、供应商 body 字段或 config_schema.properties 中的业务键。defaultConfig/values 的键遵循所指 config schema 原样保留。未知能力 ID 可以作为 optional descriptor 被跳过注册；这不允许未知结构字段被接受。未知 required capability 拒绝激活；未知字段在两种情况下都拒绝。

schema/reference/asset 校验全部有资源上限；产物处理前先检查 UTF-8、字节数/深度/文件数，不把最大字节长误用为字符数。所有插件 ID 对将要使用的文件名也执行 Windows reserved/alias 检查，或使用宿主安全 store_key 映射；不允许合法清单直到落盘才因 CON.* 等名字失败。以上生产要求不宣称当前 alpha 已完整实现。

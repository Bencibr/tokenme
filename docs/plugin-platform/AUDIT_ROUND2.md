# 第二轮文档核验与对抗审计 0.5

日期：2026-10-20（Asia/Shanghai）。隔离工作区分支 codex/plugin-platform，代码基线 e1cf55d。范围：最新总体/生产/SDK/日志/实施文档，包括后来增加的“全局通知默认＋按应用覆盖”。本轮只改内部设计和 review 资料，不改生产代码或参考 kit。

## 1. 结论

**0.4 仍有影响实现的设计缺口，上一轮“设计已闭环”的说法偏乐观。** 再次审计确认十二类问题：有些规则互相冲突，有些缺少必要字段或恢复状态，有些实施范围/依赖没有承接。不能用“实现还没开始”解释这些缺口，也不能把能运行离线参考样板当作全部生产设计已经冻结。

0.5 已把本轮发现落到[生产合同](PRODUCTION_CONTRACTS.md)、[补充合同](DESIGN_HARDENING.md)、[实施票](IMPLEMENTATION_SPEC.md)与[全量迁移清册](MIGRATION_INVENTORY.md)，并增加严格 schema 投影和可执行设计 oracle。

当前准确状态：**已修订本轮列出的设计问题，可继续实施；完整生产协议冻结仍以 F01 的全量 Schema/WIT/SDK/跨语言黄金字节和用例一致实现为门。** 生产安装/运行器/真实服务/索引/UI/通知/迁移/平台验收仍未完成，不标通过。

本轮追加用户需求“将Agent代理给其他工具”，原0.5未覆盖。已补[网关设计](AGENT_GATEWAY.md)、[WorkBuddy→Claude Code场景](WORKBUDDY_CLAUDE_CODE.md)、[两模式评估与任务委派研究](AGENT_MODES_EVALUATION.md)及F13/G01—G07。用户明确A模型网关/B任务委派，模式边界已确定；已核验客户端官方入口、本机安装代码和社区协议源码，未完成本机实时任务/模型调用及G01冻结，不据此宣称已支持。

## 2. 方法与证据类型

修改前将已有文档逐文件保存为快照，记录 SHA-256。对每项反例引用原文行号和摘要，给出输入、旧规则允许的坏结果或无法决定的行为，再给出 0.5 唯一规则。不会把一个假设的实现写法包装成已运行的漏洞。

- 修改前文件清单与摘要（本地记录：`prototypes/plugin-platform-v1/artifacts/audit/round2/before-manifest.json`）。before 目录保存原文件。
- 十二项反例及原文引证（本地记录：`prototypes/plugin-platform-v1/artifacts/audit/round2/counterexamples.json`），由[核验脚本](review/counterexamples.py)生成。
- [设计 review 说明与运行入口](review/README.md)、[可执行模型/独立期望](review/verify_contracts.py)。
- 本轮设计 review 结果（本地记录：`prototypes/plugin-platform-v1/artifacts/audit/round2/design-review.json`）：含 runId、文档/schema/模型摘要和每次独立历史。

反例属于 document_counterexample，**不是**运行中的 TokenMe/供应商攻击 PoC；review 报告强制 production_runtime_validated=false。归档中的首轮真实工具缺陷红/绿结果继续有效，但不冒充本轮生产边界已被实测。

## 3. 本轮发现和处理

| ID / 级别 | 0.4 反例与影响 | 0.5 确定的规则 / 验收归属 |
| --- | --- | --- |
| DA01 / P1 | Scope 强制 account_ref，而发现发生在账号尚未识别时；batch/session/派生 Ref 要求宿主签发，却没有分配路径 | tool/account/source 区分；source.peek/credential.discover/identity.resolve；output batch/operation 和 HostObservation 由宿主签发；F01/F03/F05 |
| DA02 / P1 | 同 revision 不同数值可被先写/后写两个策略各自解释；同 batch ID 无论新 payload 都返回旧 receipt | contribution 修订冲突整批拒绝；batch receipt 绑定 scope/规范 payload；retraction 只撤本来源；F01/F06 |
| DA03 / P1 | credits-only 被迫填写零 counts；未知时间没有可序列化 pending；累计值没有序号/起点/重置证据字段 | nullable counts/时间、持久 pending 与游标同事务；cumulative 的 series/sample/reset/evidence；F01/F06/F11 |
| DA04 / P1 | 用户改通知阈值会增加 settings revision，导致合法的进行中采集结果被拒绝；反复改设置可持续饿死摄取 | execution/policy/presentation revision 分开，只有影响该任务的变化使数据失效；有效值没变不重建基线；F04/F06/F07/F08 |
| DA05 / P1 | login_required 返回空窗口可能抹掉 last_good；partial 窗口遗漏未定义；used/limit 与百分比冲突、超长 TTL 没有宿主限制 | 健康与 last_good 分开；complete/partial、逐窗口 state、数学自洽、五分钟 TTL clamp；F01/F05/F06 |
| DA06 / P1 | 20% 告警后 5 分钟进入 5% 档可能被 30 分钟冷却压住；OS 已收到但回执丢失的恢复可能重复投递 | 严重档升级绕过较轻冷却、按账号/窗口/cycle 观察；delivery_unknown 不盲重发；业务 TTL/重放水位；F08 |
| DA07 / P2 | 默认 system 通道在未授权/未实现平台被判非法；usage_threshold/checkin_result 只有名字、无完整公共配置 | 配置合法性与投递可用性分离；三个公共策略明确字段/默认；受限 schema、旧覆盖兼容性/CAS；F01/F07/F08 |
| DA08 / P1 | 高权限凭据维护 profile 未定义；配额读取可能隐含刷新写操作；迁移要求 state.read/write，而服务表只有 get/cas | standard/maintenance profiles、独立 auth.refresh/credential_write、第一方竞争 outcome_unknown；迁移只绑定 staged get/cas；F02/F04/F05 |
| DA09 / P1 | 合法 Decimal=0 可作为 price per_units，产生除零；合法单条计数相加超过 JS 安全数，经 f64 会舍入 | 正分母/FX、明确高精度/half-even 存储；精确总数字符串+范围状态，不传近似 Number；F01/F06/F10 |
| DA10 / P1 | F02 的实际迁移依赖 runtime，却与 F03 同列在 F01 后；写凭据服务缺 lease 前置 | F03→F02→F04；F05 依赖 F04；F02 自带最小 staged state 绑定，避免隐性循环；实际文档 DAG 有自动校验 |
| DA11 / P1 | 票只迁 Codex/Qoder 代表路径，剩余适配器和 pricing/estimate/telemetry 没有明确 owner；quota-only 产品易遗漏 | F11.1 代表性＋F11.2 全量清册，F12 依赖 F11.2 后才关闭 legacy；并集包含 Copilot/Gemini 等；F11/F12 |
| DA12 / P2 | SDK 描述 init/unload hook，但 WIT/调度未定义；manifest 大小写映射可能错误重命名配置业务键 | 无未注册 guest hook，宿主释放资源；generated 字段映射不改业务字典/selector；handler/action 入口/上下文明确；F01/F03/F09 |

## 4. 可执行设计检查覆盖

review 的三个严格投影为 [Scope](review/scope.schema.json)、[通知配置](review/notification.schema.json)、[UsageFact](review/usage-fact.schema.json)，使用 jsonschema 4.25.1。它们不是完整的生产 manifest/API，也没有注册到 alpha。

语义用例覆盖：

- 无账号发现、账号/来源能力的 scope 限制、未知字段拒绝。
- 全局/应用稀疏继承、字段来源、false/0/空数组、恢复继承无隐藏值、非法渠道/布尔阈值。
- 冷却内严重升级、多档只发最严重、同档抖动、静音不补发、首样本基线、不同窗口隔离、空通道仅观察。
- OS 回执丢失的未知状态、配置版本与采集隔离、grant/generation/lease/cursor 失效。
- 同 revision/payload、旧修订、final 回退、batch scope/payload 绑定。
- 规范 UTF-8 黄金字节与非 BMP 键排序（不能使用 JS 默认 UTF-16 排序替代）。
- credits-only 的 null counts、实报零、pending 时间、未来时间、UTF-8 字节上限、四阶段总和/reasoning。
- 累计同毫秒序号、未知起点/真实零起点、无证据下降。
- 原生单位百分比、自报值冲突、健康失败保留、partial 窗口/TTL。
- 价目零分母/指数拒绝、循环小数的存储舍入、超安全整数精确总数。
- 只读任务不能隐式刷新、维护 profile、迁移只写 staged state、未知服务拒绝。
- 实施文档真实依赖图，以及现有 adapter 与 quota-only 产品的迁移清册覆盖。

独立期望包括：credits 3.5 对应 counts=null；累计 100→150 未知基线仅计 50；50/200 推导 25% 而不是自报 75%；MAX_SAFE+2 精确值 9007199254740993；1/3 派生金额存 0.333333333333。没有从真实执行结果自动反填期望。

运行方式见 review/README；每次报告关联当时全部文档/投影/模型 SHA。运行后若继续修改文档，应重新运行设计检查更新结果。测试模型通过仅说明这些约束具有确定结果；不能证明实际并发/IO/OS 实现已遵守。

新增 Agent 网关目前完成草案和人工对抗场景，不在上述可执行模型的语义覆盖内。报告收录其文档 SHA 只用于关联版本，不代表 G01—G07 或任何 Agent 兼容性已通过。

## 5. 当前仍未通过的门

1. F01 完整 v1 生成合同、SDK 绑定和 TestKit 尚未交付。本轮只提供设计输入和严格投影，不能将“生成正式 schema”视为已经发生。
2. F03 的 native 内存/JIT/累计资源、IPC、取消及恶意 Component，F02 的签名/handle 安装/真实升级 crash matrix，仍须实际实现和实测。
3. 凭据维护 profile 必须在真实锁/第一方客户端并发/厂商刷新行为中证明；CAS 本身不构成远端 exactly-once。
4. F06/F08 的事务/outbox/OS 投递恢复、全局与应用策略 UI 以及实际索引/汇总/诊断，仍须接入生产 TokenMe。
5. F11.2 清册全部为 pending；跨平台、CLI/collector、真实设备和安装保留数据仍未验收。主线更新后的接口差异仍须在隔离分支比较集成。
6. 新增F13/G01—G07尚未实现。A按WB-P0、B按WB-D0—D3逐项实测接口、认证、会话隔离、流/长任务、取消/未知、执行权限及去重。本机Gateway会话元数据均陈旧，不能据代码存在标ready；通用设计模型不替代实时门。

## 6. 追加的双模式对抗核验

| 反例 | 修订规则/验收归属 |
| --- | --- |
| agent_submit返回后MCP连接关闭，通用“断线取消”规则让已接受任务消失 | A模型流断线取消，B控制连接断开不取消job；同consumer重新查询；G01/G03/WB-D2 |
| B终态后正文立即释放，后续agent_result永远取不到结果 | 显式有界/有TTL的结果内存缓冲；过期/重启无法恢复标result_unavailable；G03/G06 |
| 拒绝permission请求但Agent原有预授权工具仍能写文件 | 拒绝请求不等于只读；实际强制受限模式或明确trusted_agent授权；G01/G04/WB-D2 |
| 复制社区全局session+/clear，两个consumer仍共享上游状态 | consumer/account/workspace会话隔离，不接管GUI/全局会话；G03/G04/WB-D1 |
| 使用陈旧sessions端口，实际连接到已复用该端口的别的进程 | 心跳+OS进程/监听归属+版本/来源联合验证，不仅检查127.0.0.1；WB-D0/G03 |
| Claude Code日志和WorkBuddy网关按不同tool各入一条，公共总量翻倍 | 消费主体与调用客户端分维度，proof/attribution关联，未知重叠不叠加；G05 |

本节为文档反例和手工合同检查，不计入46个核心可执行设计用例，也不宣称真实攻击或运行验收已通过。

本轮不新增后端平台前置条件，不安装或发布插件，不推送代码。仍在物理隔离分支工作，原工作区与其并行变更不被覆盖。

## 提交版本的复核入口

本地完整快照和运行报告未纳入Git；源码提交提供[反例引文夹具](review/fixtures/README.md)及[原文行号与摘要映射](review/fixtures/source-mapping.json)，可独立重跑十二项反例。夹具只提取审计使用的原句，不是原文完整副本。设计检查按review/README运行；本页历史分支名对应审计发生时的工作区。

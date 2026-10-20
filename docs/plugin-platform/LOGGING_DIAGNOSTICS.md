# 插件日志、健康状态与问题定位规范

版本：0.5（生产设计与参考实现边界）
日期：2026-10-20（Asia/Shanghai）
关联：[实施规格](IMPLEMENTATION_SPEC.md)、[总体方案](PLAN.md)、[开发语言与 SDK](LANGUAGE_AND_SDK.md)。本文定义生产实施目标；参考工程已提供组件运行的结构化日志和错误证据，完整健康界面、生产日志服务和排查命令仍按实施票交付。

生产 API/权限、默认分页、导出限制和事件事务以[生产合同第 9 节](PRODUCTION_CONTRACTS.md)为准；当前已执行验证见[审计](AUDIT.md)和[参考证据](REFERENCE_EVIDENCE.md)。

修订与新反例见[补充合同](DESIGN_HARDENING.md)和[第二轮审计](AUDIT_ROUND2.md)。设计 review 模型不计为生产诊断/OS 通知验收。

新增 [Agent 网关](AGENT_GATEWAY.md)沿用元数据诊断边界，并增加 consumer/request/session/provider 关联、dispatch/取消/核对状态、首事件耗时和用量去重解释。提示词、输出 delta、工具参数、header/密钥/上游会话原文及其摘要不进入正式日志或普通导出；网关业务传输与诊断读取的权限分别验证。

## 1. 用户应当能回答的问题

插件没有数据时，能够直接看出：未加载、版本不兼容、未发现工具、未登录、被哪个开关暂停、请求失败、解析失败、数据被去重、当前作用域未包含，还是页面绑定失败。

一次异常能够关联插件版本、可执行模块摘要、宿主版本、运行实例和失败阶段。开发者能够用同一错误码、诊断 ID 和匿名样本定位原因，不要求用户自行拼接多个日志文件。

所有插件使用 TokenMe 提供的日志与诊断服务，不自行选择用户目录写文件、不各建一个不兼容的日志系统。诊断功能默认本地运行，不依赖云平台。

## 2. 复用现有机制

本分支代码依据：

- `crates/usage-quota/src/diagnostics.rs`：已有带 run/probe/request 关联的类型化配额跟踪，仅记录元数据。
- `crates/usage-core/src/adapter.rs`：已有 `read_note` / `drain_read_notes`，为读失败或无数据提供诊断线索。
- `crates/usage-index/src/ingest.rs`：已有按来源的读取、摄取与游标控制；扩展为结构化批次计数和拒绝原因。
- `apps/tokenme-bar/src-tauri/src/logging.rs`：已有文件轮换、脱敏和有界诊断读取。
- `apps/tokenme-bar/src-tauri/src/diagnostics.rs`：已有授权诊断边界；插件诊断接入该边界，不自行开放不受控 HTTP/MCP 服务。

现有 `panel.log`、`quota.log`、`scan.log` 和签到记录在迁移期保留。为旧事件附加关联标识或建立兼容映射；不要丢失原有证据，也不要把同一结果双写后当作两次任务。

## 3. 三类记录

| 类型 | 记录内容 | 用途 |
| --- | --- | --- |
| 运行日志 | 结构化事件、级别、阶段、计数、耗时和标准错误码 | 查看发生了什么 |
| 执行轨迹 | 父子 span、任务实例、宿主调用、批次与报表修订引用 | 定位链路在哪里中断 |
| 健康快照 | 最后成功、最新有效数据、有效开关、下一次任务及当前异常 | 用户无需翻日志即可判断状态 |

运行健康统计与用户 Token 用量属于不同数据域。HTTP 耗时、错误次数、日志丢弃数等不进入用户用量汇总，也不产生供应商消费记录。

## 4. 结构化日志合同

| 字段 | 规则 |
| --- | --- |
| `schema_version` | 日志 schema 版本，与插件版本分别管理 |
| `timestamp` / `observed_timestamp` | UTC 事件时间和宿主接收时间；界面按用户时区格式化 |
| `sequence` | 宿主运行实例内递增；跨进程/设备不假设全局有序 |
| `severity_text` / `severity_number` | TRACE/DEBUG/INFO/WARN/ERROR/FATAL 的固定映射 |
| `event` | 稳定机器事件名；本地化文案由界面生成 |
| `host_run_id` / `runner_id` | 框架运行实例与隔离运行器实例 |
| `plugin_id` / `plugin_version` / `module_sha256` | 执行的包身份和模块摘要，由宿主盖章 |
| `host_version` / `host_api` / `sdk_version` | 协议及构建兼容性定位 |
| `scope_kind` / `tool_id` / `account_ref` / `source_ref` | tool/account/source 对应的非秘密引用，尚未发现的身份字段为 null；导出时替换为包内匿名引用，不填假账号 |
| `task_id` / `task_run_id` / `attempt` | 任务定义、一次执行及重试序号 |
| `trace_id` / `span_id` / `parent_span_id` | 一条因果链及其中的操作；具有 span 时必须有 trace |
| `stage` / `endpoint_id` / `batch_id` / `report_revision` | 绑定阶段、逻辑接口、批次和结果修订；按事件选填 |
| `execution_revision` / `notification_policy_revision` / `presentation_revision` | 按实际阶段记录，不能把通知设置变更解释成采集解析变化 |
| `duration_ms` / `outcome` / `error_code` | 单调时钟耗时、结构化结果及可检索错误码 |
| `attributes` | 事件目录定义的有类型、有长度上限的字段集合 |

采用可映射至 OpenTelemetry 的时间、级别与 trace/span 语义；首版不要求部署 Collector，也不默认开启 OTLP 网络导出。字段设计参考 [OpenTelemetry Logs Data Model](https://opentelemetry.io/docs/specs/otel/logs/data-model/)。

宿主记录加载、调度、权限拒绝、宿主服务调用和接收结果的事实。插件通过 SDK 记录其业务阶段，例如“解析到几个配额窗口”；插件不能伪造宿主已经授权、入库或成功投递通知的事件。

系统投递 delivering 后无确定回执时记 delivery_unknown，并关联 notification_key/最近阶段；无平台同 ID 核对能力时不自动重投。HostObservation 的原始正文/secret 与 payload 摘要不输出日志或普通导出；使用可授权/可匿名化的 observation Ref 关联。

## 5. 日志等级与运行模式

| 等级 | 默认策略 |
| --- | --- |
| INFO | 任务生命周期与有变化的摘要；频繁成功操作合并计数，避免每行数据一条日志 |
| WARN/ERROR/FATAL | 记录有界异常与预算拒绝；重复事件可合并，但保留首次、末次、次数和时间范围 |
| DEBUG | 用户或开发者按插件限时开启，补充门控、游标类型、解析计数与校验规则 ID |
| TRACE | 仅诊断会话启用并采样，关注宿主操作和执行顺序，不记录请求内容 |

诊断会话建议默认 15 分钟后自动结束；到期恢复正常等级，重启不自动恢复无限详细日志。该时长为初始产品默认，待 P1 压测调整。

DEBUG/TRACE 不改变授权，不允许记录秘密。首版不接受插件直接将任意字符串、异常 Display 文本、stdout/stderr 或任意对象写入正式诊断日志。

## 6. 插件事件目录和日志 API

插件在清单 `diagnostics.events` 注册稳定事件码、`messageKey` 和允许的字段 schema。SDK 采用结构化调用，概念示例：

```text
ctx.log.info("quota.parsed", { windows: 2 })
```

宿主自动加入插件、任务和 trace 上下文。事件码自动带插件命名空间；字段不符合 schema 时拒绝或丢弃该字段，并累计违规计数。以 enum、bool、有界数值、宿主产生的引用为主；标为“安全字符串”不能免除检查。

核心错误码由宿主管理，例如 `NET.TIMEOUT`。插件错误在 `diagnostics.errors` 注册后使用插件命名空间，例如 `org.example.tokenme.example-ide/quota_response_invalid`；附带恢复建议码和可重试属性，禁止把远端响应正文塞进错误消息。

日志模板在插件语言目录中，JSON 事件码和错误码不随界面语言改变。插件卸载后，框架仍可显示原事件码、保留的最小模板快照及通用说明。

自由文本开发控制台可以作为本机开发工具的独立入口，必须明确标为未审查内容、采用独立有界内存缓冲；默认不持久化、不进入诊断导出，不宣称通用脱敏能够识别任意秘密。

## 7. 完整链路与数据解释

典型链路：

```text
plugin.load → task.gate → task.start → host.call → plugin.parse
→ batch.validate → batch.commit → report.publish → view.bind / notification.deliver
```

每一步都能沿 trace 查询；后台延后发布时，通过批次和报表修订引用连接，不能把不同任务的 trace 随意复用。事件联动产生 child/link 并保留因果来源。

采集摘要至少说明：发现来源数、读取来源数、读取记录数、格式无效数、范围外数、待补全数、产生事实数、接受事实数、去重数、更新/撤销数、拒绝数及主要拒绝原因。各计数的互斥和包含关系在 schema 定义，不将多维计数盲目相加。

配额链路说明是否使用缓存、缓存年龄、何种门阻止了新请求、逻辑 endpoint ID、HTTP 状态或传输错误类别。超时没有响应时不虚构 HTTP 状态码。接口路径属于插件配置，正式日志只保留 endpoint ID，不保留解析后的路径和查询参数。

展示链路说明：当前时间/机器/账号作用域、相关事实是否被纳入、采用哪种估算和价格策略、报表修订与视图绑定状态。日志只记录选择器的安全摘要和计数，不包含会话正文。

运行器崩溃导致 begin 无 end 时，宿主在监督恢复中标记 `RUNNER.INTERRUPTED` 和未完成阶段，不把它误报为供应商接口拒绝；涉及写操作的任务先进入待核对状态。

## 8. 错误分类与排查建议

| 错误或状态 | 定位信息 | 默认处理 |
| --- | --- | --- |
| `PLUGIN.API_INCOMPATIBLE` | 宿主 API、插件要求、实际模块接口 | 阻止加载，提示兼容版本 |
| `SOURCE.NOT_FOUND` | 来源规则 ID、探测结果数 | 正常缺失状态；提示配置路径或安装工具 |
| `AUTH.LOGIN_REQUIRED` / `AUTH.EXPIRED` | 凭据引用是否存在、认证阶段 | 引导插件已注册的登录操作；不在只读诊断中自动刷新 |
| `PERMISSION.DENIED` | 服务、授权规则 ID、任务 | 解释缺少的授权，保留最后数据 |
| `TASK.GATED` | 有效门和来源设置键 | 显示暂停原因，不计为供应商失败 |
| `NET.TIMEOUT` / `NET.TLS` / `NET.RATE_LIMITED` | endpoint ID、阶段、耗时、重试安排 | 按任务的读写属性和重试规则处理 |
| `PARSE.SCHEMA_MISMATCH` | 解析器版本、规则 ID、记录位置 | 提示数据格式变化；无原始值或响应片段 |
| `DATA.VALIDATION_FAILED` | 指标/规则 ID、拒绝计数 | 拒绝批次或有定义的部分记录；不推进未提交游标 |
| `INGEST.NO_NEW_FACTS` | 去重/范围外/无变化的分项计数 | 解释无新增数据，不能统一标“采集失败” |
| `RUNTIME.BUDGET_EXCEEDED` | CPU/内存/输出等预算类别 | 取消相关实例，宿主与其他插件继续运行 |
| `ACTION.OUTCOME_UNKNOWN` | 账号匿名引用、周期、最后阶段 | 先核对服务端状态，不盲目重试写操作 |
| `VIEW.BINDING_FAILED` | 组件、binding、数据 schema 和报表修订 | 定位显示合同，不误归因给采集 |
| `NOTIFICATION.GATED` / `NOTIFICATION.DELIVERY_FAILED` | 策略原因或系统投递码 | 分别处理静音与真实投递失败，不补发被静音的旧阈值 |

## 9. 磁盘、速率与脱敏边界

在现有宿主日志根增加专属目录，目录名使用宿主规范化的安全标识，不能将插件 ID 原样当作路径：

```text
logs/plugins/
  runtime.jsonl
  <safe-plugin-key>/events.jsonl
  <safe-plugin-key>/events.jsonl.1
```

生产 v1 初始硬预算：单插件活动文件 2 MiB、一个轮换副本、最多保留 7 天、所有插件日志合计 64 MiB；单条事件不超过 8 KiB。每插件队列最多 1000 条、全局 4096 条，稳态每秒 20 条、突发 100 条；超限记录 dropped 计数、每分钟至多汇总一次。宿主控制，插件只能请求更少；P1 可通过有数据的 ADR 修订，不由作者无限扩大。

日志异步有界写入；磁盘满、无权限、队列满或插件刷屏不能阻塞业务线程。关键生命周期记录优先保留但仍受总预算约束；发生丢弃时由宿主累计并在健康页显示，不承诺资源耗尽时仍能完整留存所有日志。

正式日志与诊断包默认不含：凭据、认证头、请求或响应正文、完整 URL/查询、绝对文件路径、用户消息、环境变量值、原始 SQL 参数、内存 dump 和未审查自由文本。采用字段白名单、句柄匿名化和宿主边界校验；通用字符串替换只作为补充。

支持日志排查不改变现有只记配额请求元数据的原则。原生崩溃 dump 可能含秘密，默认不进入导出包；符号化只使用与模块摘要匹配的构建符号，包内路径应为插件相对路径。

## 10. 健康界面、CLI 与授权诊断

插件详情增加“状态与诊断”：运行版本、最后成功和下次执行、最新数据年龄、当前任务、有效门、最近错误、近段错误/耗时摘要、日志筛选、诊断 ID、限时详细日志和本地导出入口。

下列命令名仅为拟议接口，不表示目前可执行：

```text
tokenme plugin status <id>
tokenme plugin logs <id> --task <task-id> --trace <trace-id>
tokenme plugin doctor <id>
tokenme plugin diagnose <id> --since 30m --output <local-path>
```

`doctor` 默认只读本地已知状态与静态校验，不执行供应商请求、刷新凭据、签到或主动模型测速。手动运行任务使用单独的 action 命令与 UI，明确副作用并受正常授权和任务门控制。

拟议授权诊断接口：`plugin.get_status`、`plugin.get_task_runs`、`plugin.get_logs`、`plugin.explain_data`，采用窄的只读 scope 和有界查询；授权仅覆盖指定插件和时间范围。控制类操作需要独立 scope，不因已有设备同步控制权限就自动获得插件执行权。

## 11. 诊断包与复现

诊断包按用户选择的插件和时间范围生成到本地，包含宿主/插件/API/SDK 版本、模块摘要、兼容性结果、有效非秘密配置、任务摘要、脱敏事件和丢失记录说明。复制账号/来源/设备身份时使用包内一致的临时引用，不直接输出可跨包追踪的永久哈希。

导出前展示包含的类别和大小，采用同一脱敏规则，不自动上传、不自动发给第三方。宿主不附带完整插件私有目录、凭据文件、源会话或整份用户设置。

元数据日志能定位阶段，但不能重放从未保存的原始数据。开发复现依靠 SDK TestKit 的匿名 fixtures、模拟 HTTP/时钟/存储，或用户单独提供且允许使用的脱敏样本；不得把“导出诊断包”宣传为可以自动重放所有真实接口。

## 12. 交付阶段与完成条件

| 阶段 | 必交付内容 |
| --- | --- |
| P1 | 日志/错误类型、任务上下文自动传播、宿主调用 span、有界写入、超时与拒绝日志、SDK 的结构化日志 API |
| P2 | 完整摄取到报表关联、健康界面、只读 doctor/logs、限时详细日志、脱敏本地导出 |
| P3 | 迁移现有 quota/scan/checkin 日志语义；真实插件对账和故障注入 |
| P5 | 发布模块符号与源码映射的关联策略、兼容性报告、官方开发文档 |

验收至少覆盖：给定错误定位到具体插件/版本/阶段；日志量达到预算仍不阻塞；秘密标记不会进入正式日志及导出；禁用采集的原因可解释；解析成功但未入汇总可解释；运行器中断可恢复且写动作待核对；只读 doctor 无副作用；不同插件不能伪造彼此的日志上下文。

## 13. 参考

- [OpenTelemetry Logs Data Model](https://opentelemetry.io/docs/specs/otel/logs/data-model/)：时间、级别及 trace/span 的字段语义。本方案只采用可映射的数据设计，尚无 OTLP 导出或 OTel 一致性认证。
- 当前仓库代码为第 2 节所列路径；磁盘和采样预算是本方案建议，不冒充现有配置。

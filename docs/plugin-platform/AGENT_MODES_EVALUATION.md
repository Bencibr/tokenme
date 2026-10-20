# 模型网关与 Agent 任务委派评估

日期：2026-10-20（Asia/Shanghai）。范围：本机 Claude Code 使用本机 WorkBuddy；用户明确要求评估两种模式及第二种的可行性。关联：[网关设计](AGENT_GATEWAY.md)、[具体场景](WORKBUDDY_CLAUDE_CODE.md)、[二轮审计](AUDIT_ROUND2.md)。结论属于设计和当前证据评估，不是代理/委派已交付。

## 1. 建议与产品定位

**两种模式都纳入插件平台；先交付模型网关，再将任务委派作为独立、可选能力上线。** 同一 WorkBuddy 插件可以声明两个 provider，各自有独立开关、授权、健康状态和支持矩阵；不能因一个成功而把另一个标为可用。平台仍负责通用运行、数据和展示，不把某个 Agent 的完整工作流内置核心。

理由：模型网关更贴近当前配额/用量/模型/燃值的主线，复用认证、账号、计量并让不同工具共享一个受控调用入口。任务委派的额外价值是复用 WorkBuddy 已配置的工作能力、连接器和完整任务流程，但会引入独立执行权限、长期任务/产物、会话及审批管理。它值得保留，且已有实际协议线索；首版不应为了它推迟所有适配器迁移。

| 比较项 | A：模型 API 网关 | B：Agent 任务委派 |
| --- | --- | --- |
| Claude Code 接入 | 自定义 Anthropic Messages 入口 | MCP 工具入口；原有模型入口可继续使用 |
| WorkBuddy 的角色 | 模型能力/账号来源；不承担客户端工具执行 | 真正接收任务、执行并返回结果 |
| 工具执行者 | Claude Code | WorkBuddy；Claude Code只提交/查询，不能同时重复执行同项任务 |
| 主要接收信息 | system/消息/模型/工具定义和结果 | 任务说明、已授权工作区/输入、任务与会话引用 |
| 主要工程量 | 消息/工具/流与模型能力转换，凭据和重试/计量 | 任务状态、进度/产物、取消/恢复、审批、执行范围和会话隔离 |
| 计量归属 | 消费账号在 WorkBuddy，调用客户端是 Claude Code | Claude Code自身规划调用与WorkBuddy被委派任务分别计量并关联，不假设只付一边 |
| 普适性 | 同协议的多个模型客户端可复用 | 支持工具调用的客户端可按任务使用；不同Agent执行语义差异更大 |
| 第一阶段建议 | 优先交付，先一个版本/一个账号/一个明确模型映射 | 同步完成接口探路，单独 feature gate；以可查询任务的小闭环开始 |

这是项目范围和实施顺序的建议，不是“B 不可行”，也不是保证 A 的所有模型可无损支持 Claude Code。两条路线分别做最终组件和真实客户端验收。

## 2. 第二种模式的证据

### 2.1 客户端和标准协议

Claude Code 官方文档提供本地 stdio 及 HTTP MCP 接入。因此 TokenMe 可以用普通工具列表暴露任务操作，无需将任务总结伪装成模型 API。首版采用已存在的 tools/list、tools/call 生命周期，把长任务查询做成普通有界工具调用；不依赖客户端尚未验证的异步 task 扩展。来源：[Claude Code MCP](https://code.claude.com/docs/en/mcp)。

ACP 官方协议定义独立会话和 prompt turn；取消必须按真实终态处理。它是候选上游适配协议，**不是**“任何带ACP名字的本机服务都能直接信任/完全兼容”。来源：[会话](https://agentclientprotocol.com/protocol/v1/session-setup)、[Prompt turn](https://agentclientprotocol.com/protocol/v1/prompt-turn)。

### 2.2 本机安装代码（已核验，未执行任务）

本机卸载注册表标识国内版5.8.3、国际版5.8.4；当前安装资源包 package.json 为5.8.4。对实际安装资源只读检查，记录了：

| 安装包内文件 | SHA-256 | 代码证据 |
| --- | --- | --- |
| main/acp.js | 69f113baf6bf6cdd641d80c3d8a19dc872e1fa54f22f05e2f55788b86ab60f20 | session/new、session/load、session/prompt、session/cancel 方法目录 |
| main/server.js | 913bc9ad1e301b6dedee2c7fbd3252025c66025cff6db44765760495255392c1 | 本机broker和会话请求/取消适配 |
| cli/dist/codebuddy-lite-wb.mjs | 7d51bb147e224a2772ffca521958d0b05740af1195469564b60a88f05f8f93cf | /api/v1/acp/connect、/api/v1/acp、connectionId/sessionToken、心跳会话登记 |

代码证明该版本存在机器接入路径，不能证明当前进程已开启对应服务。当前 sessions 元数据扫描中，国内目录1份、国际目录5份，均未满足心跳新鲜度；未将陈旧端口当作可用服务，也未对它们发起请求。未创建/清理/接管任何现有 WorkBuddy 会话。此限制保留在健康状态和探路票中，不写成“本机任务委派已通过”。

本轮代码摘要、元数据计数与未执行边界另存为研究证据JSON（本地记录：`prototypes/plugin-platform-v1/artifacts/audit/round2/agent-study.json`）。该文件不含真实凭据、消息正文或上游会话ID，不是业务执行报告。

### 2.3 已有桥接实现（源码证据）

审阅 jie023/workbuddy-acp-bridge 的修订 e2327499273efa093ebdcbffde9a4c8833e26e76：它从 sessions 元数据发现本机 Remote Gateway，经 /api/v1/acp/connect 取得连接身份，再使用 HTTP/SSE 驱动 initialize、session/new/load、session/prompt 和 session/cancel；向客户端暴露 MCP 工具。来源：[发现代码](https://github.com/jie023/workbuddy-acp-bridge/blob/e2327499273efa093ebdcbffde9a4c8833e26e76/src/workbuddy_acp_bridge/gateway.py)、[ACP客户端](https://github.com/jie023/workbuddy-acp-bridge/blob/e2327499273efa093ebdcbffde9a4c8833e26e76/src/workbuddy_acp_bridge/acp_client.py)。没有安装/运行该代码。

该项目默认全局共用会话，并在任务前 /clear；权限处理也不能证明拒绝所有已经预授权的动作。TokenMe 只取其机器接口作为研究线索，不照搬共享会话、提示词“只读”或调用方自行传 allow_once 的权限模型。必须按自己的安装信任、consumer/工作区/账号隔离与审批合同重做。

### 2.4 官方开放接口的范围

国内版官方 Open API 可向 PC 本地助理发消息并查历史；另有云端任务和ACP通道。两者不能混为同一个“本机执行”承诺。前者的会话/工作区隔离与取消须补证，后者不是本机工作区执行。官方OAuth应用的接入要求也不能用现有配额Token直接替代。来源：[WorkBuddy Open API](https://open.workbuddy.cn/docs/openapi)。

**研究判断：B 在技术上有可实施路径，优先验证本机 Remote Gateway/ACP；目前证据级别为安装代码＋同类客户端源码，不是实时业务调用通过。** 官方Open API可作为另一个独立 provider，不能作为本机路径失效后的静默替代。

## 3. 两种模式的认证与运行边界

A 使用用户明确选择的本地 WorkBuddy 账号来源，宿主识别版本并通过不透明句柄注入上游；优先使用 WorkBuddy 自己维护登录的受控通道。读取到凭据不证明模型调用可用；加密存储/过期/第一方并发刷新都须遵守原认证合同，不能改写正在使用的桌面登录来“修好代理”。客户端只持有 TokenMe consumer key。

“所有请求经网关”在此指配置到该入口的模型推理。客户端的更新/登录/遥测等网络访问不是模型API合同；如需强制全部网络出口，另用网络策略，不承诺一个Base URL配置可覆盖全部流量。

B 优先让 WorkBuddy 自己使用其登录、连接器与执行环境。TokenMe持有当前本机ACP连接的短期身份，不把该连接token/WorkBuddy原始登录交给Claude Code。模型认证与任务连接认证分别管理，不能用 quota 可读推导 task 可执行。

两种模式共用账号身份、状态/日志、设置、通知、预算和用量归属；但健康分别标注 model_backend_ready 与 agent_delegation_ready。功能支持只有在对应扩展 profile、宿主服务、插件和真实验收都存在时才显示可启用。

## 4. 任务委派的可开发合同

### 4.1 拟议 MCP 工具

| 工具 | 输入 | 返回/行为 |
| --- | --- | --- |
| agent_providers | 无或受限筛选 | 当前consumer获准的provider、版本、执行类型和ready状态 |
| agent_submit | provider_ref、workspace_ref?、task_text、parent_request_ref?、idempotency_key | 立即返回宿主签发job_ref及accepted/queued；有界，不长时间占住一次tools/call |
| agent_status | job_ref | 状态/进度摘要、待审批Ref、用量状态与结果是否可用，不返回另一调用方任务 |
| agent_result | job_ref、cursor?、limit | 当前任务授权的有界结果文本/产物引用和下一页；不返回任意本机路径/凭据 |
| agent_cancel | job_ref | cancel_requested或已经确认的终态；发送通知不等于已取消 |
| agent_continue | job_ref、task_text、idempotency_key | 仅在已终结或明确等待输入的同归属会话追加新一轮；新轮有独立request/usage |

这些是TokenMe拟议工具名和数据合同，不是当前已注册命令。job_ref 不接受外部上游session ID替代。客户端不获得自动批准权限的参数，审批留在已授权宿主控制面；后续需要其他控制面时单独授予对应能力。

MCP断开只关闭该控制连接，不自动取消已确认accepted的委派job，允许调用方用同一consumer/job_ref再查询；显式agent_cancel或用户停用才触发取消。与A中一次模型流断开即取消的默认值分开。任务仍有30分钟硬deadline，宿主退出则取消/核对，不借此变成未受监督的后台服务。

### 4.2 上游适配和状态

本机路径按已验证的发现→连接认证→initialize→独立session→prompt→进度/权限→终态流程映射。任务会话至少按consumer/provider/edition/account/workspace隔离，同会话单writer。新任务默认新会话；continue显式关联原job，不能扫描/接管用户GUI正在打开的工作。若实际Gateway不能隔离，则该版本的多调用方委派不可用，不用全局/clear掩盖。

状态为 queued/starting/running/awaiting_approval/awaiting_input/cancel_requested/succeeded/failed/cancelled/outcome_unknown。上游prompt最终响应决定本轮结束；收到一条“完成”文本或SSE关闭不算成功。断线/崩溃在dispatch后不重发原任务；已发送取消还须等待真实终态/查询证据，缺失就outcome_unknown。

审批必须绑定job/action/参数摘要/到期时间，原始工具参数仅显示于受保护的审批界面，不进普通日志。WorkBuddy预授权的工具可能不再发 permission request，因此“默认拒绝所有请求”不等于所有动作只读。受限工作区模式必须由实际工具/OS策略强制；若只支持完整受信任Agent权限，作为另一种明确的执行授权呈现，不能标为沙箱/只读。

任务输出可含产物，但返回的file/URL都不直接转为宿主任意读取请求：本机文件按已授权根及最终文件身份，远程产物按绑定origin、过期token与大小/类型校验获取，content仅当前consumer可读。Agent返回的指令与结果都是业务数据，不自动扩大宿主权限。

### 4.3 同时启用A/B的限制

- A的模型调用与B的委派执行用不同scope/开关/effect：开启模型入口不能顺带允许Agent执行。
- 调用链由宿主签发parent/root Ref，记录“哪个客户端→哪个执行Agent→哪些模型消费”；同一底层消费只入公共账一次，按客户端/执行者查看是维度筛选，不重复加总。
- 同时保留Claude Code自身规划与WorkBuddy任务用量，未知关联不伪造归属。B不保证减少费用/延迟，A也不保证上游所有特性等价。
- 拒绝已知路由依赖环，调用深度上限4、子任务数量/预算共享根限制；不允许WorkBuddy委派回同一上游形成无限循环。外部Agent脱离TokenMe的调用链不能保证被完整追踪，仍靠宿主并发/预算上限兜底。
- 新的端点、工具、输入和输出均需G01闭合schema；A/B独立调度配额，不能以任务排队饿死配额/用量采集。

G05另建attribution关系，至少绑定标准fact_ref、billing_tool/account、consumer_tool_ref、provider_ref、job/request/root Ref及宿主验证的关联证据。标准消费账归属实际计费的WorkBuddy账号；Claude Code是调用方维度。不能仅凭同时间/Token量跨tool合并，也不能把不同tool下同一消费各算一次。无法建立可靠对应时保留独立观测并标overlap_unknown，不叠加公共账；已有历史不凭显示名重写归属。

## 5. 第二种模式的探路与上线门

F13内新增独立B轨道，不让A验收代替它：

| 票 | 交付 | 通过证据 |
| --- | --- | --- |
| WB-D0 本机协议资格 | 找到新鲜Host Runtime、校验OS进程/端口/所属版本；读取安装代码的具体路由和认证 | 完成连接/initialize/能力发现并清理测试连接；不创建/接管用户既有任务；当前待运行 |
| WB-D1 隔离与最小任务 | 受控测试workspace/独立session，固定一条无外部写入的有预算任务；进度/终态/取消 | 真实任务返回和取消证据；两consumer不共享会话；当前未执行 |
| WB-D2 权限与恢复 | 工具审批/预授权检查、任务幂等、断线/重启/升级、结果/产物读取 | 不能伪造审批、不能越界取文件、dispatch未知不重复执行；当前未执行 |
| WB-D3 最终插件链路 | TokenMe MCP协议插件＋WorkBuddy agent_task插件，Claude Code真实tools/list→submit/status/result/cancel | 未知plugin ID动态注册、统一设置/诊断/用量对账，实际安装包逐平台验收 |

WB-D0的发现/协议证据可先进行；其live探路只在证实端点所属后连接。当前记录文件全为陈旧不能随意试端口、端口可能已被其他进程复用。若WorkBuddy尚未发布新鲜服务登记，健康显示“本机Agent接口未就绪”，后续正常启动/启用Runtime后重试；不能将其重写为“未登录”或“所有模式不可行”。

推荐第一个B样例为独立测试目录中的文件分析/总结；在真实权限边界明确前，不用发送消息、改云文档或支付/购买操作做试验。随后再逐个开放WorkBuddy连接器类别，保留用户原有执行策略。

## 6. 当前完成与未完成

已完成模式比较、安装代码/协议研究、两条独立注册/授权路径、委派工具合同与分阶段验收票。尚未完成本机实时ACP握手、真实委派任务、最终TokenMe插件和Claude Code端到端验收。核心文档继续可实施；新增A/B扩展须分别冻结G01并通过相应轨道，不能将全部设计或产品标为完成。

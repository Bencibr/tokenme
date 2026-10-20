# 插件开发语言、SDK 与开发工具链

版本：0.5（实施路线与生产 SDK 设计输入）
日期：2026-10-20（Asia/Shanghai）
实施入口：[实施规格 0.5](IMPLEMENTATION_SPEC.md)、[可运行参考工程](../../prototypes/plugin-platform-v1/README.md)。关联：[主合同](PRODUCTION_CONTRACTS.md)、[补充合同](DESIGN_HARDENING.md)、[总体方案](PLAN.md)、[日志与诊断](LOGGING_DIAGNOSTICS.md)、[第二轮审计](AUDIT_ROUND2.md)。参考 SDK/工具已有源代码；生产 v1 全部生成合同与 SDK 仍在 F01 交付，不称已发布/冻结。

[Agent 网关扩展](AGENT_GATEWAY.md)继续使用同一 Rust/Wasm 和包工具路线，但需要 G01 新的 GatewayContext/事件流/服务绑定与 TestKit；当前 SDK 的 execute/http.request/process.invoke 不能用于承诺长时代理。外部原生 Agent CLI 是显式受控依赖，不因插件以 Wasm 运行而获得隔离证明。

## 1. 已选定的语言路线

首条路线固定使用 **Rust**，已有独立参考 SDK/组件/宿主可用于开发；**TypeScript** 是下一阶段官方 SDK 目标。先将已验证合同接入生产平台，再扩展语言工具链。

| 编写方式 | 规划支持级别 | 进入正式支持的条件 |
| --- | --- | --- |
| Rust | 参考链路已提供，生产 SDK 首发路线 | 复杂宿主调用、实际 TokenMe 接入、全局取消/资源和目标平台验收 |
| TypeScript | P1 做最小可行性记录；P2 后推进独立 SDK，初期实验状态 | 组件化构建、打包体积、启动/内存、Promise/取消和调试能力达到同一合同，才进入正式支持 |
| JavaScript | 若复用 TS 工具链可作为无类型源入口；不额外维护另一套运行协议 | 使用同一受限 API 和测试矩阵，不承诺 Node.js 兼容 |
| 纯声明式配置 | 首版提供标准模板覆盖简单 HTTP/字段映射任务 | 由官方通用处理器实现，权限、错误和结果仍遵守标准合同；复杂逻辑仍需代码 |
| Python/Go/C++ 等 | 首版不发布正式 SDK；保留语言无关协议 | 存在编译工具不等于 TokenMe 已支持，需独立绑定、打包、调试和兼容性验证 |

Rust 首发的理由是当前代码复用和迁移成本可验证；这不是要求所有未来插件作者只能使用 Rust。TypeScript 的易用性目标也不能靠放开任意 Node API 来实现。

Rust/Wasm 是当前实施选择，不要求实施者重新选型。它不表示全部生产功能已上线；若上线验证推翻路线，写 ADR 并同步修订 SDK 和迁移计划。参考锁定版本、项目结构和逐步命令以实施规格为准。

## 2. 开发语言、构建格式和运行引擎分别管理

Wasm 是构建/执行格式，WIT 是接口合同，Rust/TypeScript 是开发语言，Wasmtime 是已选定的宿主执行引擎。这四者分别管理版本与验收。

当前路线与下一阶段路线：

```text
Rust 源码 + Rust SDK + WIT 绑定
    → wasm32-wasip2 组件 → .tokenme-plugin → TokenMe 的一个 Wasm 执行引擎

TypeScript 源码 + TS SDK + 生成类型
    → JavaScript ESM → componentize-js/jco 等组件化工具
    → 同一插件合同的 Wasm 组件 → 同一个宿主执行引擎
```

第二条为待本仓库验证的可行路线，并非当前已经支持。JS 组件通常包含语言引擎支持，不能默认与 Rust 组件具有相同的体积和启动成本；必须实测。

Bytecode Alliance 的官方资料描述了 Rust 组件构建，以及 JavaScript/先转 JS 的 TypeScript 经组件工具构建的路径。官方 Rust 指引目前使用原生 `wasm32-wasip2` 目标，并提示 `cargo-component` 正在退出推荐路径；SDK 应锁定经验证的工具链，不照搬旧命令。参考见第 12 节。

不并行维护嵌入式 JS 引擎备选，也不把 `jco transpile` 用于 Node/浏览器的输出当作当前部署格式。路线变化通过 ADR 修订，不放任开发者自行分叉宿主协议。

## 3. 开发者与用户环境

插件作者需要语言编译工具链和 TokenMe 开发工具；普通用户只安装插件包，运行所需能力由 TokenMe 提供，不自行安装 Rust、Node.js、Python 或 Wasmtime CLI。

官方模板固定 Rust/目标/绑定生成器或 Node/打包器/组件工具的版本与依赖锁。SDK 构建输出记录来源修订、SDK 版本、工具链版本、宿主 API 和模块摘要；发行时不暴露构建机用户名和绝对路径。

插件源码可复用纯解析、模型映射和算法库。依赖原生数据库、文件系统、网络、进程、系统钥匙串的旧代码不能直接假设在 Wasm 中可用：这些操作改走 SDK 宿主服务，插件保留厂商协议、SQL/字段映射和处理策略。

不继承 Node 的 `fs`/`net`/`child_process` 或 Rust 标准库的任意环境访问。实际导入哪些 WASI/宿主接口，由协议和授权决定；关闭一个底层接口不能再用同等权限的旁路补回。

## 4. SDK 不只是类型定义

SDK 交付包含六部分：

1. **合同与绑定**：身份、任务、来源、事实、游标、配额、估算、设置、事件和错误类型，以及生成绑定。
2. **安全宿主客户端**：HTTP、文件、SQLite、系统凭据、受控程序、状态存储和限额/取消。
3. **注册与资源描述**：能力/处理器、品牌、视图、设置、语言、通知和诊断事件目录。
4. **测试工具包**：模拟服务、固定时钟、匿名 fixtures、错误注入、对账与日志断言。
5. **开发/打包工具**：脚手架、绑定生成、构建、验证、本地开发安装、调试、打包和兼容检查。
6. **文档与参考插件**：最小插件、完整配额/用量插件、带状态核对的签到样例和迁移教程。

本地实施 crate 固定名 `usage-plugin-sdk`、`usage-plugin-testkit`。未来 TS 拟用 `@tokenme/plugin-sdk` 和 `@tokenme/plugin-testkit`，发布前检查包名归属；不表示注册中心已有这些包。

## 5. 核心接口与生命周期

插件通过能力实现而不是继承一个必须实现所有方法的巨型类。SDK 提供以下接口族，处理器与清单声明需逐项匹配：

| 接口族 | 作者实现或使用 |
| --- | --- |
| 注册/生命周期 | 清单静态登记；无任意 guest init/unload hook。宿主登记监听/连接/租约并释放；私有状态首次 get/cas、升级仅 host:migrate staged snapshot |
| SourceDetector / UsageCollector | 发现数据源、有界增量读取，返回事实和版本化下一游标 |
| AuthDiscovery / CredentialRefresh | tool scope 下发现候选，宿主确认账号；独立授权的 auth.refresh 执行 credential_write，不混入只读配额 |
| QuotaReader | 绑定账号读取配额，返回状态、窗口、单位和采样时间 |
| CheckinAction | 查询领取状态，执行合法动作，处理结果未知和重复调用 |
| TokenEstimator / ValueEstimator / PricingResolver | 消费获准输入，返回可追溯派生结果或价格合同 |
| TaskContext | 获取本次身份、设置快照、授权服务、截止期、取消信号、预算和 trace |
| StructuredLogger / PluginError | 使用已注册事件码、类型化字段和错误分类；宿主自动关联上下文 |
| Resource/Settings/View descriptors | 生成或验证品牌、语言、组件和配置的声明式描述 |

`TaskContext` 不提供全局宿主对象。每次服务调用继承插件/账号/任务授权；插件不能提供另一个插件 ID 来访问其私有数据。

Scope 按 tool/account/source 区分；Ref 与 resource handle 分开，session/series/derived Ref 由 identity.resolve 取得，output batch/operation 由宿主签发。读取响应附 HostObservation，SDK 不编造数据来源。task/handler 的分派来自已验证清单，源码函数不是任意可调用入口。完整字段见补充合同 §1。

结果交付与原子提交属于宿主：插件返回批次和游标，宿主校验并提交，成功后才确认下一游标。SDK 不允许插件自己把游标写入核心索引，也不把日志成功当作事实已经入库。

同批次 ID 不同内容拒绝、同 revision 不同 contribution 冲突；counts/时间缺失有明确 nullable/pending 语义，不能补假 0。执行配置与通知/展示版本独立，大数和价目使用规定的精确计算/舍入规则，见补充合同 §2/4/6。

完整生产输入输出、服务请求、上下文、错误类型、单位与大小限制见生产合同第 3—5 节。SDK 不能自行扩展未注册服务或把未来接口混入当前 alpha。F01 提交生成类型/schema/WIT，F05/F10 提交真实服务及能力，逐项进入支持清单。

长任务使用有界分页/批次。取消、超时、服务失败和插件 panic/trap 映射成标准结果；对远端写操作保留 outcome_unknown，不以语言异常自动重试。

## 6. 作者侧代码形态

完整可编译实现见 [SDK](../../prototypes/plugin-platform-v1/sdk/src/lib.rs)、[参考插件](../../prototypes/plugin-platform-v1/reference-plugin/src/lib.rs)和 [WIT](../../prototypes/plugin-platform-v1/wit/plugin.wit)。实际入口签名为：

```rust
pub fn execute(task_id: &str, input: &str, host: &mut dyn HostServices) -> Result<String, String>
```

参考通过 `host.http("quota")` 调用受限的逻辑接口，通过 SDK `TaskResult` 返回已校验结果；当前只由宿主提供 fixture，不接真实账号。生产认证句柄按实施票增加。`quota.parsed`、语言模板和允许字段在清单内注册。

TS SDK 未来提供相同语义的类型和上下文，具体同步/Promise 映射以绑定原型为准。不能先给出任意异步 API，再假设已有组件工具自动实现全部取消、资源释放和错误传播。

## 7. 开发工具与工作流

当前可运行工具位于参考工程 `tools/plugin.py`；以下命令真实存在，在参考工程目录执行：

```text
python tools/plugin.py init sandbox/new-ide --id org.example.community.new-ide
python tools/plugin.py check
python tools/plugin.py test
python tools/plugin.py build
python tools/plugin.py pack
python tools/plugin.py inspect dist/org.example.tokenme.reference_0.1.0.tokenme-plugin
python tools/plugin.py install dist/org.example.tokenme.reference_0.1.0.tokenme-plugin --allow-unsigned-local
python tools/plugin.py run org.example.tokenme.reference --task collect-usage --repeat 2
python tools/plugin.py verify
```

- `init`：创建身份/能力模板、语言资源、标准日志和匿名测试骨架。
- 绑定由 Rust 构建中的宏从锁定 WIT 生成，无需一个尚不存在的 bindings 命令。
- `check`：检查清单、实现导出、权限、任务/设置引用、语言键/占位符、资源和包内路径。
- `test`：默认运行离线匿名样本，输出标准事实和合同报告，不读取开发者真实账号。
- `run`：运行开发沙箱中已安装的最终 Wasm 组件，使用 fixture 服务；当前修改后重跑 build/verify，不冒充已实现热重载。
- `inspect`：查看产物的接口导入/导出、SDK/工具链/模块摘要和权限需求，不运行插件代码。
- `pack`：生成受限包结构、完整性文件和构建元数据；签名通过单独的发布者身份流程完成。
- `verify`：native/最终组件/包/安装/错误/门控/预算闭环，输出独立场景报告和 verification.json。

生产独立工具名 `tokenme-plugin`、自动重建/热重载和多版本兼容扫描由实施票 F09 交付；本轮不将这些后续命令写成已经可用。

开发 profile 与用户数据目录物理隔离；实时网络和真实账号测试必须显式选择 live profile，并经过运行时授权。开发模式不绕过权限，热更新也不隐式重放副作用任务。

## 8. TestKit、复现与问题定位

TestKit 提供：HTTP 请求和失败序列模拟、只读 JSONL/SQLite/WAL 样本、系统凭据服务 mock、固定时钟与厂商周期、锁/存储 mock、游标和事实断言、模拟通知 sink、有界日志收集和错误码断言。

重点合同：重放不重复、截断能修复、累计值转增量正确、真实/估算和积分分离、令牌轮换比较后写回、签到结果未知先核对、超时取消释放资源、开关不清缓存、语言回退可见。

按 trace ID 导出结构化执行路径和任务输入的非秘密摘要。诊断日志不能代替原始 fixture；TestKit replay 只重放明确的匿名输入和记录好的模拟结果，不从生产日志猜测原始请求或读取真实凭据。

编译后的 Wasm 在本机独立运行环境中执行同一合同测试，而不只运行插件源码的 native 单元测试。TS 若进入支持范围，也必须执行最终组件测试，不能仅以 Node 测试通过替代宿主兼容。

## 9. 调试和发布产物

提供开发构建的插件相对源码位置、Wasm backtrace 或 JS source map 与模块摘要的匹配机制；精确能力取决于 P1 工具链验证，首版不承诺全语言 IDE 断点调试。

生产包保留最小可诊断的函数/处理器标识、模块摘要和构建记录。完整符号/源码映射可以作为单独发布者调试附件，不能把用户内存、凭据或构建机绝对路径带入产物。

调试器和分析器不自动打开网络端口。插件作者可在开发 profile 启用本地调试入口，生命周期和授权由开发工具管理；宿主正式运行仅使用标准诊断接口。

## 10. 版本、依赖和兼容性

宿主 API 是合同版本；SDK 是语言封装版本；插件是业务实现版本；工具链是构建版本。四者均记录，不能以插件作者装了最新版 SDK 就假设所有宿主都支持它。

SDK 的必要导入应与清单 `hostApi`/能力要求一致；打包时拒绝未声明或宿主不支持的系统导入。可选能力有明确探测和降级；迁移不依靠主机动态下载任意源代码。

依赖在构建时锁定并打包，不在用户运行时执行 npm install/pip install/cargo build。是否允许组件组合由协议版本定义，首版不开放任意依赖插件的代码互调。

Rust/TS 绑定从同一个类型合同生成或校验，分别测试数值范围、时间戳、空/缺失、错误码、游标字节和资源释放。JS 的安全整数和 Wasm 64 位值使用经验证的线格式，不能静默转换导致用量失真。

## 11. 分阶段交付和语言支持门

| 阶段 | SDK/工具链必交付 |
| --- | --- |
| P1 | 锁定的最小合同、Rust 绑定和 TaskContext、日志/错误 API、fixture runner；记录 TS 组件工具的最小可行性与成本 |
| P2 | Rust SDK 第一条完整样板链路、init/check/build/test/pack 的可用实现、开发 profile、安装和诊断；此时才能宣称 Rust 正式可开发 |
| P3 | 复杂真实插件迁移模板、TestKit 故障矩阵；TS SDK 按独立实验里程碑接入，不阻塞已验证的 Rust 迁移 |
| P5 | Rust SDK 版本化文档、发布包和兼容报告；TS 只有达到同等验收时才列为正式支持，否则保留实验标签 |

一种语言进入支持清单，至少证明：新项目脚手架能构建并安装；主程序无需重新编译；配置/任务/通知/国际化完整；日志可定位；取消/权限/数值边界正确；最终发行组件通过 fixtures 和真实支持环境验证。

## 12. 官方参考与证据范围

以下查阅于 2026-10-19，只证明上游存在对应路径，不证明 TokenMe 已集成：

- [Bytecode Alliance：Rust component](https://component-model.bytecodealliance.org/language-support/building-a-simple-component/rust.html)：原生 Rust 目标及绑定生成路径。
- [Bytecode Alliance：JavaScript/TypeScript component](https://component-model.bytecodealliance.org/language-support/building-a-simple-component/javascript.html)：TS 先转 JS，组件化及生成类型的工具路径。
- [WIT Reference](https://component-model.bytecodealliance.org/design/wit.html)：接口合同语言，与业务实现语言区分。
- [jco 官方仓库](https://github.com/bytecodealliance/jco)：JS 组件相关工具；不意味着任意 Node 模块可直接作为 TokenMe 插件执行。

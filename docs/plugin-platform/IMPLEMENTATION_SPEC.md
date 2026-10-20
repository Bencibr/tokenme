# TokenMe 插件平台实施规格 0.5

日期：2026-10-20（Asia/Shanghai）
开发分支：`codex/plugin-platform`。本文是当前实施入口，覆盖平台开发和插件作者两条工作流。

本轮实测与产物摘要见 [参考闭环验证记录](REFERENCE_EVIDENCE.md)，可直接对照报告复核，不以设计文字替代运行证据。

## 1. 规范优先级与实施边界

0.1/0.2 描述产品架构；本规格将第一条开发路线固定，并提供实际源代码和工具。冲突时按以下顺序处理：

1. `prototypes/plugin-platform-v1/wit/plugin.wit`、`schema/` 的 JSON Schema、SDK 类型和经过验证的参考工具是 **alpha 0.1 可执行合同**。
2. 本文定义参考工作流；[生产协议与状态合同](PRODUCTION_CONTRACTS.md)固定 F01—F12 的生产语义，尚未实现的生产 v1 与 alpha 分开版本管理。
3. [总体方案](PLAN.md)、[日志规范](LOGGING_DIAGNOSTICS.md)、[SDK 设计](LANGUAGE_AND_SDK.md)补充产品边界；有冲突时以上述对应版本合同为准。
4. `docs/plugin-platform/examples/` 的早期合成清单只用于设计说明，不可替代可执行 schema，也不能拿去安装。

当前参考实现覆盖：配额读取、Token/模型用量、声明式设置/品牌/国际化的静态校验、结构化日志、有界执行、归档完整性、开发沙箱安装及匿名数据汇总。实际 TokenMe UI、通知、真实供应商认证、签到、估算、SQLite、同步和生产升级尚需后续实施票，不从参考测试推导其已经完成。

[本轮审计](AUDIT.md)记录实际复现的缺陷、修复与剩余验收门。文档可用于分票开发，不等于生产改造已通过验收。

第二轮改进见[设计补充合同](DESIGN_HARDENING.md)、[对抗审计记录](AUDIT_ROUND2.md)、[全量迁移清册](MIGRATION_INVENTORY.md)。发现/引用来源、同修订冲突、配置版本、通知投递和依赖修订按 0.5 执行。正式生产协议冻结仍是 F01 的交付门，不能把 review 模型通过当作已完成 F01。

新增“把Agent代理给其他工具”按[网关扩展](AGENT_GATEWAY.md)单列F13/G01—G07；原参考只离线读取。用户明确[WorkBuddy→Claude Code](WORKBUDDY_CLAUDE_CODE.md)包括模型网关和任务委派；[模式评估](AGENT_MODES_EVALUATION.md)建议A优先、B独立验收，分别有WB-P0和WB-D0—D3。真实接口资格/完整生成合同待完成，不算可运行能力。

## 2. 首条路线固定

| 项目 | 本次执行选择 |
| --- | --- |
| 插件开发语言 | Rust；首批 SDK 和现有适配器迁移以 Rust 为基准 |
| 编译目标 | `wasm32-wasip2`，产出 Wasm Component |
| 接口 | WIT `tokenme:plugin/adapter@0.1.0`；参数/结果 JSON 另有结构和语义校验 |
| 宿主引擎 | Wasmtime，参考版本固定为 49.0.0，不安装 CLI |
| 绑定 | wit-bindgen 0.63.0；宿主绑定由同一 WIT 生成 |
| Rust 编译器 | 参考工具链锁定 1.98.1；依赖完整记录在独立 `Cargo.lock` |
| 开发工具 | 已实现的 Python CLI，Python 3.11+、jsonschema 4.25.1 |
| 包 | `.tokenme-plugin`，ZIP_STORED，内部包含独立编译的组件和资源 |
| 首版支持 | Rust；TypeScript 作为后续独立语言适配里程碑，不阻塞这条路线 |

现在不要求实施者再从 Wasm、嵌入式 JS、原生动态库中重新选型。若实测证明该路线达不到上线预算，另写 ADR 说明证据与迁移成本，再修订合同；不得在同一阶段默默维护第二套运行时。

参考采用 Rust 标准库所需的受限 WASI 上下文：不继承环境、标准输入输出或用户目录，不预打开文件目录，关闭 TCP/UDP/DNS。数据访问走 TokenMe 的 WIT 服务。链接 WASI 库不等于授予宿主文件或网络权限。

## 3. 开发者拿到的项目

参考工程：[README](../../prototypes/plugin-platform-v1/README.md)。它是独立 Cargo workspace，不修改当前主项目的 workspace、版本号或已安装应用。

```text
plugin-project/
  Cargo.toml                         sdk / reference-plugin / reference-host 三成员
  Cargo.lock                         必须提交，禁止发版时自动漂移
  toolchain.json                     编译器/引擎/目标/API 配置
  requirements-dev.txt
  wit/plugin.wit                     共享接口，不能由某个插件擅自改宿主合同
  schema/
    manifest.schema.json             清单结构合同
    result.schema.json               执行结果结构合同
    build-info.schema.json           构建来源与模块摘要合同
    scenarios.schema.json            作者独立期望和负例
  sdk/
    src/lib.rs                       HostServices、TaskInput、TaskResult、语义校验
  reference-plugin/
    Cargo.toml                       本插件依赖和 crate 配置
    manifest.json                    插件 ID、工具 ID、任务、授权、设置和资源绑定
    src/lib.rs                       数据解析、任务分派、生成绑定和组件导出
    assets/icon.svg                  静态图标
    i18n/{en,zh-CN}.json              同 key、同 placeholder
    fixtures/happy.json              离线样本与固定时间
    fixtures/malformed-quota.json    可复现失败样本
    fixtures/scenarios.json          任务名、输入路径、独立断言与日志禁止项
    LICENSE
  reference-host/
    src/main.rs                      隔离执行、模拟宿主服务、接收/汇总与结构化诊断
  tools/
    plugin.py                        可运行的完整参考工作流
    tests/test_package.py            校验器和打包/篡改/资源测试
    tests/test_audit.py              脚手架/旧构建/旧证据/链接/冒用/并发等负例
  target/                            构建目录，不提交
  dist/                              插件包与 SHA-256，不提交
  sandbox/                           开发安装实例，不提交
  artifacts/                         验证报告和诊断日志，不提交
```

开发者日常主要修改 `reference-plugin/`。SDK 正式发布前，脚手架复制完整独立工程，避免依赖一个尚不存在的线上 SDK 包。正式 SDK 发布后再提供精简、版本依赖式模板；两种方式仍需通过相同最终组件测试。

## 4. 准备环境与第一次运行

以下命令在隔离分支的仓库根开始。工具需要的系统编译器由本机 Rust 环境提供；Windows 需要可工作的 MSVC 链接器。已有对应版本时跳过安装步骤，仍运行版本检查。

```powershell
Set-Location prototypes/plugin-platform-v1
rustc --version
cargo --version
python --version
rustup target list --installed
```

如果当前目录未使用 Rust 1.98.1：

```powershell
rustup toolchain install 1.98.1 --profile minimal
rustup override set 1.98.1
rustup target add wasm32-wasip2
```

Python 依赖与闭环验证：

```powershell
python -m venv .tools/venv
.tools/venv/Scripts/python -m pip install -r requirements-dev.txt
.tools/venv/Scripts/python tools/plugin.py verify
```

此后在该 venv 中运行本文的 `python` 命令，或始终使用上述解释器完整相对路径。已有匹配依赖的环境也可直接：

```powershell
python tools/plugin.py verify
```

`verify` 是已经存在的工具，不是未来命令占位符。第一次允许从公开包源下载锁定依赖；依赖齐备后的复跑不需要访问供应商 API。它默认仅使用匿名 fixture，不读取开发者第一方凭据。

成功判据是退出 0、该次 runId 的报告 passed，且当前源树/包/宿主/模块摘要仍与报告一致；不能仅打开一个历史 passed 文件。所有 Python 依赖的直接/传递版本由 requirements-dev.txt 固定并检查。编译成功、JSON 可解析或 native 测试通过均不足以替代最终组件运行。

## 5. 创建一个新插件并开发

从参考工程创建新目录：

```powershell
python tools/plugin.py init sandbox/my-ide --id org.example.community.my-ide
Set-Location sandbox/my-ide
python tools/plugin.py check
```

目标必须不存在；脚手架带入完整 SDK、宿主、锁文件与工具，不复制旧 target/dist/sandbox。新 ID 已写入 manifest。之后按顺序修改：

1. `manifest.json`：修改 `id`、`tool.id`、语言键、品牌、数据源、逻辑 endpoint 和任务授权。该 alpha 只接受 `quota.read`/`usage.collect`，未知能力明确失败。
2. `i18n/` 和 `assets/`：提供相同 key/placeholder 的资源及安全图标。
3. `fixtures/`：加入已匿名的输入、明确的时间和独立核算的期望值；在 scenarios.json 更新任务 ID、fixture、JSON Pointer 断言、退出码、负例和日志禁止项。每个注册任务至少有一个 `/status=ok` 的成功场景。接口域名使用合成值时注明，不将其称为真实适配。
4. `src/lib.rs`：实现供应商数据映射；通过 `HostServices` 访问服务，返回 SDK 标准类型；不自行写核心数据库。
5. Rust 测试：覆盖真实映射、缺失/异常字段、累计值和缓存口径；测试期望由独立数据核算，不能重复实现代码本身的表达式。
6. 运行 `check`、`test`、`build`、`pack`、`inspect`、`install` 和 `run`；完整标准回归仍运行 `verify`。

如果作者引入新的可选能力，先完成第 12 节对应服务/结果合同和验证，不在清单中放一个宿主会忽略的字符串来冒充支持。

## 6. 接口和结果的具体约定

### 6.1 WIT 边界

唯一接口源为 [plugin.wit](../../prototypes/plugin-platform-v1/wit/plugin.wit)：

```text
execute(task-id: string, input-json: string) -> result<string, string>
host.now-ms() -> u64
host.http(endpoint-id: string) -> result<http-response, string>
host.read-source(source-id: string, cursor: option<string>) -> result<string, string>
host.log(event: string, fields-json: string) -> result<(), string>
```

这段是阅读摘要；WIT 文件及其生成绑定定义真实签名。返回成功字符串必须符合 result schema 和 SDK 语义校验；错误字符串在 alpha 中只能是标准机器错误码，不能承载响应正文、凭据或任意异常文本。

`TaskInput` 的当前精确内容为 `cursor: string|null`，字段可省略，未知字段拒绝。完整生产上下文的账号/授权/截止期属于宿主调用边界，不由插件传入一个可伪造的全局管理员对象。

### 6.2 配额结果

```json
{
  "kind": "quota",
  "windows": [
    { "id": "daily", "used_percent": 25, "resets_at_ms": 1792440000000, "sampled_at_ms": 1792404000000 }
  ]
}
```

窗口 ID 稳定、时间为 UTC epoch 毫秒。百分比允许超过 100 以保留超额语义，alpha 数值上限为 10000；展示裁剪不应改写原始事实。配额结果不携带伪造的 Token 用量。

### 6.3 用量结果

```json
{
  "kind": "usage",
  "events": [
    { "occurrence_id": "request-1", "model": "model-a", "ts_ms": 1792400000000, "input": 120, "cache_creation": 0, "cache_read": 0, "output": 80 }
  ],
  "next_cursor": "1"
}
```

计数为非负整数，单字段与四阶段总量均不得超过 JSON 安全整数上限；单批最多 1000 条。reasoning 将来作为 output 子拆分扩展，不额外累计。Credits/金额/估算使用独立合同，不能占用这些实际 Token 字段。

当前参考账本为单一合成工具/账号作用域。生产集成必须使用工具/账号/真实发生身份进行归属和去重，插件版本不进入事实身份。相同事实重放不累计，修正记录替换旧事实并修复汇总。

宿主先完整校验，再提交批次和下一游标；任一必须拒绝的记录使该批事务失败。部分接受需要另行设计明确协议，不能靠异常后继续推进游标。

## 7. 验证分层、命令和证据

| 层 | 真实命令/入口 | 验证内容 | 通过条件 |
| --- | --- | --- | --- |
| 清单与资源 | `python tools/plugin.py check` | schema、ID、引用、授权、语言、placeholder、SVG | 无未知能力、缺失授权或悬空引用 |
| Python 合同 | `python -m unittest discover -s tools/tests -v` | ZIP、路径、签名状态、摘要、资源篡改等 | 正反例全部符合预定分类 |
| Rust native | `cargo test --workspace --locked` | 解析/错误、计数/结果、幂等、修正、批次原子性 | 独立 fixture 期望全部通过 |
| 构建 | `python tools/plugin.py build` | 原生宿主和最终 Wasm Component | 锁定依赖构建成功；输出存在且为组件格式 |
| 包静态检查 | `pack` 后 `inspect <package>` | 包内容、摘要覆盖、文件类型与资源 | 无路径穿越、重复路径、未覆盖文件或非法资源 |
| 最终组件 | `python tools/plugin.py verify` | 实际 Wasm 执行、宿主授权、结果、重复、错误/门控/预算 | verification.json passed，逐场景独立证据 |
| 真实 TokenMe 集成 | 第 12 节 F07/F08 的面板和 CLI 检查 | 注册到实际索引、设置、展示和通知 | 真实应用中安装新 ID 不改主程序，不重复计算 |
| 安装/跨平台 | 各平台发行包安装与图形验收 | 内嵌插件、权限、数据保留、性能、真实设备 | 分开记录构建、单机运行和跨设备结论 |

包结构检查不等于执行其字节码；native 测试不等于 Wasm 测试；参考宿主测试不等于桌面应用验收。报告必须保留这三个区别。

标准匿名用量样例两条请求为 120+80 和 300+50，独立总量 550；重复两次执行，仍为 2 条事实、550 Token，重复计数为 2。配额为 25%，不会让 Token 总数增加。

每个插件额外提供自己的 fixtures/期望；不能把固定参考样例的总数机械套用于真实供应商。

scenarios.json 的精确格式由 schema 定义；断言从固定输入独立核算，不能运行一次后把观察值自动抄回当 oracle。平台负例由工具自身测试维护，插件场景还须覆盖自己的协议错误；场景数量不等于全部权限沙箱已验证。

## 8. 打包方式和字节合同

```powershell
python tools/plugin.py build
python tools/plugin.py pack
python tools/plugin.py inspect dist/org.example.tokenme.reference_0.1.0.tokenme-plugin
```

包包含且仅包含：manifest、一个组件、声明的图标/语言、LICENSE、build-info、checksums、signature。源码、匿名测试输入、用户日志、第一方凭据和整个 SDK 不进入插件包。

可复现归档规则：

- 文件名为规范化 POSIX 相对路径，排序后写入；拒绝 `..`、绝对路径、反斜杠、盘符、Windows 保留名、大小写冲突和 symlink。
- ZIP_STORED；固定时间 1980-01-01 00:00:00，文件属性为普通文件 0644。
- JSON 元数据采用 UTF-8、排序 key、紧凑分隔、末尾一个换行；拒绝重复 key 和非有限数字。这是本 alpha 的精确序列化规则，不冒充完整 JCS 实现。
- `checksums.json` 显式覆盖所有载荷的 SHA-256/大小，排除它自己与签名信封，避免循环。
- 每文件最多 32 MiB、总载荷/包最多 64 MiB、最多 64 文件；JSON/schema 还有更小限额。预算由宿主控制。
- build 先令构建凭据不可用，成功后写 target/build-receipt.json，绑定源树/宿主/组件摘要；pack 拒绝缺失凭据、旧源码或变化的组件/宿主。源树覆盖锁文件、依赖、README、SDK、插件、fixture、WIT/schema 和工具。
- build-info 记录工具链、源树及模块摘要并经 schema 校验，不写个人路径；重复打包比较同一组件的归档字节，不宣称独立干净环境重建可复现。
- guest 构建通过单次子进程的 remap-path-prefix 将项目、依赖和 sysroot 路径映射到中性前缀，不修改用户全局 Rust 配置；verify 检查组件中不残留本机项目/依赖根。

输出包、整包 `.sha256` 和验证报告一起交付。当前签名状态为 `unsigned-local`，只能进入显式允许的开发沙箱；不能将 SHA-256 一致称作发布者签名已验证。

生产签名票 F02 要固定签名消息为域分隔符 `tokenme.plugin.package/v1`、换行和 checksums 原始字节的 SHA-256 二进制值，使用 Ed25519；信封记录 publisher/key ID、算法、integrity 摘要和 base64 签名。可信密钥来自宿主信任库，不能来自包内自签名的 `official` 字段。该生产格式在 F02 的正反例通过前不加入可接受签名类型。

## 9. 安装、运行、调试和问题定位

```powershell
python tools/plugin.py install dist/org.example.tokenme.reference_0.1.0.tokenme-plugin --allow-unsigned-local
python tools/plugin.py run org.example.tokenme.reference --task collect-usage --repeat 2
python tools/plugin.py run org.example.tokenme.reference --task read-quota
```

当前安装目标固定在参考工程 `sandbox/`，不是用户 TokenMe 目录。按 `<id>/<version>/<package-sha256>` 保存不可变构建，暂存后原子切换 active.json；开发同版本不同构建保留旧摘要目录。生产 registry 对一个已发布版本绑定一个摘要。

安装器使用已验证的同一份源包字节、每插件独占锁、随机 exclusive 临时文件；拒绝 I/O 路径上的 symlink、Windows reparse point 和文件 hardlink。active 的插件/版本必须匹配包内身份。仅接受规范 ZIP 字节，避免 inspect 通过、安装后重建摘要却失败。该便携开发工具不能保证抵御同一 OS 用户的恶意并发路径替换，生产 F02 使用原生 handle 操作和独立 profile；不能因此把开发沙箱宣称为生产安装器。

每次运行重新核对安装载荷和摘要再加载；不授予插件读取安装目录以外的文件。未知 ID、未知 task、未授权资源或能力不兼容明确失败。

查看报告 artifactDirectory 下 `<task>-<case>.json` 与 `.jsonl`；手动 run 写入 artifacts/manual 的新目录。通过 trace ID、task_run_id、阶段、插件版本和模块摘要关联；验证器核对日志序号/插件/任务/trace。正常日志无响应正文和绝对路径，fixture 秘密 marker 不得出现在正式日志中。

参考宿主对构造与每次调用执行清单 timeoutMs，使用 10ms epoch tick，另有每调用 1000 万 fuel、每个 linear memory 64MiB 上限；这不是整个 Store/进程的内存总上限。Python 对包括编译的整个子进程单独限制 30 秒。epoch/fuel 不替代 native I/O 取消；生产累计 memory/table/instance、JIT/IPC/子进程树预算按 F03 验收。

当前没有伪造 `tokenme-plugin dev` 或 IDE 断点承诺：作者修改后执行 build/verify；fixture runner 就是已可用的开发执行环境。F09 再交付有监督的自动 rebuild/reload、符号映射和健康 UI，且不双跑副作用任务。

## 10. 可执行参考的验收报告

参考工具在检查前将最新报告写为 running，失败/中断写 failed/interrupted；进程被强杀可能留下 running，绝不能解释为通过。每次生成 artifacts/runs/<runId>/verification.json 和独立场景文件；latest 是同一次结果副本，旧 run 保留。报告含工具/依赖版本、源树/包/宿主/模块摘要、逐场景结果、Wasm 实际执行、运行时间，以及 mainAppIntegrated=false。

每个场景保留独立 JSON/JSONL，不能复跑覆盖另一种场景的证据。最终复查命令为：

```powershell
Get-Content artifacts/verification.json
$verificationRecord = Get-Content artifacts/verification.json | ConvertFrom-Json
Get-Content (Join-Path $verificationRecord.artifactDirectory 'collect-usage-replay.json')
Get-Content (Join-Path $verificationRecord.artifactDirectory 'read-quota-denied.json')
```

首次真实工具迁移还须提交采集→事实→索引→汇总逐字段对账，以及独立的凭据/签到副作用测试；本参考报告不是这些票的通过证明。

## 11. 生产模块接口与责任

以下为待实现的生产模块接口；[生产合同](PRODUCTION_CONTRACTS.md)给出字段、边界、事务和状态机，对应实现票负责将它们生成/落实为类型和绑定并通过合同测试，不能仅创建空目录宣称完成。

| 模块 | 对外接口合同 | 实现要求 |
| --- | --- | --- |
| PackageStore | `verify(source)->VerifiedPackage`；`stage(package)->StagedInstall`；`activate(staged, grants)->Activation` | 与参考包字节规则一致，生产签名/迁移/回退，禁止包自增权限 |
| PluginRegistry | `register(VerifiedPackage)->Registration`；`snapshot()->RegistrySnapshot`；`deactivate(id,generation)` | 元数据和任务单一真源，原子注册，保持工具历史身份，拒绝无意覆盖 |
| TaskScheduler | `reconcile(settings, registry)`；`enqueue(TaskInvocation)`；`cancel(generation)` | 全局/插件/账号门、截止期、租约、退避和 side-effect 规则，不清最后缓存 |
| ComponentRuntime | `execute(context, invocation)->ExecutionOutcome` | 生成 WIT 绑定、限定 WASI、逐调用授权、预算、取消及模块摘要 |
| HostServices | `http(grant, params, deadline)`；`read_source(grant,cursor,limit)`；受控系统服务 | 网络/文件真实实现，重定向逐跳验证，路径归属，秘密句柄与日志元数据 |
| FactReceiver | `accept(scope,batch,source_revision,cursor)->CommitReceipt` | 校验身份和语义，事务提交事实+游标，去重/修正/rollup 修复 |
| SettingsRegistry | `describe(plugin)->SettingsSchema`；`set(key,value,revision)` | 类型/归属/CAS 校验；execution、notification_policy、presentation revision 分离，通知/主题变更不丢采集结果 |
| PresentationRegistry | `describe(plugin,locale,theme)->PresentationSnapshot` | 图标/品牌、标准组件、语言回退和历史快照，不执行任意 HTML |
| NotificationRouter | `observe(event,rule,policy)->DeliveryDecision` | 稳定事件身份、全局门、限频、静音期间状态推进和系统投递 |
| Diagnostics | `status/query/explain/export` | 有界只读排查、健康状态、全链路引用和本地脱敏导出 |

生产 TaskInvocation 包含 plugin/digest/generation、task/handler/capability、带 kind 的 Scope、trigger、settings_snapshot_revision/execution_revision、grant revision、deadline、budget、trace 和输入；引用及 output batch/operation 来自宿主，发现阶段不要求尚不存在的账号/来源。

`CommitReceipt` 固定包含：批次 ID、接受/去重/修正/拒绝计数、已提交游标修订和报表修订引用。只有成功 receipt 才推进来源状态。

## 12. 可直接领用的实施票与依赖

参考代码用于启动实施与锁定合同，不跳过下面的生产接入工作。

| 票 | 修改位置与交付 | 前置 | 验证/完成标准 |
| --- | --- | --- | --- |
| F01 API 与 SDK | `crates/usage-plugin-api`、`usage-plugin-sdk`、`usage-plugin-testkit`；迁入并扩展参考 WIT/schema/类型 | 本参考通过 | `cargo test -p usage-plugin-api -p usage-plugin-sdk -p usage-plugin-testkit`；同一 fixture 的 native/最终组件输出一致 |
| F02 包与信任 | `usage-plugin-host/package`、`store`、签名和升级状态 schema | F01/F03 | 摘要、签名信任、越权扩张、归档攻击、崩溃恢复正反例；升级迁移使用真实隔离 runtime；只变更新建 profile |
| F03 运行器 | `usage-plugin-runtime` 与独立 runner 入口 | F01 | 最终组件授权、内存/fuel/截止期、native 崩溃隔离、IPC 有界；其他任务和报表持续更新 |
| F04 注册与调度 | `usage-plugin-host/registry`、`scheduler` | F01/F02/F03 | 动态新 ID、门控、租约、休眠补跑、generation；轮询禁用保留缓存，不触发强制刷新 |
| F05 真实宿主服务 | `usage-plugin-host/services`、平台系统 API、认证服务 schema | F02/F03/F04 | 发现/Ref/observation、HTTP/JSONL/SQLite/WAL、DPAPI/凭据、受控程序；auth.refresh 的 lease/CAS/未知状态和取消；无厂商特例硬编码 |
| F06 索引接收 | `usage-index`、动态 SourceAdapter 代理、结果映射 | F01/F04 | 事件+游标同事务，多源/账号/设备去重，累积/修正，旧数据对账；复用旧索引而非一并重写 |
| F07 设置与展示 | 面板 `commands/bridge/types`、插件元数据 provider、设置和标准视图 | F02/F04/F06 | 实际安装新 ID 后无需加静态表即可显示品牌、设置、用量与今日汇总；语言/主题切换通过 |
| F08 通知与诊断 | `notify`、`logging/diagnostics`、健康页及只读 CLI/MCP scope | F04/F06/F07 | 请求到展示可定位，静音不补发，失败投递可解释；日志有界、秘密 marker 不导出 |
| F09 作者工具 | 将参考工具迁为维护的 `tools/tokenme-plugin`；SDK 版本化模板 | F02/F03/F07/F08 | 空目录创建→实际构建→开发 profile 安装→执行→诊断；重建收敛旧任务，不双跑签到 |
| F10 签到/估算/价格 | 落实 v1 能力处理器、SDK 客户端、作者样例、通用接收和设置绑定 | F04/F05/F06 | 签到先查询、结果未知核对、周期/互斥；估算有来源/版本，未计入真实 Token 的重复总和 |
| F11.1 代表性迁移 | `plugins/official/codex`、`qoder-cn` 及轮换 mock | F05/F07/F08/F10 | 标准事实/报表对账；批准的旧缺陷修正单列；影子不双跑真实写操作 |
| F11.2 全量迁移 | MIGRATION_INVENTORY.md 每个 tool 及工具外估算/定价/测速/品牌策略 | F11.1 | 每能力来源、配置/游标/缓存映射、最终组件与切换回退留证据；包含 quota-only 工具；适用能力不得空白 |
| F12 CLI/collector/交付 | CLI 共享注册、内嵌官方包、构建/安装脚本、平台验证 | F09/F11.2 | 禁用 legacy bridge 后全量能力可用，移除逐工具 production path dependency；collector/数据保留/平台与真实设备分别留证据 |
| F13 Agent 网关/委派（可选） | AGENT_GATEWAY.md/AGENT_MODES_EVALUATION.md的G01—G07：A模型API、B任务MCP，共享通用listener/consumer/长IO/数据与控制面 | F05/F06/F07/F08/F09/F10 | A经WB-P0、B经WB-D0—D3分别验收；真实流/工具/取消/会话权限/未知结果/归属去重/升级；原alpha不计通过 |

表内 `cargo test -p ...` 是该票创建生产 crate 后的验收命令，当前立即可运行的命令仅限第 4—10 节参考工程。面板是独立 workspace，F07/F08 还须在其目录运行前端类型/构建及 Rust 检查；Windows 既有测试进程启动问题按仓库约定单列，不用根 workspace 结果替代。

依赖波次：F01 → F03 → F02 → F04 → F05/F06 → F07 → F08/F10 → F09/F11.1 → F11.2 → F12。F06 可先用只读 mock 开发，生产完成仍需真实来源验证。实施按可评审范围提交，禁止 cargo fmt，不在原工作区改造。

F13从公共核心分出，不以F11.2/F12为前置，也不使它们反向依赖Agent。A先按WB-P0步骤1—2、B按WB-D0的协议证据分批冻结G01；后续真实组件/上游/客户端联调属于G04/G07，不能反过来设为全部schema起草前置。两模式不互相标通过，各G/WB-D票见网关及评估文档。

## 13. 扩展能力的精确输入输出要求

F01 以[生产合同](PRODUCTION_CONTRACTS.md)生成以下记录的 schema/绑定，F05/F10 实现服务和能力并复核其 fixture。alpha 0.1 不接受尚未实现的导入或结果。

| 能力 | 输入记录 | 输出记录与约束 |
| --- | --- | --- |
| 真实 HTTP | endpoint ID、路径/查询白名单参数、账号凭据句柄、deadline/size limit | status、受限响应句柄/bytes、采样时刻；日志仅 endpoint ID/耗时/错误类别 |
| SQLite | source grant、read-only query ID/SQL 和类型化参数、游标/row limit | 有界行、来源/WAL 修订及下一游标；只读校验，禁止 ATTACH/扩展加载，取消必须可中断查询 |
| 账号与认证 | 账号来源描述、认证动作 ID、credential slot、expected revision | 不透明句柄、到期/登录状态和新 revision；读/用/轮换/写回权限分开，CAS 与第一方锁协议 |
| 签到 | action ID、tool/account、厂商周期、manual/automatic、idempotency key | claimed/already_claimed/not_available/login_required/outcome_unknown/failed；未知结果不能当成功或立即重复领取 |
| Token 估算 | 标准事实引用、配额窗口、覆盖时间、模型及算法配置 | estimated tokens、输入引用、覆盖率、算法版本、限制和生成时刻；与 observed 分开 |
| 燃值/价格 | 用量/估算引用、模型/credits 原生单位、价格源、币种/有效期 | 十进制金额和币种、价格/换算版本、来源、估算标识；不直接改原始事件，跨币种不盲加 |
| 通知 | 事件类型/身份、规则/schema/默认值、全局模板、按应用继承/覆盖、语言键和 action | 有效策略及字段来源、DeliveryDecision 和原因/投递结果；总控优先、恢复继承、不补历史告警；见生产合同 §7.1 |
| 专属设置/视图 | 稳定键、类型、默认值、scope、受控目标、语言/主题/组件绑定 | 校验后的设置 revision/展示快照；不创建逐工具专属主程序分支 |

每一项的完成定义相同：schema → SDK/绑定 → 授权服务 → 作者样例 → 正反 fixture → 最终组件 → 实际 TokenMe 接入与诊断。缺少任一步，能力不进入正式支持清单。

## 14. 故障处理与回退

| 失败阶段 | 开发者动作 |
| --- | --- |
| `TOOLCHAIN.RUST_VERSION` / WASM_TARGET | 对照 toolchain.json 设置目录级工具链和目标，不能修改应用版本来绕过 |
| `SCHEMA.INVALID` / LOCALE / TASK.PERMISSION | 修正清单/资源引用，重新 check；不在宿主忽略字段 |
| `MODULE.IMPORTS_UNSUPPORTED` | 检查 WIT 与 Rust 标准库的 WASI profile；不能通过继承全部用户环境/目录来解决链接 |
| `PERMISSION.DENIED` | 核对任务与插件的交集授权；不将鉴权失败当作解析成功后的 0 |
| `PARSE.SCHEMA_MISMATCH` | 加入匿名复现样本，调整插件映射，重跑 native 与最终组件；不把响应正文写进正式日志 |
| HASH / PACKAGE.PATH / 签名错误 | 停止安装，保留原 active；重新构建正确包，不手改 installed 文件绕过 |
| 预算超限 | 查看阶段和计数，缩小批次或修复循环；预算调整须有实测且由宿主控制 |
| 生产升级失败 | 暂停新调度，恢复兼容的 active 与状态快照；结果未知的写任务先核对，不自动清空历史 |

## 15. 交付审核清单

一次插件交付必须能给出：源项目与锁文件、manifest/WIT/API 版本、SDK 版本、匿名输入与独立期望、native 和最终组件结果、包及 SHA-256、签名/信任状态、兼容环境、诊断方法和已知未验收项。

一次平台交付还必须给出：动态安装未知 ID 的实际界面证据、设置和通知联动、现有数据迁移对账、共享 CLI/collector 结果、运行预算与首屏回归，以及对应平台安装和实际设备证据。

以这些产物作为“拿到即可开发”和“开发闭环通过”的判断标准；文档页数、仅可解析的示例 JSON 或尚不存在的命令不作为完成证据。

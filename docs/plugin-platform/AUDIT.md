# 开发闭环审计与对抗审计

**2026-10-20 更新：本页保留首轮事实；最新结论以[第二轮对抗审计](AUDIT_ROUND2.md)为准。** 再次核验发现 scope/修订/通知/凭据/依赖等设计缺口，0.5 已修订，不能直接沿用首轮“已闭环”判断。生产协议冻结和主程序验收仍是明确交付门。

日期：2026-10-19。基线：隔离分支 `codex/plugin-platform`。审计对象为本目录设计及 `prototypes/plugin-platform-v1/`，不是对 TokenMe 全仓或全部 Wasmtime 安全边界出具认证。使用 BugX 局部审计技能的复现/回归方法；不启动全库 Worker/云端同步/自动提交。技能为本机工具，不是工程依赖。

## 1. 结论与三个完成标准

**审计前未达到可直接执行的完整开发闭环。** 架构方向与独立参考工程已存在，但新项目模板、源码/包对应、安装边界和失败证据存在可复现断点；生产部分还缺少足够具体的跨模块合同。

本轮修复参考工具，并新增 [生产协议与状态合同](PRODUCTION_CONTRACTS.md)，同步修订实施规格、总体方案、SDK 和诊断规范。当前可按 F01—F12 分票开工。完成状态必须分别表述：

| 标准 | 本轮结论 | 不可替代的证据 |
| --- | --- | --- |
| 可按方案实施 | 合同、责任、状态机、默认限制、依赖和验收已补齐；F01 将文档合同生成正式 schema/绑定 | 本文追踪矩阵和生产合同；schema/实现变更必须通过本票 oracle |
| 参考工程作者闭环 | 读取能力的最终组件/包/开发安装已复验；新项目完整验证结果以参考证据为准 | 带当前摘要的 verify 报告、新 ID/任务/输入的独立 scaffold 报告 |
| 正式 TokenMe 插件化闭环 | **未完成**；F01—F12 仍为待实现票 | 真实应用安装未知 ID、索引/汇总/设置/主题/国际化/通知、真实服务/迁移/平台证据 |

本轮没有修改生产核心、安装现有应用、部署后台、推送或发布。不能把方案通过审计等同于产品能力已上线。

末次检查发现原工作区 public 已由其他并行工作推进到 4c29b7b0f3873dd2d8ca59f6679d03df78d0ce6e，并出现面板 Cargo/设置/bridge 等变化；本轮没有合并、回退或覆盖它们。隔离分支仍以 e1cf55d 为审计基线。进入 F01/F07 前先比较最新主线接口、在隔离分支集成需要的变更并复验；本报告不覆盖那些并行改动。

## 2. 有负对照的实际缺陷

先新增独立回归，再在原工具上运行，六项全部失败，保留完整 traceback：baseline-red.txt（本地记录：`prototypes/plugin-platform-v1/artifacts/audit/baseline-red.txt`）。修复后相同用例通过；完整 verify 同时执行全部回归。所有路径攻击仅针对临时合成文件，未读取用户秘密或改动真实 TokenMe profile。

| 编号 / 级别 | 复现与影响 | 修复 / 回归 |
| --- | --- | --- |
| R01 / P1 | init 修改 manifest ID 后执行 test_package，断言仍要求旧 ID，导致新作者项目无法通过自带测试 | 测试读取项目身份；`test_scaffold_package_checks_accept_its_new_identity`；另做完整新项目验证 |
| R02 / P1 | 放入旧组件后调用 pack，无成功构建凭据也能打包，并写入当前源树摘要；旧产物冒充当前源码 | build receipt 绑定源树/宿主/组件；pack 核对；缺凭据、源码变化、组件变化分别拒绝 |
| R03 / P1 | 已有 passed，注入 verify 启动失败，latest 仍为旧 passed，容易把失败交付当成功 | 开始即 running，finally 写 failed/interrupted；每次 runId 独立目录，旧记录保留 |
| R04 / P1 | 安装前将固定 active.tmp 硬链接到合成 sentinel，安装会覆盖 sentinel 内容 | 随机 exclusive 临时文件、I/O 路径拒绝 hardlink/reparse/symlink、插件独占锁；sentinel 回归确认保留 |
| R05 / P1 | 检查器接受 ZIP_DEFLATED 包，安装保存原字节摘要，运行重建 ZIP_STORED 后摘要不同，形成“可安装但不能运行” | 输入必须为规范 ZIP_STORED 且逐字节等于规范重建；检查/安装/运行使用同一摘要规则 |
| R06 / P1 | 将有效包放到另一个插件 ID 的 active 目录，运行直到寻找宿主阶段仍未拒绝身份；准备了宿主就会按错误命名空间加载 | 在访问宿主前核对 manifest ID/version 与 active 路径，返回 INSTALL.IDENTITY |

R04 的权限前提是本地开发目录可被同一用户/进程写入，不是远程无条件文件写漏洞。便携 Python 工具修复已知静态链接/临时名攻击，仍不宣称抵御恶意同用户 TOCTOU；生产 F02 必须通过原生 handle 边界验收。

编码对抗另保留 encoding-red.txt（本地记录：`prototypes/plugin-platform-v1/artifacts/audit/encoding-red.txt`）：UTF-16 DTD 图标绕过 ASCII 字节检测、UTF-16 JSON 偏离 UTF-8 合同，两项修复前均未拒绝。现强制 UTF-8，再做 DTD/entity/静态节点校验，禁止纯形状 SVG 携带非空文本；相同负例进入完整回归。这属于 R07/P2 的输入格式绕过，不声称已证明远程代码执行。

## 3. 追加的合同与健壮性修复

以下项目通过源码审查、定向测试或完整执行确认；没有事前负对照的项目不冒充“全部都已独立利用”。

| 编号 | 原断点 | 本轮处理与证据 |
| --- | --- | --- |
| C01 | verify 写死任务名、550 Token、25%，自定义插件不能携带独立 oracle | scenarios schema/文件，JSON Pointer 类型感知断言；每任务至少一条成功场景；新项目改为新任务/560/37% 完整验证 |
| C02 | task timeoutMs 只声明、没有真正用于组件执行 | 原生 epoch clock 同时限制构造/调用；无限 guest loop 的无 fuel 中断测试；Python 另控整个进程 30 秒，避免把 JIT 误算为任务时间 |
| C03 | 安装源包先校验后再次读取摘要，可遇到内容替换；并发激活没有独占 | 使用一次字节快照；每插件安装锁及竞争拒绝用例；active 严格结构校验 |
| C04 | manifest 未经 schema 就解引用；JSON 的 1e999 可变为 inf；重复诊断 ID / SVG DTD 或外部命名空间没有完整拒绝 | 先 schema、JSON 大小/深度/finite 校验、诊断 ID 唯一、静态 SVG 大小/元素/属性/DTD/namespace 限制及回归 |
| C05 | 安装目录运行读取时只有单文件限制，没有累计/数量限制 | 遍历前路径检查，文件/目录项数量、累计大小及单文件大小共同限制 |
| C06 | build-info 无精确 schema；验证依赖和构建 target 可能漂移 | build-info schema、模块摘要对应、Python 传递依赖固定并检查；显式 target-dir、guest 构建路径 remap |
| C07 | 重复复验覆盖既有场景文件，trace 关联只由人检查 | run 目录隔离；日志插件/任务/trace/sequence 自动核对，秘密 marker 与缺失 host event 断言 |
| C08 | README 将 memory_size 的每 memory 限制写成整个 Store 上限 | 对照锁定 Wasmtime 源码修正文案；生产合同明确累计 ResourceLimiter/多 memory 对抗门，参考不冒充已实现总内存限制 |

执行记录见 [参考证据](REFERENCE_EVIDENCE.md)；[源码回归](../../prototypes/plugin-platform-v1/tools/tests/test_audit.py)为可复现入口。原生 epoch 单元测试不代替任意恶意 Component、平台内存和 native I/O 取消验收。

## 4. 设计缺口及落点

| 缺口 | 本轮固定的设计 | 实施归属 |
| --- | --- | --- |
| 只有能力名/职责，缺明确字段和错误语义 | profile/WIT 演进、TaskContext、Scope、Cursor、Receipt、各能力输入输出与单位 | 生产合同 §1/3/4；F01/F10 |
| 事实、账号、来源、设备混同 | contribution 合成、稳定 occurrence 域、跨设备去重、修正、累计差量、游标同事务 | §3/4；F06 |
| 读取/签到/凭据写的失败重试混用 | 写角色、周期、先查询、未知结果 reconcile、CAS、撤权/停用后的未决状态 | §4/5/6；F04/F05/F10 |
| 摘要等于签名、信任或升级回退过于概括 | 签名消息、key 绑定/轮换/撤销、升级权限差异、事务激活、私有状态快照/迁移 | §2/8；F02/F04 |
| 网络/文件/SQLite 的“受控”缺执行规则 | endpoint 参数、重定向/DNS、native handle、WAL/authorizer、程序身份与真正取消 | §5/6；F03/F05 |
| 配色/i18n/设置/通知只列功能 | setting scope/revision、组件 binding、资源协议、语言回退、事件 outbox 与通知去重/静音 | §7/9/11；F07/F08 |
| 运行预算、日志速率只有建议 | guest/任务/IO/并发初始硬限，日志队列/速率/磁盘，待测全局性能门 | §6/9 与诊断 §9；F03/F08 |
| 动态插件仍可能受静态 trait/UI/列表阻挡 | SourceAdapter 静态生命周期改造、registry 单一真源、前端动态 ID、保留历史元数据 | §11；F04/F06/F07/F11 |
| 扩展“无限”却没有兼容原则 | 已知版本能力协商、可选能力禁用、DAG provider、未知算子需宿主升级 | §1/4；F01/F04 |
| 后台是否前置/如何分发 | 本地包/运行先交付；市场后台独立后续；凭据默认留本机 | 总体方案 §1/11；F02/F12 |

## 5. 需求到验收的追踪

P=参考部分通过；U=生产未验证；D=后续独立范围。**没有生产项被本次离线报告标记为通过。** 对原方案 A01—A22 的状态逐项如下。

| 验收 | 合同 / 实施票 | 本轮状态与后续 oracle |
| --- | --- | --- |
| A01 新 ID | §1/2/11；F02/F04/F07 | P：参考宿主新 ID；U：真实 UI 无工具表/command 修改 |
| A02 仅配额插件 | §4.2/7；F06/F07 | P：配额不制造 Token；U：缺能力的真实界面不显示伪 0 |
| A03 单位/估算/金额 | §4.1/4.4；F06/F10 | P：四阶段 observed；U：credits/多币种/缺价/估算不重复 |
| A04 增量/崩溃/WAL | §4.1/5；F05/F06 | P：内存账本幂等/修正/原子负例；U：持久游标、重启、日志轮转、WAL |
| A05 多源/多设备 | §3/4.1；F06/F12 | U：同 occurrence 重复与互补贡献/来源撤回的对账 |
| A06 开关联动 | §6/7；F04/F07/F08 | P：参考禁用不调用 HTTP；U：保留真实缓存、不开签到、不补通知 |
| A06-N 全局/应用通知继承 | §7.1；F01/F07/F08 | U：全局统一默认、按 tool_ref 稀疏覆盖、恢复跟随、总控优先；修改规则不请求供应商/不补历史；同包多应用相互独立 |
| A07 签到未知/竞争 | §4.3/6；F04/F10 | U：写响应丢失、崩溃、设备失联；无盲重试 |
| A08 凭据轮换 | §5；F05/F11 | U：DPAPI/平台库、原客户端竞争、revision CAS、原子写回 |
| A09 语言/品牌/主题 | §7/11；F07 | P：资源键/placeholder/SVG 静态校验；U：真实切换与回退 |
| A10 资源与隔离 | §6；F03 | P：fuel 失败、epoch native loop；U：恶意组件大内存/输出、native I/O、runner 崩溃后主程序继续 |
| A11 服务越权 | §5；F03/F05 | P：参考 endpoint grant 拒绝、包路径负例；U：DNS/重定向/账号句柄/真实文件边界 |
| A12 信任/安装 | §2；F02 | P：篡改/规范 ZIP/链接/身份/未签名门；U：签名/key 轮换/撤销/权限扩张 |
| A13 生命周期 | §2/8；F02/F04/F06 | P：开发不可变包、锁；U：生产激活/迁移/崩溃恢复/回退/卸载历史 |
| A14 CLI/桌面并发 | §4.3/6/11；F04/F12 | U：同 profile lease/fencing，未决写不重复 |
| A15 平台与设备 | §10/11；F12 | P：本机 Windows 参考；U：macOS/Linux、真实设备与安装 |
| A16 首屏/性能 | §6/10；F03/F12 | U：相同样本基线、首屏与公共报表持续性、JIT/RSS/连续稳定性 |
| A17 数据到视图诊断 | §9/11；F06/F08 | P：参考 task→接收→报表 trace；U：真实索引/作用域/视图 explain |
| A18 日志降级 | §6/9、日志规范；F03/F08 | P：参考事件条数限制；U：队列/磁盘满/轮换/到期和 dropped UI |
| A19 诊断/秘密 | §5/9；F08 | P：marker 不进日志；U：真实本地导出/匿名引用/doctor 无副作用 |
| A20 作者闭环 | 实施规格 §3—10；F09 | P：参考完整组件/包/安装与新 scaffold（以证据记录为准）；U：作者装入真实 TokenMe |
| A21 后续语言 | §1/10、SDK 规范；独立语言票 | D：TS/JS 无正式支持声明；需同合同和目标平台报告 |
| A22 身份盖章 | §3/9；F03/F08 | P：结构化字段/trace 核对；U：恶意 guest 冒用上下文的最终组件对抗 |

## 6. 必须阻断正式交付的项目

以下是实现/验收工作，不能改写成“文档已说明所以已完成”：

1. F01 的生产 v1 WIT/schema/SDK/TestKit 尚未生成和落入主工程；alpha 只支持两个读取能力。
2. F02/F03 的生产签名信任、handle 安装、独立 runner IPC、全局内存/JIT、真实 I/O 取消与恶意组件边界未验收。
3. F04—F08 未接入主程序的注册、租约、索引、缓存、UI、设置、语言、通知、日志/健康和本地诊断导出。
4. F10/F11 未完成实际认证、SQLite/WAL、签到/写回未知状态、估算/价格和旧历史逐字段迁移对账。
5. F12 未完成 CLI/collector、各平台安装、首屏/持续运行与真实跨设备验证。

开发顺序仍使用实施规格 F01—F12。每票 PR/交付必须引用上表的 acceptance ID、独立 oracle、最终组件和真实接入证据；未执行项标 U，不用“全部通过”覆盖。

## 7. 复核步骤

在参考工程目录运行 `python -m unittest discover -s tools/tests -v` 复现回归，再运行 `python tools/plugin.py verify`。读取 latest 的 runId/status/hash，再进入 artifactDirectory 核对逐场景 JSON/JSONL。重新编辑后先 build/verify，不拿旧报告验收新源码。

新作者项目的完整验证应从不存在的目录 init 开始，至少变更 plugin/tool ID、任务名和样本期望；本轮脚本和结果保存在 artifacts/audit，具体位置/摘要见参考证据。它是合成协议验证，不冒充 Devin 已接通，也不访问供应商数据。

本审计为定向白盒/负例/最终组件验证，未进行无界 fuzz、全仓覆盖或安全认证。Windows/macOS/Linux 的生产边界仍按上表单独验收。

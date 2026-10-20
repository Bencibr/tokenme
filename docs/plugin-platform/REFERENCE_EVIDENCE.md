# 可执行参考闭环验证记录 0.4

> 本页下方保留2026-10-19历史运行。提交到plug前将绝对路径负例改用通用占位符，并于2026-10-20重新执行完整verify；当前源树/包摘要以本次记录为准。宿主和组件代码未变。

日期：2026-10-19（Asia/Shanghai）。环境：Windows x64；隔离分支 codex/plugin-platform，基线 e1cf55d2a86c7ff668d2c1dacbcefc961b8b16d6。
入口：[审计](AUDIT.md)、[实施规格](IMPLEMENTATION_SPEC.md)、[生产合同](PRODUCTION_CONTRACTS.md)、[参考工程](../../prototypes/plugin-platform-v1/README.md)。

## 最终复验

以下两次最终 verify 均退出 0，包含包/schema/资源和对抗回归、Rust native 合同、实际 Wasm 构建/执行、重复打包、隔离安装、逐场景/trace 断言、新身份且宿主不重建。之后只编辑内部设计文档，没有再修改参考工程。已重新核对当前源树、包及 .sha256、宿主、组件摘要，全部与报告一致。

| 项目 | 标准参考 | 新作者项目 |
| --- | --- | --- |
| 插件 ID | org.example.tokenme.reference | org.example.audit.author-ide |
| 工具 ID | example_ide | audit_author_ide |
| 用量/配额任务 | collect-usage / read-quota | audit-usage / audit-quota |
| 独立预期 | 550 Token，25% 配额 | 560 Token，37% 配额 |
| 按模型 | 200 / 350 | 210 / 350 |
| 重放 | 2 条事实、重复 2 条，总量仍 550 | 2 条事实、重复 2 条，总量仍 560 |
| 负例 | 权限、格式/秘密 marker、关闭、fuel | 相同负例，使用更换后的任务名 |
| 状态 | passed，实际 Wasm 执行 | passed，实际 Wasm 执行 |
| runId | 228cf431212f4d919ac754e89747e17e | 0582ce8331c0498e86e5a62c47dc2c6c |
| 完整复验耗时 | 95.95 秒 | 95.613 秒 |

新项目通过 init 从不存在的目录创建，修改 plugin/tool ID、Rust 分派任务名、fixture 数值和独立 scenarios 期望。审计期间同步了修复后的工具/README，最后重新完成整个流程；没有用原样板固定总数替代自定义期望。它仍是匿名合成协议，不声称 Devin 或其他真实供应商已经接通。

## 不随下一次 latest 移动的证据

- 参考最终报告（本地记录：`prototypes/plugin-platform-v1/artifacts/runs/228cf431212f4d919ac754e89747e17e/verification.json`）。
- 参考用量（本地记录：`prototypes/plugin-platform-v1/artifacts/runs/228cf431212f4d919ac754e89747e17e/collect-usage-replay.json`）及配额（本地记录：`prototypes/plugin-platform-v1/artifacts/runs/228cf431212f4d919ac754e89747e17e/read-quota-happy.json`）。
- 新作者完整报告（本地记录：`prototypes/plugin-platform-v1/sandbox/audit-author-a0c5efc7/artifacts/runs/0582ce8331c0498e86e5a62c47dc2c6c/verification.json`）。
- 新作者用量（本地记录：`prototypes/plugin-platform-v1/sandbox/audit-author-a0c5efc7/artifacts/runs/0582ce8331c0498e86e5a62c47dc2c6c/audit-usage-replay.json`）及配额（本地记录：`prototypes/plugin-platform-v1/sandbox/audit-author-a0c5efc7/artifacts/runs/0582ce8331c0498e86e5a62c47dc2c6c/audit-quota-happy.json`）。
- 创建/修改/复验脚本（本地记录：`prototypes/plugin-platform-v1/artifacts/audit/scaffold-e2e.py`）；汇总副本（本地记录：`prototypes/plugin-platform-v1/artifacts/audit/scaffold-verification.json`）。
- 首轮六项失败负对照（本地记录：`prototypes/plugin-platform-v1/artifacts/audit/baseline-red.txt`）；编码绕过失败负对照（本地记录：`prototypes/plugin-platform-v1/artifacts/audit/encoding-red.txt`）。
- 失败不残留 passed 的实际进程报告（本地记录：`prototypes/plugin-platform-v1/artifacts/runs/f4041e3fecc64d629c3a2a8f70e8eb91/verification.json`）：首次误将任务时限用于整个进程而超时，记录 failed；后续修复为独立编译/组件时限，并保留失败历史。

每场景同目录有 JSONL，关联 trace/plugin/task/sequence 由验证器核对。参考和新作者项目各自在最终运行中安装随机新 ID 的元数据变体，并确认宿主文件哈希不变；该变体不冒充新供应商实现。

## 摘要与产物

| 字段 | 标准参考 | 新作者项目 |
| --- | --- | --- |
| 包名 | org.example.tokenme.reference_0.1.0.tokenme-plugin | org.example.audit.author-ide_0.1.0.tokenme-plugin |
| bytes | 245072 | 244877 |
| package SHA-256 | 10e29be5c03354c82405e77992702a352171f0345381acea84e963ef7c7624a4 | 7ca72260b3d032225e1ab0ee25e0e5fdb350878823cb59a0bd86e1072bd11e77 |
| source tree SHA-256 | cee1a7ca6b876059da5f491817b45c5cc9c320984882f5d567dd8fad5434e080 | de4a128179f983955373fcbed9f0b17b9c14e5b93978fe4bc4e05c8ae1cdb7f4 |
| host SHA-256 | 9c20cb1e2481536be8222392915a7b2283972b74bcc6ea62cb3f558425a46390 | cf57ee5d506a84216872d76d6096cbbd5caf05ad4cac9dd56de538eafac23253 |
| component SHA-256 | 77837dd6176cb2fb53234ed2ea69ca77c78827fe03bdbb8da9430e03e67f1e72 | 7caf13fc701f4275326c1d8501d9a9c3a3da1b60ee42ef6bf46f7cf70d484f18 |

Rust 为 1.98.1，Python 为 3.14.6；Wasmtime/WASI 49.0.0、wit-bindgen 0.63.0；Python 直接与传递依赖已固定并检查，完整版本见报告。签名状态均为 unsigned-local，仅安装参考开发沙箱。重复打包证明同一组件的归档字节稳定，未声明干净环境重建或发布者签名证明。

## 验收边界

mainAppIntegrated=false。参考账本仅单个合成工具/账号、进程内去重；不持久提交核心游标。配额/源数据来自 fixtures，没有真实网络、认证或第一方写入。资源包含每 memory 上限/fuel/epoch，但没有生产累计 Store/JIT/全局内存保证；便携安装器仍不保证抵御同用户恶意目录竞态。

未验证实际 TokenMe 的索引/缓存/设置/品牌/语言/通知/诊断界面、真实认证/签到/SQLite/估算/价格、升级迁移、CLI/collector、macOS/Linux 和真实跨设备。完整上线阻断项及 A01—A22 追踪见审计。以上耗时为整套开发复验，不是首屏/用户任务性能指标。

本页保留原审计运行的摘要；当时未提交、推送或发布。本次将设计和参考源码提交到 plug 分支；build/dist/sandbox/artifacts 运行记录仍保留本地，不包含在源码提交中。后续改动必须重新生成对应证据，不能把本页快照当作未来源码的通过记录。

## plug分支提交前复验（2026-10-20）

完整verify通过，包含Python回归、Rust原生测试、实际Wasm构建/运行、重复打包、隔离安装和未知插件ID。运行报告保留本地，可按参考工程命令重建；不计作生产TokenMe或Agent网关验收。

- runId: `c36af14e64964f029f3c69a6300a5e23`
- sourceTreeSha256: `0a76176b4a78029db4da7d00cfcbb2c0606c3bd49a08936aa0cbc6acbcea2cb9`
- hostSha256: `9c20cb1e2481536be8222392915a7b2283972b74bcc6ea62cb3f558425a46390`
- moduleSha256: `77837dd6176cb2fb53234ed2ea69ca77c78827fe03bdbb8da9430e03e67f1e72`
- packageSha256: `bec51fa3fd1552d0ecd567317c36cd2a18d3e9cf6360effa7a02f2e2ab8c69cd`
- packageBytes: `245072`
- elapsedSeconds: `121.555`
- wasmExecution: `True`
- mainAppIntegrated: `False`

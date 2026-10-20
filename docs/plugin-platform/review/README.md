# 设计合同的可执行反例

此目录属于设计合同审计，不是已发布的 TokenMe SDK/生产 schema。三个 schema 是发现 Scope、通知配置、UsageFact 的严格投影；verify_contracts.py 以独立数值期望验证关键语义。生产 F01 必须实现同样的 oracle，并扩展为完整合同，不能拿此处模型替代产品代码。

在隔离仓库根、使用参考工程已锁定的 Python/jsonschema 环境运行：

```powershell
python docs/plugin-platform/review/counterexamples.py --baseline docs/plugin-platform/review/fixtures/before --output prototypes/plugin-platform-v1/artifacts/audit/round2/counterexamples.json
python docs/plugin-platform/review/verify_contracts.py --output prototypes/plugin-platform-v1/artifacts/audit/round2/design-review.json
```

第一条命令核对修改前文件的具体引文/行号/SHA，再输出允许坏结果或无法确定行为的设计反例；它不把假设实现当成真实运行漏洞。第二条命令输出逐条合同场景和带文档/schema/模型摘要的结果，每次保留独立 runId 历史，最新结果开头即 running。

适用范围：继承的 false/0/空数组、发现无账号、修订/批次冲突、缺测与 pending、累计序列、配额健康/TTL/单位、冷却内严重升级、OS 回执不确定、独立配置版本、价格除零/大整数、迁移依赖与范围。测试输入全为匿名合成值，不访问真实账号、供应商、OS 通知或用户 profile。

模型使用校验后的内部表示；例如配额缓存按 window ID 映射，不是对外 windows 数组的完整解析器。各模型只验证所列语义，权限模型的 grant/平台实际执行仍须生产服务测试；禁止把某个纯函数拷贝成未校验完整输入的生产边界。

报告 production_runtime_validated=false 是强制边界。未覆盖的 native 服务、并发 crash injection、恶意 Component、生产安装和 UI/跨平台仍须执行原验收票。文档再变更后旧报告只对应其记录的摘要。

新增 Agent 网关只有设计草案和人工对抗验收输入，当前模型不覆盖其语义或真实客户端链路。报告包含该文档摘要仅表示关联版本，不能把文档被收录解释成网关验证通过；G01—G07 分别交付合同/实际运行证据。

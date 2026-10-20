# 适配器与策略迁移清册 0.5

日期：2026-10-20。基线：隔离分支 e1cf55d。此表是 F11.2 的范围清册，所有行均待迁移/验收，登记存在不表示插件已实现。F01 前比较最新主线后刷新，不从当前机器有没有登录推断产品范围。

来源：crates/adapters/all/src/lib.rs::TOOL_IDS、crates/usage-quota/src/providers/*::tool 与 lib.rs::CHECKIN_TOOLS 的并集。提取定义后核对 optional/built_in 注册；没有 SourceAdapter 的 quota-only 产品也必须迁移。

| 旧 tool ID | SourceAdapter | 配额 probe | 签到表 | 生产目标 / 状态 |
| --- | --- | --- | --- | --- |
| agnes | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| antigravity | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| antigravity_ide | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| atomcode | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| catpaw | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| claude | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| cline | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| codebuddy | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| codex | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| cola | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| copilot | 无 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| crow5 | 有 | 无 | 无 | 官方包保留 tool ID；F11.2 pending |
| dsh | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| funide | 有 | 有 | 有 | 官方包保留 tool ID；F11.2 pending |
| gemini | 无 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| hermes | 有 | 无 | 无 | 官方包保留 tool ID；F11.2 pending |
| joycode | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| kimicode | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| mimocode | 有 | 无 | 无 | 官方包保留 tool ID；F11.2 pending |
| minimaxcode | 有 | 有 | 有 | 官方包保留 tool ID；F11.2 pending |
| omp | 有 | 无 | 无 | 官方包保留 tool ID；F11.2 pending |
| opencode | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| pi | 有 | 无 | 无 | 官方包保留 tool ID；F11.2 pending |
| qoder | 有 | 有 | 有 | 官方包保留 tool ID；F11.2 pending |
| qoder_cn | 有 | 有 | 有 | 官方包保留 tool ID；F11.2 pending |
| trae | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| trae_cn | 有 | 有 | 有 | 官方包保留 tool ID；F11.2 pending |
| workbuddy | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |
| workbuddy_cn | 有 | 有 | 有 | 官方包保留 tool ID；F11.2 pending |
| zcode | 有 | 有 | 无 | 官方包保留 tool ID；F11.2 pending |

## 每行必须交付的证据

F11.2 开始时将每行展开为能力记录（而不是一个完成勾）：源码入口、plugin/tool ID、usage/quota/checkin/auth/estimate/pricing/telemetry/action/品牌/i18n/设置/通知的适用性，来源/账号/edition、平台和权限、游标/缓存/配置映射、固定输入与独立期望、最终组件/真实索引/迁移回退报告。无能力必须有旧实现依据，不能把空白当不适用。

国内/国际版本分别记录，不能合并 ID、credential root、账号、缓存、签到周期或价格。批准的历史缺陷修正单列差异及 oracle，不为了逐字对齐旧 bug 而复制错误；标准用量/报表守恒对账必须可解释。

## 工具行之外的强制策略范围

| 切面 | 原入口 / 范围 | 迁移边界 |
| --- | --- | --- |
| 原生 credits/价格策略 | usage-core/pricing.rs 的 CREDIT_USD、模型/供应商价格来源、单位映射 | 通用定点/查价/聚合机制留框架；工具/edition 特例和数据由 capability provider 提供 |
| 配额/周期 Token 估算 | usage-index/facts.rs 的工具判断与周期模型推算 | 策略移到插件，输出可追溯估算；公共总量不重复计入 |
| 主动/被动测速 | src-tauri/active_speed.rs 与 adapter telemetry 来源 | 协议、程序/endpoint/模型映射由插件；预算、取消、调度/授权留宿主，主动调用独立 action |
| 品牌/设置/通知/展示 | core/icons、format、ToolIcon、SettingsSheet、notify、bridge | 静态工具表退出新增入口；缺失插件历史仍可显示 |
| 动态 broker/第一方凭据维护 | WorkBuddy broker、SQLite/WAL、系统解密/轮换等既有路径 | 每能力单列授权/并发/异常恢复；不能只迁 HTTP quota 就删除旧链路 |

F11.1 只证明代表性路径，不关闭本清册。F11.2 全部适用能力通过后，F12 才可验收关闭 legacy bridge、移除逐工具 production path dependencies；不能用剩余旧适配器继续静态运行来替代插件迁移。

# 合成插件清单示例

本目录用于评审 `../PLAN.md` 的协议草案，所有字段尚待 P1 原型验证。

0.3 起，本目录仅保留早期产品示意。实际可编译、可验证、可打包的项目与规范源见 [实施规格](../IMPLEMENTATION_SPEC.md)及 [参考工程](../../../prototypes/plugin-platform-v1/README.md)，请勿混用两种清单格式。

- `manifest.json`：展示插件身份、运行入口、品牌、接口、任务、设置、视图和通知的注册方式。
- `i18n/en.json`、`i18n/zh-CN.json`：同一插件的语言键和占位符示例。
- `assets/icon.svg`：无脚本、无外部资源的静态示意图标。
- `diagnostic-event.json`：合成的超时诊断元数据，体现插件/版本/任务/trace 关联；没有真实请求、账号、路径或模块摘要。

示例工具名为 **Example IDE**。域名使用不可作为真实服务使用的 `example.invalid`，数据路径为合成路径；时区和签到周期同样只是示意，不代表 Devin 或任何供应商的真实合同。

**本目录没有 `dist/plugin.wasm` 或插件处理器，不是可安装、可执行的插件包。** JSON 可解析及引用自洽只证明文档样例的一致性，不代表宿主已经支持这些字段。首版 runtime 和 schema 确定后，应将通过完整验证的公开样例迁入 `spec/plugin/`。

清单采用这些草案约定：

- `manifestVersion` 与 `hostApi` 是插件协议版本，不是当前应用版本；不据此决定 TokenMe 发版版本号。
- `tools[].id` 延续工具身份；`id` 标识代码包，插件更新不会更换工具 ID。
- `labelKey`、`titleKey`、`descriptionKey`、`summaryKey` 为插件语言目录中的键。
- `messageKey` 连接诊断事件/错误目录与插件语言资源；事件名和错误码保持机器可检索。
- `enabledSetting`、`thresholdSetting` 等引用 `settings[].key`。
- `hostGates` 是宿主已支持的策略键，不允许任意表达式或脚本。
- `handler` 指向未来可执行模块中的处理器，由注册表验证；当前仅为契约示意。
- `sources` 和 `endpoints` 的描述由插件提供，访问必须同时在 `permissions` 内获准。
- `estimates.*` 绑定派生结果，与 `usage.actual.*` 的真实统计分开。
- `signature.json`、完整性清单、许可证和可执行模块在真正打包时生成；示例不伪造签名。
- `build` 字段中的 Rust/SDK 信息仅示意作者侧语言与构建元数据，不表示 SDK 已经发布或样例已经编译。
- 日志和语言路线细节见 [日志与诊断](../LOGGING_DIAGNOSTICS.md)、[开发语言与 SDK](../LANGUAGE_AND_SDK.md)。

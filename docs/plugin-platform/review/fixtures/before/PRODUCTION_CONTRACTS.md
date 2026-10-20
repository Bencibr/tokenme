# Archived design excerpts: PRODUCTION_CONTRACTS.md

Source document SHA-256: 43dce4843d936ff468fd734415a40881ba0fa5e3d7bd1dd57be77f56271ec971

Excerpts only; see ../source-mapping.json for original line numbers.

<!-- original line 11 -->
采用 JSON UTF-8、字段 snake_case、机器枚举固定英文、未知字段拒绝。声明式清单沿用当前 camelCase；两者通过生成类型映射，禁止隐式大小写转换。数组有上限，字符串按 UTF-8 字节计；字典 key 另行校验。所有整数在 `[0, 9007199254740991]`；时间为 UTC Unix ms，日界由独立的时区/周期字段决定。缺失和 `null` 只在声明为 optional 的字段允许；缺失数据不转为 0。浮点仅允许有限的比例/测量值，金额与原生 credits 使用十进制字符串。

<!-- original line 47 -->
| Scope | `tool_ref:Ref, account_ref:Ref, source_ref:Ref?, origin_device_ref:Ref`；来源属于该工具/账号，授权不因参数中出现 Ref 自动成立 |

<!-- original line 52 -->
| BatchHeader | `schema_version:Int, batch_id:Ref, source_revision:Ref, expected_cursor_revision:Int, completeness:complete|partial, next_cursor:Cursor?` |

<!-- original line 55 -->
| Decimal | 正则 `^(0|[1-9][0-9]{0,17})(\.[0-9]{1,12})?$`，非负，最多 18 位整数+12 位小数；整数运算/定点库实现，拒绝指数形式、逗号、NaN/Inf |

<!-- original line 61 -->
manifest 负责静态注册，不执行“注册脚本”。execute 仅处理已注册 task ID，保留的 `host:migrate` 是独立生命周期操作，普通任务 ID 不允许冒号。每次实例化禁止宿主 call/log 在 task context 未开启时调用；迁移仅具备私有副本 state.read/state.write，无网络、认证、文件或签到权限。context 的身份即使返回给插件，也不能被改写后拿到更高授权。

<!-- original line 78 -->
| ts_ms | 实际发生时间；不能使用扫描时间替代未知时间，未知时间的记录进入待补全区 |

<!-- original line 81 -->
| counts | `{input:Int, cache_creation:Int, cache_read:Int, output:Int, reasoning:Int}`；四阶段互斥；reasoning ≤ output，不额外相加 |

<!-- original line 85 -->
| revision | 该 occurrence 的单调修订 Int，旧版本不能覆盖新版本 |

<!-- original line 92 -->
cumulative 按 `(tool,account,source,epoch,session,model)` 的稳定序列排序求差，第一次为基线；只有存在明确 session 起点时第一次从 0 计数。单调增加产生差量，负差须有 reset/修正证据，否则隔离并报 `DATA.CUMULATIVE_RESET_UNKNOWN`。迟到/修正从受影响的相邻区间重算事实及 rollup；不直接对累计快照求和。映射到现有 `UsageEvent/TokenCounts/Semantics`，保留其模型 join、reasoning、credits 和同步 origin 语义。

<!-- original line 94 -->
`facts+contributions+cursor+source_revision+batch_receipt+report_dirty` 在同一 usage-index 事务提交；generation、settings/grants revision 和 source lease 的 fencing token 在事务内核验。失败整批回滚；complete 批次可推进 next_cursor，partial 只能推进已完整消费前缀且明确返回该前缀，不能跨过半条 JSONL/未完成 WAL 行。重试相同 batch_id 返回原 receipt。

<!-- original line 98 -->
`quota.read` 输入 `account_ref, previous_sample_ref:Ref?, window_ids:ID[]`；输出 `status:ok|unlimited|login_required|unavailable, sampled_at_ms:Int, valid_until_ms:Int, windows:QuotaWindow[]`。失败通过 PluginError 返回并保留缓存，不转成空窗口覆盖最后答案。

<!-- original line 102 -->
缓存键 `(tool,account,window)`，新样本按 sampled_at_ms+任务序号覆盖旧样本；晚到响应不回退数据。有效期过后标 stale；禁用保留最后缓存并显示 disabled 原因。今日总 Token 不叠加配额百分比。

<!-- original line 119 -->
| pricing.resolve | `model:string?, meter:tokens|credits, native_unit:ID?, at_ms:Int, currency:ISO4217` | `price_id:Ref, rates:[{stage:input|cache_creation|cache_read|output|native_credits, amount:Decimal, per_units:Decimal}], currency, effective_from_ms:Int, effective_to_ms:Int?, source_kind:official|user|derived, source_ref:Ref, version:string` |

<!-- original line 152 -->
credential 文件/DB/注册表来源列为 secret resource，不能通过 source.read 绕过认证服务。默认宿主提取、DPAPI/钥匙串解密、注入已绑定 endpoint；刷新响应的 secret selectors 由宿主捕获并从交付给 guest 的正文中移除。确需把明文或第一方凭据写权限授予插件，必须是单独的高权限 profile 和再次授权，v1 普通第三方 profile 不支持；不默默退回继承用户环境。第一方写回遵循插件声明的锁协议、revision CAS、原子替换和 intent journal，恢复时检测外部客户端已更新而不覆盖它。

<!-- original line 166 -->
任务状态为 queued/running/succeeded/failed/gated/cancel_requested/cancelled/outcome_unknown。每次触发先落 invocation+lease，租约带单调 fencing token，正常每 5 秒续租、15 秒失活（写任务未决状态不因失活清除）。接收事务验证 token，过期 runner 返回的数据不入库。所有设置/grants/generation 改变使旧结果失效；已发出的写请求进入 reconcile，不假装已撤销远端操作。

<!-- original line 182 -->
宿主 Gate 先更新观察状态，再决定投递；全局静音/单插件关闭/系统拒绝都不补发历史层级变化。冷却默认 30 分钟，队列最多 1000、去重保留 7 日；离线积压只保留最新有意义状态，单条最多延迟 5 分钟。系统投递失败记录 reason，可显示在应用内状态；点击 action 再核验插件版本、参数、grants 和当前设置，不能保存一条可绕过权限的旧调用。

<!-- original line 197 -->
quota_percent_low 的统一字段为 `{enabled:bool, thresholds_remaining_percent:number[], cooldown_ms:Int, channels:Channel[]}`；阈值为 0..100 的有限数、唯一、降序、最多 8 项，空数组表示无阈值提醒。全局初始模板为 enabled=true、阈值 [20,5]、cooldown_ms=1800000；channels 初始 [system]，只可选择平台已实现且授权的投递方式。此为新生产功能的初始模板，迁移旧用户时优先保留其已有设置，不重置。

<!-- original line 201 -->
修改全局默认立即影响继承字段，保留应用的显式覆盖；保存只增加设置 revision、发布配置事件和重建内存有效策略，不强制刷新配额、不清缓存、不执行签到。对已有配额按新配置更新“已观察阈值”基线而不投递；无缓存时下一个有效样本先建立该次配置变更基线。切换模式、开启提醒、修改阈值或取消静音都不补发旧告警。排队中的通知在投递前按最新有效配置重验，旧配置通知不绕过新关闭状态。

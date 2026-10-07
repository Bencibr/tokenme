# TokenMe 引擎性能优化路线图 (Internal Perf Roadmap)

> 本文档归档引擎 CPU 占用的实测基线、已完成的两轮修复（2026-09-30）与尚未实施的结构级优化项。
> 供后续排期参考；数字均在 M 系列本机、有活跃 CLI 写入的负载下测得，换机器需重测基线。

---

## 0. 背景：一轮修复后的成本模型

引擎是单线程循环（`apps/tokenme-bar/src-tauri/src/engine.rs`）：watcher 唤醒/30s cadence timer → `ingest()` → 有新事件时重建完整报告并发布。

每轮 `ingest` 的三段成本（2026-09-30 实测，本机索引 ~37 万事件、2797 个受管文件）：

| 阶段 | 内容 | 单次成本 | 频率（修复后） |
| :--- | :--- | :--- | :--- |
| discover | 每个适配器重新枚举数据源：codex 保留窗内 872 个 rollout 的 readdir+fstatat+全路径排序、qoder 353 文件 walkdir、opencode 族 stat | 76–430 ms | 每 3 s（`MIN_PASS_GAP` 下限，ZCode CLI 每秒写遥测库触发唤醒） |
| read | 各适配器增量读（游标之后的新行） | 数 ms | 仅文件变化时 |
| publish | `Index::all_events()` 把**全部索引史**拉成 `Vec<UsageEvent>`（每行多次 String 分配），`summarize` 单线程聚合，逐事件 `price_for`（内部 `normalize_key` 每次分配） | 500 ms–1 s | 至多每 10 s（`PUBLISH_GAP` 节流）或 30 s timer |

已完成的修复（commit 8a58aa1 / 1039bb6 / 8db1f39）：opencode 梯子 rung 缓存（原每轮对 2.5 GB crow5.db 全表 `count(*)`，是 40–90% 尖峰的真身）、quota pass 60s 缓存、无新事件不发布、轮次 3s 下限、发布 10s 节流、audit 后置。

**修复前后**（90 s 连续 `top` 采样，活跃写入负载）：

```
修复前   avg 18.6%   max 90.6%   >12% 样本 40/180
修复后   avg  8.5%   max  8.4%   >12% 样本  0/180
```

剩余 ~8% 的构成：discover 均摊 ~3–5% + 发布重建均摊 ~5%。本文档的优化项就是把这两段继续压下去。

---

## 1. 【P1】发布聚合下推 SQL —— 消灭全量 rebuild

### 现状与问题

`Report` 需要：day/week/month/year 四个 `Window`、53 周 heatmap、今日 24 小时 hourly、top-20 recent_sessions、all_time。`usage-core/report.rs::summarize` 的输入是 `&[UsageEvent]`，于是每次发布都要把 37 万行完整事件（含 tool/session/model/source/project/dedupe_key 字符串）从 SQLite 搬进 Rust 再逐条聚合计价。这占总 CPU 的 ~5%，且随历史线性增长。

### 方案

在 `usage-index` 新增聚合查询层（如 `Index::report_aggregates()`），把分组下推给 SQLite：

- **一组 GROUP BY 查询**按 `(period_bucket, tool, model_id)` 返回 `sum(input/cache_read/cache_creation/output/reasoning)`、`count(*)`、`min/max(ts_ms)`。period_bucket 在 SQL 侧用 `date(ts_ms/1000,'unixepoch','localtime')` 派生日桶，Rust 侧再把日桶折进 day/week/month/year/heatmap/hourly 边界——边界判定仍归 Rust（DST、周界语义与 chrono 一致），SQL 只做朴素日桶。
- **计价从逐事件变逐组**：`price_for` 的调用次数从 37 万次降到 distinct(tool, model) 数量级（本机 ~40）。钱 = 组内 token 合计 × 单价，与逐事件累加在数学上恒等。
- **recent_sessions**：SQL 先取 `max(ts_ms)` 最新的 20 个 session id，再只对这 20 个做一次分组查询。
- **all_time**：同一条不分组查询。
- `summarize(&[UsageEvent], ...)` 契约保留给 CLI 的 `--from-json` / 测试路径；面板引擎换走聚合入口。report 结构体本身不动，前端零改动。

### 必须守住的语义（验收即测试）

1. **Golden 对拍测试**：同一批事件（fixture 覆盖 DST 切换日、周/月/年边界、reasoning 折叠、credits 事件、零值行），新旧两条路径产出逐字段相等的 `Report`。这是本项的主要工程量所在。
2. `sqlite3` 的 `localtime` 修饰符与 chrono `Local` 在时区规则上需抽样对齐（至少验证本机时区的 DST 两个切换点）；若发现偏差，退回"SQL 只出裸 `(ts_ms, tool, model, tokens)` 聚合列、Rust 扫聚合列分桶"的中间形态——仍然省掉全部字符串分配与逐事件计价。
3. 去重契约不碰：聚合读的是索引终态，`首条生效/终态发事件` 的约束原样成立。

### 预期收益

发布重建 500 ms–1 s → 预计 < 50 ms；引擎 CPU 再降 ~5%；成本从 O(全史) 变 O(增量分组数)。

---

## 2. 【P2】discover 定域 —— 别为一棵树的写动全森林

### 现状与问题

任何被watch 根目录的一次写事件，都会让**所有**适配器重新 discover 一遍。本机空转轮 76–430 ms（430 ms 的尾部是 APFS stat 抖动），均摊 ~3–5%。

### 方案（两档，可递进）

- **A（引擎层，不动适配器）**：watcher 事件携带根目录 → `ingest` 只对根命中的适配器做 discover，其余适配器沿用上一次的 `Vec<SourceFile>` 快照，仅对快照逐文件 `fstatat` 刷新 size/mtime（去掉了 readdir/排序/PathBuf 分配，保留 N 次 stat）。快照按适配器存引擎侧，TTL（如 30 s）兜底强制全量重走，保证新文件最终可见。
- **B（适配器层）**：`SourceAdapter` 增加 `discover_cached` 默认方法做同样的事，codex/qoder 这类大目录适配器内建日期分区缓存（`sessions/YYYY/MM/DD/` 已是现成的分区键）。

### 必须守住的语义

1. `unchanged_since` 的输入必须是**新鲜 stat**，不能用快照里的旧 size/mtime——否则漏更新（这是本项唯一的正确性红线）。
2. 新文件可见延迟 ≤ TTL（30 s），且 cadence timer 轮强制全量 discover 兜底。
3. 不涉及 read 路径与去重契约。

### 预期收益

空转轮 76–430 ms → 预计 10–30 ms；引擎 CPU 再降 ~3%。

---

## 3. 【P3】小项（随手可做）

- **`price_for` 归一化 memoize**：`normalize_key` 每次调用分配 String，37 万次/重建。若 P1 未实施，可先给 `PricingMap` 加 `Mutex<HashMap<String, Option<Price>>>` 缓存 distinct model（~几十个），省重建成本 ~10–15%；P1 落地后此项自然消亡。
- **`prune` 内的 `PRAGMA optimize` 节流**：目前每轮 ingest 都跑；SQLite 自己有 10 分钟节流，但可以显式只在 cadence timer 轮跑，省一次每轮的库级检查。
- **空转轮日志降噪**：`scanned N changed 0 new 0` 每轮一行，静默机器上一小时 1200 行；可改为状态变化时记录或 cadence 轮才记录。

---

## 4. 排期建议与度量口径

| 期 | 内容 | 预期效果（活跃写入负载） |
| :--- | :--- | :--- |
| 已完成（2026-09-30） | rung 缓存 + 节流三件套 | avg 18.6% → 8.5%，max 90.6% → 8.4% |
| P1 | SQL 聚合 + golden 对拍 | avg → ~3–4%，历史增长不再推高 CPU |
| P2 | discover 定域 | avg → ~2%，空转轮 < 30 ms |
| P3 | 小项 | 收尾 |

**验收口径**（与本文档基线同法测）：90 s 连续 `top -l 90 -s 1 -pid <PID> -stats pid,cpu` 取 avg/max/分布；panel.log 核对轮次与耗时；发布延迟 P95 ≤ `PUBLISH_GAP`(10 s) 不放宽；`cargo test` 全绿含新增 golden 对拍。

**非目标**：不改索引 schema 与去重契约；不改适配器 read 语义；不动前端。

---

## 5. 落地记录（2026-10-01）

三项全部落地，验收达标。**P1 最终形态不是字面的 SQL GROUP BY 下推**：SQLite 没有哈希聚合，每个 GROUP BY 都要排序，宽字符串键的 sorter ≈ 第二次全量物化（本机实测纯 SQL 全扫分组 526–2303 ms，裸全表扫描的物理下限也要 135 ms）——任何全扫形态都到不了 ≤50 ms。落地为 §1 预留"中间形态"的增量终态：

- **增量日聚合（`event_rollup` 表）**：`persist()` 对每个非重复事件做一次 upsert（µs 级），按本地日（与 report 同一个 chrono `local_day_of`，SQL 零时区数学——比 §1 的 `date(...,'localtime')` 草案更强，DST 与边界由构造保证一致）+ (tool, session, project, model, meter) 分桶累加。purge/prune/clear 走受影响日的定向重算（>400 天跨度直接清空待懒重建）。表是 additive + derived + 自愈的，**SCHEMA_VERSION 不动**。本机 388k 事件的 rollup 基数只有 2,234 行。
- **发布读路径**：全量读 rollup（~2.2k 行）+ ≤5 个 live 切片（今日 + 四个 prev 头 + 24 小时槽，VALUES-join）+ calls 首次 CTE + quota 过期谓词下推 SQL（199k 行 → 数百行）+ top-N sessions（`event(tool,session)` 索引上的相关子查询取首 project/末 model）。`summarize_facts` 与旧 `summarize` 共享抽出的 `merge_quotas`；计价按 (tool, model, meter) 组 memoize——§3 的 price_for memoize 由此自然消亡。
- **Golden 对拍 8 条**：双路径（mock-adapter 走真实 persist 增量 + 裸 INSERT 走懒重建，两次读取全等）、purge/prune/clear 生命周期、1e-9 相对容差仅限 f64（其余逐字段全等含顺序）。NULL project/model 以 `''` 哨兵入桶（真实数据零 `''` 出现，fold 回 None）。

**P2 落地为方案 B + 引擎按根路由**：`SourceAdapter::discover_cached` 默认方法（18 个适配器零改动）、`SourceFile::restat`（新鲜 stat + WAL mtime 折叠，守住红线 1）、watcher 载荷携带声明的根；引擎对被唤醒的工具走树、其余适配器 restat 快照，TTL 30 s + cadence 轮强制全量兜底（红线 2）。集成时踩过一个真坑：wake 载荷本来就是声明的根，"根命中 → 全量"的整轮条件恒真——定域必须**按 adapter** 决策，不是按整轮。

**P3**：`PRAGMA optimize` 从每轮 `prune` 拆出为 `Index::optimize()`，只在 cadence 轮调用；空转轮（changed 0 new 0 的 file change）不再写 panel.log。

**度量（口径同 §4，2026-10-01 凌晨，活跃写入负载下两次独立 90 s 采样）**：

| | 改造前 | 改造后 |
| :--- | :--- | :--- |
| 90 s `top -l 90` avg | 8.5% | **2.39% / 2.07%**（两次采样） |
| 分布 | 常驻 ~8% | p50 0.4%，p90 6.8%，尖峰集中在 60 s 一次的 quota 轮秒 |
| initial scan | 秒级 | 40 ms（boot 发布含一次性 rollup 重建 <1 s） |
| 空转轮 | 76–430 ms | 40–60 ms（restat ~2.8k 文件的 stat 成本高于 §2 预估的 10–30 ms，但 duty cycle 达标） |

`cargo test --workspace` 全绿（含 8 条 golden）；非目标未触碰（schema 仅 additive）。升级后第一次发布会先做一次全量 rollup 重建（本机 <1 s，panel.log 有 `rebuilt the day rollup` 标记行）。

# TokenMe Linux 采集同步方案 (Internal Linux Collect → Mac/Windows Display Plan)

> 本文档归档"Linux 机器做采集、macOS/Windows 面板做显示"的落地方案（2026-10-05 定稿）。
> 核心机制是**索引事件流同步**：不走日志镜像，把 Linux 端的索引导出成 JSONL 事件流，
> 显示机幂等合并进自己的索引，面板与前端零改动。
> 状态：方案定稿，经**两轮对抗审计**（合并语义 §10-1；通信/安装/认证 §10-2）。
> 第二轮把实施主体从 Python 脚本改为 Rust 子命令 + 面板引擎自动导入（F1-F5，见 §10-2），
> 实施进行中；实施后在本文件末尾追加落地记录（口径见 §11）。

---

## 1. 结论

```
Linux：tokenme CLI 维护本地索引 → tokenme export 导出窗口事件流（JSONL.gz + manifest）
      → 任意文件通道（scp/rsync/共享盘）→ 显示机：幂等 import 合并进本机 index.db
```

- Linux 端跑 CLI（它本来就是 Linux 交付物，无 GUI 依赖），采集是这份索引；
  安装 = `install-linux.sh` 一条命令（二进制入 `~/.local/bin`，可选定时 + 推送一次接好）。
- 显示端 Mac/Windows **零安装零配置**：面板引擎每轮自动导入 `~/tokenme-sync`（§5.5），
  手动 `tokenme import` 只留给 CI/一次性场景。扇出、互相独立、可重复。
- **不做日志镜像**（理由见 §7）；**不改 schema、不改面板前端既有结构、不引入常驻服务**。

## 2. 现状事实（设计依据）

| 事实 | 证据 |
| :--- | :--- |
| CLI 无 GUI 依赖，自述为 "the Linux deliverable"；报告命令会先自动刷索引 | `crates/usage-cli/src/main.rs:3,67-73` |
| 索引是单文件 SQLite，面板与 CLI 共用；Linux 上是 `~/.local/share/tokenme/index.db` | `crates/usage-index/src/lib.rs:291-293` |
| `--db <PATH>` 可指向任意索引文件；`--no-ingest` 可从现有索引直接出报告 | `crates/usage-cli/src/args.rs:51-57` |
| `event` 有 `dedupe_key` 部分唯一索引 → 键控行 `INSERT OR IGNORE` 天然幂等；Codex 行无键，靠 file_state 字节游标幂等 | `crates/usage-index/src/lib.rs:69-77` |
| 键控行"追更"语义：同键新总量 > 旧总量时整行覆盖（ts/counts/source 全量更新；精确谓词见 §5.2-2） | `crates/usage-index/src/ingest.rs:640-697` |
| 报表主体读 `event_rollup`；今日/昨日/小时桶等 live 切片与 calls 直接读 `event`；rollup 懒重建**仅当它全空**时触发 | `crates/usage-index/src/facts.rs:58-79,175-226` |
| `source` 列是文件键；文件收缩重写时 `purge_source` 按它整行删除 | `crates/usage-index/src/ingest.rs:816-851` |
| CLI 发布矩阵无 Linux target（GitHub CI；Gitea 无 runner） | `.github/workflows/release.yml:114-119`、`AGENTS.md` |
| 平台相关代码全部 cfg 门控，非 macOS/Windows 有显式分支 → Linux 可编译（待实机验证） | `crates/usage-quota/src/providers/qoder.rs:92,316`、`…/zcode.rs:234,624`、`…/claude.rs:137`、`crates/usage-quota/Cargo.toml:24` |

## 3. 数据流

```
Linux（install-linux.sh 一次装好：二进制 + 可选 systemd --user timer）:
  每 15min：tokenme index                # 增量刷索引（另有 --status/--prune/--rebuild/--force）
            tokenme export --days 30     # 导出 ~/tokenme-sync/tokenme-<host>.jsonl.gz + manifest.json
  推送：--push user@host 预置的 ssh 通道（scp）；不直连就借共享盘/Syncthing（契约不变，通道无关）
显示机（mac/Windows，零安装）:
  面板引擎每轮自动导入 ~/tokenme-sync/*.jsonl.gz（sha256 未变即跳过；§5.5）
  手动/CI 一次性：tokenme import <file.gz> [--dry-run]
面板：既有结构零改动，自动显示合并后数据 + 同步健康度徽标（§9.1）
```

文件契约 = `tokenme-<host>.jsonl.gz` + `tokenme-<host>.manifest.json`（同名前缀，支持多台 Linux），传输通道无关（直连不通就借任何共享存储，契约不变）。

默认索引路径（`dirs::data_dir()/tokenme/index.db`，`crates/usage-index/src/lib.rs:291-293`）：

| 机器 | 路径 |
| :--- | :--- |
| macOS 显示机 | `~/Library/Application Support/tokenme/index.db` |
| Linux 采集机 | `~/.local/share/tokenme/index.db` |
| Windows 显示机 | `%APPDATA%\tokenme\index.db` |

导出/导入收进 `crates/usage-index/src/sync.rs`，CLI 只做参数与输出——不复用 `INSERT_EVENT`（它对冲突只 IGNORE，喂不进"追更"语义）而是沿用 §5.2 的精确语句。面板与 `tokenme index --status` 的来源列表会把"有事件但未检出"的来源照列（`crates/usage-index/src/lib.rs:708-725`），导入行不会因为显示机没装那个工具而消失。

## 4. 阶段 0：Linux 侧能跑 tokenme（安装 = 一条命令）

面向用户的安装路径（发布产物，见 §9）：

```sh
tar xzf tokenme-cli-<ver>-x86_64-linux-musl.tar.gz
./install-linux.sh                 # 二进制 → ~/.local/bin/tokenme
./install-linux.sh --every 15 --push user@display-host   # 顺带装好定时导出 + scp 推送
./install-linux.sh --uninstall     # 全部撤销（二进制/timer/单元文件）
```

构建产物为 x86_64/aarch64 musl 全静态二进制（`scripts/build-linux.sh`，cargo zigbuild；本机工具链已就绪），无 libc/glibc 依赖，scp 即用——**Linux 采集机不需要装 Rust、不需要装 Python**。

装好后验收三连：`tokenme detect` 列出该机工具根 → `tokenme index` 建成索引 → `tokenme daily` 出数。注意报告命令默认先自动刷索引，且"一个工具都没检出 + 索引为空"时会**报错退出**（`crates/usage-cli/src/context.rs:217-222`）——这正是阶段 0 想要的红灯，不要绕过它。

**这一步过不去，后续全部无意义**——先确认那台机器真的在产出可识别的日志。

## 5. 实施主体：Rust `export`/`import` 子命令 + 自动导入

第二轮审计 F2 否掉了原"Python 脚本先行的阶段 1/2 分期"：显示机装 Python + 定时任务违背"用户最小化操作"，而脚本对账/upsert 的正确性完全没有复用仓库里已实测的 SQL。**一次到位，进仓库**：

| 文件 | 职责 |
| :--- | :--- |
| `crates/usage-index/src/sync.rs` | 导出/导入全部逻辑（§5.1/§5.2/§5.3），CLI 只做参数与渲染 |
| `crates/usage-cli` 子命令 | `tokenme export --days <n> [--out <dir>] [--origin <name>]`、`tokenme import <file.gz> [--dry-run] [--json]` |
| `apps/tokenme-bar` 引擎 | 每轮自动导入 `~/tokenme-sync`（§5.5）——显示机零安装的核心 |
| `scripts/build-linux.sh` / `install-linux.sh` | Linux 产物构建 + 一条命令安装（§4） |

### 5.1 导出契约

- 只读查询 `index.db`（同用户；WAL 下与 ingest 并发安全）；**整个导出在单个只读事务里完成**，每行的 calls 与行本身来自同一一致性快照。
- 窗口默认 30 天，按**事件 ts** 过滤 `[lo,hi)`：迟到的旧 ts 行落在窗外不会同步——首次全量用 `--days 400`（对齐保留期）补齐，此后重叠导出无害、幂等兜底。
- 每行 = event 全字段（`tool, ts_ms, session, project, model, meter, in/cc/cr/out/reason, credits, dedupe_key, source`）+ 该事件的 calls（kind/name）；**quota 不带**（见 §6）。
- **键合成**：无 `dedupe_key` 的行（Codex）合成 `mix:<origin>:sha1(内容字段)`；批内同内容重复按 id 序取序号——**首个不加后缀，第 2 个起为 `#2`、`#3`…**；键只由内容与出现序号决定，重导出/重导入键集合稳定，真重复行不丢（不折叠）。**内容字段集与序列化方式是冻结契约**（改了就是键漂移，老行会被重复计一遍）。
- manifest（`tokenme-<host>.manifest.json`）：`origin`(hostname，可被 `--origin` 覆盖并固定)、源索引 `schema_version`、格式版本 `format`、窗口 `[lo,hi)` ms、行数、per-tool sums、sha256、生成时间。**导入前校验 sha256 与解压完整性，损坏/截断直接拒导**（fail loud，绝不做半个合并）。
- 落盘顺序：gz 先写 `*.tmp` 再 rename，manifest 最后写——通道上永远看不到"半个导出"。

### 5.2 合并语义（正确性核心，§8 的坑逐条在此防）

1. **source 命名空间**：写入前改写成 `linux:<origin>:<原路径>` —— 本机永远不会 discover 到同键文件，收缩 purge 不会误删（`crates/usage-index/src/ingest.rs:816`）。
2. **键控行 = 逐字移植 persist 的"追更"谓词**：同键且
   `(in+cc+cr+out)_新 > (in+cc+cr+out+credits)_旧`
   时整行覆盖（ts/session/model/counts/source 全量），否则视为 dedupe 忽略（`ingest.rs:640-643`）。两侧**不对称**是冻结语义：新侧是 `TokenCounts::total()`（`crates/usage-core/src/types.rs:78-80`，不含 reason/credits），旧侧含 credits——**不要**简写成"新 total > 旧 total"。SQLite 写法：`ON CONFLICT(dedupe_key) WHERE dedupe_key IS NOT NULL DO UPDATE SET … WHERE <上式>`（两侧列都套 `COALESCE(…,0)`）；**合成键行** plain `INSERT OR IGNORE`。
3. **rollup 修复**：收集本批插入+更新行的本地日（含更新前的旧日）→ `DELETE` + 按 `recompute_day_tx` 的 SQL 重算（`ingest.rs:69-88`）；日边界由调用方算好传入（`local_day_bounds`，chrono Local，`ingest.rs:94-103`），SQL 永不自己推导本地日；解析不出本地午夜的（DST 空洞）按 ingest 同款兜底清空 rollup 交给懒重建。不修则周/月/年/热力图/最近会话看不到导入数据——懒重建只在 rollup 全空时触发（`crates/usage-index/src/facts.rs:70-72`），**非空但缺行不会自愈**。受影响 >400 天（首次全量）直接清空 rollup 交给懒重建。
4. **并发与原子性**：`BEGIN IMMEDIATE` + `busy_timeout=5000`（写锁被面板/引擎占着时退避重试）；event 写入、call 补缺、rollup 修复、`meta` 记录（§9.1）**同一事务**，末尾一次 COMMIT——中途失败整批回滚，读侧只可能看到旧或新，绝不看到半个合并（实验台已实测）。
5. **call 行 = 并集补缺**：只对本批"插入或生长更新"的行，按 dedupe_key 反查 id，用 `INSERT … SELECT … WHERE NOT EXISTS(同 event_id+kind+name)` 逐条补缺——重复导入、重复生长都不产生重复行，也**绝不 DELETE**。上游 persist 在生长分支是纯追加（`ingest.rs:661-663`；`call` 表无唯一约束，`lib.rs:78-82`），照抄会在重复生长时累积重复行——有意不移植这一处。被忽略（dedupe）的行不动 call，与上游一致（`ingest.rs:694-696`）。
6. **schema_version / format 不匹配拒绝导入**（fail loud，不猜）。

### 5.3 对账门禁（按键对账；F1 修复）

**原稿的"窗口内 `source LIKE` 汇总 == manifest sums"是对账设计缺陷（F1）**：Linux 端重写日志会触发 purge，而显示机按"只增不删"永远留着旧键行——窗口 sums 从此恒大于 manifest，导入**每次都 ROLLBACK，永久卡死且无法自愈**。对账必须对准"本批导出的键集合"，而不是显示机现状：

1. manifest 携带逐键清单（`key → counts` 与 per-tool sums）；import 把清单一并解出，装进临时表并 JOIN `event.dedupe_key`：
   - **存在性**：命中的清单键数 `== manifest.rows`（少一个 → 有键没写进去，ROLLBACK）；
   - **数值**：只对命中的键取 `SUM(in/cc/cr/out/reason/credits)`，必须与 manifest 的逐键值逐列相等（UNION 对拍，任何不等 → ROLLBACK）；
2. 窗口内"有 event 行但不在本批清单里"的键 = **陈旧行**（Linux 端已 purge/重写）：只计数并在输出里提示 `stale N rows (kept)`，**不阻塞**——它们正是"只增不删"的既定语义；
3. 幂等复跑：第二次 import 全部命中且谓词不触发 → 0 新增 0 更新 0 call，仍须通过对账；
4. 显示机 `tokenme daily --no-ingest` 的增量 == manifest 窗口总量（绝对 ms 窗口，不受时区影响）；
5. 逐日数字只在**两机同一时区**时可比：Linux 若跑 UTC，要么给采集进程挂显示机时区（install 脚本写 `TZ=`，systemd `Environment=TZ=Asia/Shanghai`），要么只对窗口总量；同 TZ 下 Linux `tokenme daily` 同窗口 == 显示机对应数字；
6. 面板实查：今日数字、热力图、最近会话能看到 Linux 会话（live 切片直接读 `event`，导入后立即可见）。
7. 预演可全程在本机做：对索引副本用 `--db` 指 scratch 库，自导出自导入，先证明幂等与 rollup 修复，再上真机。

对账不过 → 整批 ROLLBACK（§5.2-4），`import` 以非零退出并输出哪一条断言失败。

### 5.4 认证与威胁模型（第二轮审计补全）

**结论：不引入应用层账号体系——信任锚是传输通道本身，导入侧只做数据面收敛。**

| 问题 | 设计 |
| :--- | :--- |
| 通道认证 | 导出物走**已认证的私有通道**：ssh/scp（现有密钥；`--push user@host` 直接复用 `~/.ssh`）、Syncthing（设备 ID 配对）、或私有共享盘（ACL）。不设计 token/密码/账号：tokenme 从没有服务端，凭空加一套只是自造攻击面 |
| 完整性 vs 真实性 | manifest 的 sha256 **只防损坏/截断，不防伪造**——能写入通道的人就能重签 manifest。这是有意的非目标：通道已认证，伪造者必须先攻破 ssh/Syncthing |
| 导入侧最坏情况 | 坏数据 → 数字污染，而不是代码执行：SQL 全参数化，只写 `event/call/rollup/meta` 四张表。恢复：`DELETE FROM event WHERE source LIKE 'linux:<origin>:%'` + `DELETE` 对应 meta 行（rollup 按 `recompute_days_tx` 重算或整体 rebuild），重新导入 |
| 凭据边界 | **凭据绝不随导出走**：manifest 不含 token/keychain 内容；quota 不进导出（§6）。显示机 live 探针仍用本机凭据 |
| 自导入 | manifest `origin == 本机 hostname` 时打印警告（生产流程是跨机）；演练用 scratch 副本 |
| 解压炸弹 | gz 解压设行数上限（manifest.rows + 余量），超限拒收——导出物是本工具自己产出的格式，防御只针对损坏而非恶意构造 |

### 5.5 展示端自动导入（F2：显示机零安装）

面板引擎的 `ingest()` 每轮在 `prune/optimize` 之后扫描 `~/tokenme-sync/`（目录不存在 = 零开销静默跳过）：

- 对每个 `*.jsonl.gz`：先算 sha256，与 `meta["sync:file:<文件名>"]` 记录相同 → 跳过（省掉解压与全量对账）；不同 → 走与 `tokenme import` 完全相同的路径（同一 `sync.rs` 函数），成功后在同一事务里覆盖该 memo；
- 失败（sha 不符/对账不过/解压失败）：**保持旧数据继续服务**，错误写引擎日志，文件留在目录下一轮重试——同步断流只体现为 §9.1 徽标变旧，绝不让面板崩或吞掉本机数据；
- 导入结果计入本轮发布：自动导入放在 `new_events == 0` 早退判定**之前**，且"本批导入非空"本身就是发布理由（否则 file-change 早退会把刚合并的数字吞掉）；
- 面板无新增设置项：目录固定 `~/tokenme-sync`，用手动 `tokenme import` 的用户不受影响。

## 6. 边界与非目标

- **非目标**：不改索引 schema（`event` 是冻结表）；不改面板前端；不动适配器 read 语义；不引入常驻服务。
- **quota 不跨机**：探针凭据在各自机器，显示机 live 探针不受影响；Linux 的用量数字会并入显示。
- **默认合并不区分机器**：`tool` 保持原值（定价/报表全兼容），origin 保留在 `source` 前缀里——将来要"只看 Linux"再加筛选，不需要迁移数据。
- **显示机 `tokenme index --rebuild` 会清空导入行**：重建后重跑一次 import 即恢复（幂等，代价为零）。
- **保留期两边一致**（400 天）：prune 会同步裁掉导入的旧行。
- **只增不删**：导入从不删除行——Linux 端 prune/rebuild 后消失的键不会在显示机被删（显示机自己的 400 天 prune 按同样规则裁）。要严格镜像需显示机也 rebuild，非目标。
- **升级顺序**：schema_version 升级后，旧导入脚本会拒收新导出（fail loud）——先升级显示机、再升级 Linux CLI；反向只断流，不毁数据。

## 7. 备选方案 B（不推荐长期使用）：日志镜像

rsync 厂商日志到显示机 + 适配器 env 覆盖（claude 有 `CONFIG_DIR_ENV`、`KIMI_CODE_HOME`、`MINIMAX_DATA_DIR` 等），让本地适配器直接读。缺点：Codex 日志 GB 级带宽、只有部分工具有路径覆盖、项目路径语义漂移、file_state 按镜像路径重建。仅当阶段 0 受阻时当临时桥。

（另：直接合并两台机器的 `index.db` 被否决——file_state/rollup/purge 语义交叠、schema 版本各自演进。）

## 8. 已知坑（实现时逐条防）

| 坑 | 防法 |
| :--- | :--- |
| Codex 行无键，导入端没有游标 → 重复导入翻倍 | 合成键（内容 hash + 批内去重序号；首个无后缀，第 2 个起 `#2`） |
| `source` 不命名空间 → 本机文件收缩 purge 误删导入行 | `linux:<origin>:` 前缀 |
| rollup 非空但缺导入天 → 不会自愈 | 导入主动重算受影响天（含更新前旧日） |
| 生长谓词新旧两侧不对称（旧含 credits、新不含） | 逐字照抄 `ingest.rs:640-641` 的表达式，别用"新 total > 旧 total" |
| `call` 上游生长分支纯追加（表无唯一约束）→ 照抄累积重复 | 导入用 `NOT EXISTS` 并集补缺；绝不 DELETE |
| counts 持平但 call 变化不追更 | 与上游同语义（上游也只在插入/生长分支写 call），接受 |
| 服务器 UTC、显示机 UTC+8 → 逐日数字对不上 | 逐日只在同 TZ 可比；给采集进程挂 `TZ=` 或对账只对绝对 ms 窗口 |
| 首次全量导入跨几百天 | export 默认 30 天窗口；首次 `--days 400` 全量，走"清空 rollup 懒重建" |
| export 与 ingest 并发 | 只读单事务 + 幂等，重叠导出无害 |
| 导入机 SQLite < 3.24（部分索引冲突目标不支持） | 版本检查直接拒导；两机现在都是 rusqlite bundled（同一版本，天然满足） |
| 对本机索引自导出自导入（测试时容易干）→ 无键行没有可命中的键，重复计入 | import 检测 manifest `origin` == 本机 hostname 时打印警告（生产流程是跨机）；别拿真索引做自导入演练，用 scratch 副本 |
| **F1**：Linux 端重写日志 purge → 显示机旧键行恒在（只增不删）→ "窗口 sums == manifest"永远失败、每次 ROLLBACK、永久卡死 | 对账改按键对账：存在性 + 按键 sums 全等为准；窗口内旧键只报 `stale N rows (kept)` 不阻塞（§5.3） |
| hostname 改名/重装 → 同一台机产生两个 origin，旧 origin 的行不再更新 | manifest 显式携带 origin；`install-linux.sh` 默认写死当前 hostname，可用 `--origin` 固定；旧 origin 行按陈旧数据保留或手动 `DELETE source LIKE 'linux:<old>:%'` |
| 面板引擎 file-change 早退（`new_events == 0`）会吞掉刚自动导入的结果 | 自动导入在早退判定之前，且导入非空本身触发本轮发布（§5.5） |
| `~/tokenme-sync` 不存在时的自动导入噪音 | 目录不存在 = 静默跳过（零开销；无同步需求的用户完全无感） |
| 显示机没装 `~/tokenme-sync` 里对应工具 → 导入行出现在"来源"列表但工具页缺图标 | 预期行为：来源列表照列（`lib.rs:708-725`）；工具图标表缺省无图标，不报错 |

## 9. 交付与 CI

- Linux 产物：`scripts/build-linux.sh` 用 cargo zigbuild 出 x86_64/aarch64 musl 全静态二进制，打成 `tokenme-cli-<ver>-<arch>-linux-musl.tar.gz`（内含二进制 + `install-linux.sh`）+ sha256；
- `release.yml` CLI 矩阵增补 `cli-linux` job（**additive**，不碰既有 mac/win job；Gitea 无 runner → 本地构建 + 手工挂 release，路径同 Windows）；
- 可选后续：面板按来源筛选（解析 `source` 前缀，不动 schema）；quota 样本跨机合并（同一账号时 Codex `rate_limits` 是账号级事实）。

### 9.1 同步健康度徽标（B 档：提前实施，2026-10-05）

显示机上"Linux 的数字有多新、最后一次成功合并是什么"必须可见——否则同步断了，
面板上的旧数字与安静的数字长得一模一样（同 §8 冻结徽标的教训）。

**写侧契约**：`tokenme import`（及面板引擎自动导入，同一函数）在**合并的同一事务里**写入 `meta` 表
key `sync:linux:<origin>`，value 为 JSON：

```json
{"origin": "<host>", "imported_at_ms": 0, "window_lo_ms": 0, "window_hi_ms": 0,
 "rows": 0, "file": "tokenme-<host>.jsonl.gz", "sha256": "…"}
```

只有通过 §5.3 对账门禁（合并结果 == manifest sums）才会 COMMIT——所以这行记录
是可信的"最后一次成功同步"；重复导入 `INSERT OR REPLACE` 同键覆盖，不累积。

**读路径**：`Index::report_facts` 读 `meta`（`LIKE 'sync:linux:%'`，坏 JSON 跳过）
→ `ReportFacts.syncs` → `Report.syncs`（`serde(default)`，旧快照缺字段读为空）。
CLI `report --json` 与面板同源获得，不需要各自查询。

**UI 规则**（Header 数据行，frozen 徽标之后）：

- 无同步记录 → 不渲染（既有用户零变化）；
- 有记录 → 显示最新一条：`Linux 同步 · <origin> · <age> · <rows> 行`；多台时尾缀
  `+N`，tooltip 列全部（含窗口与文件名）；
- `now - imported_at_ms > 24h` → 警示色；阈值是常量，可调；
- age 复用 `relativeTime`、行数复用 `count()`——与 frozen 徽标同一套格式工具。

**非目标**：CLI 文本表格不渲染 syncs（JSON 里有）；不做"哪个数字来自哪台机"的
逐行标注（那是 A 档：`SourceStatus` 增加按 origin 聚合的副行，留待真有多机数据再做）；
不做同步记录的手动清除（清一条 meta 行即可）。

## 10. 对抗审计修订记录（2026-10-05）

定稿后对全案做了对抗审计：全部代码引用逐条重验；合并语义在真实 SQLite（本机 3.51.0）上用 22 项断言实测——键合成稳定性、两次导入严格幂等、生长/收缩/credits 不对称谓词、rollup 与 `event` 逐日对拍、陈旧 rollup 不自愈、并发读旧或新、call 并集补缺（上游追加行为已语句级复现）。修订：

| 修订 | 原稿问题 | 现状 |
| :--- | :--- | :--- |
| §5.2-2 谓词 | "新总量>旧总量"没写清两侧定义 | 补精确表达式；不对称是冻结语义 |
| §5.2-5 call 语义 | "写入时插"歧义；照抄上游会累积重复行 | 改 `NOT EXISTS` 并集补缺 |
| §5.3 对账 | "per-tool sums 全等"没给公式 | 补 `source LIKE 'linux:<origin>:%'` + 窗口过滤 SQL |
| §5.2-4 原子性 | 未声明同事务 | 明确 event+call+rollup 单事务；实测读侧只看到旧或新 |
| §5.1 快照/容错/版本 | 缺 | 单只读事务；sha256+解压校验拒导；SQLite ≥ 3.24 |
| §5.1 窗口语义 | "窗口 30 天"没说迟到行 | 按事件 ts 过滤；首次全量 `--days 400` |
| §2/§6 面板与删行边界 | 来源列表/只增不删/升级顺序未写明 | 引 `lib.rs:708-725`；只增不删、先升显示机 |

（实验台为本机 scratch 产物，非仓库文件。）

### 10-2. 第二轮对抗审计：通信 / 安装 / 认证（2026-10-05，实施前）

对"Linux↔Windows↔mac 通信是否合理、Linux 安装是否最小操作、认证是否合理"三条轴做对抗审计，全部代码引用重验（`main.rs`/`args.rs`/`context.rs`/`ingest.rs`/`lib.rs`/`facts.rs`/`engine.rs`）、工具链实测（zig + cargo-zigbuild 装好；musl target 可用；`~/.ssh` 现有 bugx-cn 可达——真机 e2e 载体）、SQLite 语义沿用 10-1 的 22 项断言实验台。结论：

| # | 级别 | 发现 | 修复（本文档已改） |
| :--- | :--- | :--- | :--- |
| F1 | P0 正确性 | §5.3 原"窗口 `source LIKE` 汇总 == manifest"对账：Linux 端 purge 后显示机旧键行恒在（只增不删），对账从此恒失败 → **每次 ROLLBACK，导入永久卡死且无法自愈** | §5.3 改**按键对账**：清单键存在性 + 按键 sums 全等；陈旧行只提示 `stale N rows (kept)` |
| F2 | P0 最小化操作 | 原分期让**显示机**装 Python + launchd/schtasks 定时跑脚本，直接违背"用户最小化操作"；对账/upsert 逻辑也脱离仓库已实测 SQL | 砍掉 Python：Rust `tokenme export/import` 进仓库（§5）；显示机由**面板引擎每轮自动导入** `~/tokenme-sync`（§5.5），mac/Windows 零安装零配置 |
| F3 | P1 安装 | Linux 侧原说明"有 Rust 就 cargo build"——把工具链当成了用户环境 | `scripts/build-linux.sh`（zigbuild 出 musl 静态两 arch）+ `install-linux.sh`（一条命令装二进制/定时/推送，`--uninstall` 干净撤销）（§4/§9） |
| F4 | P2 CI | `release.yml` CLI 矩阵无 Linux target，版本发布时无 Linux 产物 | 增补 additive `cli-linux` job（§9） |
| F5 | P1 认证 | 原稿只说"任意文件通道"，没有认证/威胁模型一节 | 新增 §5.4：信任锚 = 已认证私有通道（ssh/Syncthing/共享盘 ACL）；不引入自造账号体系；sha256 只防损不防伪（有意非目标）；导入最坏=数字污染（参数化 SQL、四表可删可重导）；凭据绝不随导出走 |

另修复两处连带设计缺口：`meta` 需同时存自动导入的 sha memo（`sync:file:<name>`，与既有 `sync:linux:<origin>` 不冲突）；`.tmp + rename` 落盘顺序写明（§5.1）。落地方案即按修订后的 §3-§5 实施，验证记录进 §11。

## 11. 落地记录

**2026-10-05 · v0.1.5（不加版本号）· 按 §3-§5 + §9.1 全量实施完成。**

实施面：

| 件 | 位置 |
| :--- | :--- |
| 同步协议 | `crates/usage-index/src/sync.rs`（`export_sync`/`import_sync`，按键对账、单事务、tmp+rename、HashingWriter 嵌 GzEncoder 之下哈希压缩字节） |
| CLI | `tokenme export` / `tokenme import [--dry-run]`（args/main/commands 接线；export 自 ingest、允许空窗口） |
| 读路径 | meta `sync:linux:<origin>` + `sync:file:<name>` → `report_facts` → `Report.syncs` → 面板 `Linux 同步` 徽标（24h 警示、`?sync=<h>` QA pin） |
| 自动导入 | `engine.rs::import_sync_dir` 每轮扫描 `~/tokenme-sync`，sha memo 跳重，成功即发布（双 early-return 旁路） |
| 安装链 | `scripts/build-linux.sh`（zigbuild 静态 musl 双 arch + sha256.txt）、`scripts/install-linux.sh`（timer / cron 回退 / push / uninstall）、`release.yml` additive `cli-linux` job |
| 文档 | AGENTS.md「Linux CLI (collector side)」、COMMANDS.md（export/import + 限额/信任注记）、USER_GUIDE.md（§1/§3/§4 新章 Cross-machine sync/§8）、README.md + README_CN.md（常用命令行 + 数据与隐私段） |

实测证据（全部本机 + 真机，命令输出留档）：

- **真机往返（bugx-cn，x86_64 Ubuntu，musl 静态）**：`detect`→`export` 产出 bundle；manifest sha256 == 该机 `sha256sum` 独立输出（`a907c185…`，验证 HashingWriter 覆盖含 gzip trailer 的全部压缩字节）；bundle 拉回 mac → 全新 scratch db `import` = 3 new → 重导 = 0 new / 3 deduped（严格幂等）；`sync:linux:VM-0-7-ubuntu`（完整 SyncRecord）+ `sync:file:*`（memo）落库；`daily` 报表出现合并行（10-05：3 requests / 4.3k tokens / $0.01）。
- **本机真实数据 30 天窗口**：15,672 行导出（643 KB gz，1.2s）；`--dry-run` 验 0 写入回滚；正式导入 15,672 new；重导全 deduped；`calls+ 0` 与源窗口真实 0 个 call 行一致；元数据与清单逐字段相等。
- **单测**：usage-index lib 29 项全绿（其中 sync 模块 11 项），另集成 17 项（tests/ingest 14、tests/watcher 3+2 ignored）全绿；含 `imported_record_surfaces_in_report_facts`（真实导入 → `report_facts` → 恰好 1 条记录；坏 meta 行被跳过）。注：单次全量门径 exit 0 不等于稳定——温复跑暴露 `round_trip_is_idempotent_and_reconciles` 偶发失败（约 1/5），根因是测试自身的墙钟依赖而非协议缺陷，修复与复测见下方「测试自纠」。
- **徽标**：fixture 双态像素截图（正常：accent 色「Linux 同步 · build-01 · 6 分钟前 · 12,431 行 · +1 台」；`?sync=30`：24h 警示色、年龄「昨天」）+ 结构快照；面板重编译并已重装到 /Applications 运行。
- **自动导入链（生产路径实测）**：把 bugx-cn 的真实 bundle 投入 `~/tokenme-sync`，运行中的面板引擎下一轮自动合并（`panel.log` 21:52:57 `sync: merged … 3 rows (3 new, 0 updated, 0 deduped), calls +0`），meta 落库、发布即触发。
- **安装链真机双分支**：
  - systemd 正常路径：units 写入 → `enable --now` → linger 翻转 → one-shot 导出成功（无日志机 `rows 0` 空 bundle 也是合法产物）→ `INSTALLER-EXIT=0`。
  - cron 回退（`XDG_RUNTIME_DIR` 不可达模拟）：自动装 cron 行（`>/dev/null`，失败走 cron 邮件/journal），用户原有 crontab 两行原样保留。
  - `--uninstall` 双分支：units/timer/binary/cron 全清，crontab 复原为原两行，数据保留提示打印。
- **tarball 洁净**：`._*` 与 xattr 头均无，GNU tar 解包零警告。

实施期新发现（F6-F8，均修复并复测；审计记录追加）：

| # | 级别 | 发现 | 修复 |
| :--- | :--- | :--- | :--- |
| F6 | P1 最小化操作 | `install-linux.sh --every` 的 `systemctl --user enable --now` 未防护：容器/裸 ssh 上 set -e 令脚本半途死亡（units 已写、timer 未启用，且**为无会话机器写的 linger 提示永远不会打印**）；首次导出失败同样令整个安装 exit 1 | 守卫 enable；不可达时自动改装 cron job（同一 `tokenme-export-run.sh`）；首次运行失败只提示不失败 |
| F7 | P0 正确性（真机才暴露） | 无日志机 `tokenme export` 走 `prepare()` 的"空报告拒绝"直接失败 → collector timer 在空闲机器上**永远失败、永不产出** | export 改为自行 ingest、但不做空窗口拒绝（`rows 0` 合法）；`--no-ingest` 语义不变 |
| F8 | P2 打包 | macOS tar 携带 `._*` AppleDouble 与 xattr 头，GNU tar 每次解包警告 | `COPYFILE_DISABLE=1` + `--no-xattrs` |

诊断注记：F6 的现场最初表现为 `Failed to enable unit: Unit … does not exist`——首轮归因为沙箱 HOME 与 user manager 视角不一致（systemd --user 的 unit 搜索路径取自 manager 自身的 HOME），属测试工件；真正产品缺陷是失败路径未被守卫、提示不可达。两件事分开修复与记录。

自纠记录：自动导入链实测时，投放的 bundle 拿错了版本（把 e2e 的 3 行合成数据当成了空 bundle），3 行测试事件（source `linux:VM-0-7-ubuntu:/tmp/tme-e2e/…`）因此并入**本机生产索引**（event id 848320-848322，2026-10-05 当天 +4.3k）。处置：停面板 → `tokenme index --rebuild`（事件行清零、rollup 精确重算）→ `DELETE FROM meta WHERE key LIKE 'sync:%'`（meta 不随 rebuild 重建）→ 移除 `~/tokenme-sync` 种子目录 → 重启面板。复核：`event` linux 行 0、sync meta 0、面板日志新会话无 sync 行。教训：生产索引的投放验证必须先在 scratch db 验证文件内容（`zcat | wc -l` 应为 0），再进产线目录。

测试自纠（flake 根因，2026-10-05 晚）：

- 现象：`cargo test --workspace` 单次 exit 0；随后温复跑 `cargo test -p usage-index -p usage-cli` 出现 `round_trip_is_idempotent_and_reconciles ... FAILED`（`sync.rs:991` 断言 `#2` 键行计数 left:0 right:1），单跑必过、约 1/5 概率复现。
- 根因（两处，均为**测试侧墙钟依赖**；协议与产品路径无缺陷）：
  1. `base_source()` 用两次独立 `ts(2)` 造「两行完全相同的无键行」——`ts()` 每次调用各读一次墙钟，跨毫秒边界时两行的 `ts_ms` 差 1 ms。canonical 编码按设计包含 `ts_ms`（内容身份的一部分），于是两行成为**不同**的行、各得裸 `mix:` 键，`#2` 后缀不出现。生产路径的行时间戳来自日志解析、并非播种时读钟，故不受影响。
  2. 潜性（更早埋雷、从未触发）：rollup 对账断言把「全表 claude 行总和」与「`ts(5)` 当天一个日期的 rollup」相比——三行 ts 为 5/4/3 小时前，若套件在本地 03:00–05:00 运行必跨午夜（600 vs 300 必失败），本机测试时段恰好从未落入此窗。
- 算术复现（`TZ=Etc/GMT+11`，本地 03:23，straddle 成立）：旧断言 `ev_sum=600 vs roll(day(ts5))=300` → **FAIL**；新断言逐日映射 `{'10-04':300,'10-05':300}` 两侧全等 → **PASS**。
- 修复（只动测试，判据收紧不放松）：
  1. 两行重复无键行共用一次 `dup_ts` 读取——确定性构造「完全相同的两行」；
  2. 对账改为**逐日双向**核对：event 侧按 `local_day_of` 聚合、rollup 侧 `GROUP BY day`，两侧 `BTreeMap` 全等（旧断言「单日对全景、隐含不跨日假设」，新断言对 straddle 场景也成立且更严）。
- 复测：`TZ=Etc/GMT+11 cargo test -p usage-index`（straddle 窗口内）exit 0，lib 29 + 集成 14 + 3 全绿；15× 全 crate 循环复跑 **15/15 exit 0、零失败**（每轮 lib 29 + 集成 14 + watcher 3+2 ignored + doc-tests，含约 34–81 s 的真实数据 watcher 测试），原始 1/5 复现率归零。

约束加固与安全文档（2026-10-05 用户问询后追加）：

- bundle/manifest 落盘即 `0600`（unix；`File::create` 后立刻 `set_permissions`，rename 携带权限），`sync.rs` 单测断言两文件 mode 与内容不变；两架构 tarball 已随此变更重打（新 sha256 见 `dist/linux/sha256.txt`）。
- 用户面文档：`USER_GUIDE.md` 新增 §4「Cross-machine sync」（两侧最小操作步骤 + 安全与限额五条），原 §4-§7 顺延，§7 Privacy 补同步条；`COMMANDS.md` export/import 节补 0600、行数上限、「验证不签名」信任边界与 §4 引用；`README.md`/`README_CN.md` 各加常用命令行 + 数据与隐私段。
- 审计答复落档为本文件 §12。

## 12. 安全边界与约束（连接 / 安装 / 认证，2026-10-05 审计答复）

**连接模型**：同步不是网络服务——tokenme 自身不监听任何端口、无守护进程、无云账号与同步 token；「连接」只是你选择的文件通道（scp/ssh、Syncthing、挂载盘、USB）。认证完全由通道提供（ssh 密钥、Syncthing TLS + 设备 ID）；tokenme 不存储任何同步凭据，故不存在可泄露的同步身份。CLI 的出网行为均不属同步链路（配额探测、models.dev 价表各自独立）。安装链全程无需 sudo/root，定时器以当前用户身份运行。

**完整性约束（导入闸门，全部先于落行）**：

| 约束 | 作用 |
| :--- | :--- |
| sha256（覆盖含 gzip trailer 的全部压缩字节）对照 manifest | 传输损坏/截断即整批拒绝 |
| 每批 token 求和对照 manifest.sums | 手改或损坏的负载即拒 |
| format + schema_version 门 | 版本错配即拒，杜绝半合并 |
| 行数上限 = manifest.rows + 1M slack | 损坏文件不能拖垮内存 |
| 单批重复 key 拒绝 | 键冲突即整批回滚 |
| 单 IMMEDIATE 事务 | 任何失败整体回滚，旧数据不动 |
| 增长判定（新总量 > 旧总量+credits 才更新） | bundle 无法把行往回改小 |
| 只增不删 + stale 上报 | 导入绝不删本机既有行 |
| 幂等（按键去重 + 文件 sha memo） | 重导同一 bundle 零变化 |

**隐私约束**：bundle 与 manifest 落盘即 `0600`（unix；`sync.rs` 单测断言）。bundle 内容 = 索引同款元数据（token 数、模型/会话/项目名、本机日志路径），无代码/提示词/凭据。真机复验（bugx-cn，重打后二进制）：两文件均 `600`，manifest sha 与系统 `sha256sum` 全等（`ac73670a…`），空窗口 bundle 合法（rows 0），tarball 3 条目无 `._*`。

**信任边界（残留风险，明示）**：`~/tokenme-sync` 目录的可写者（你的账号，或被共享该目录的第三方）可注入用量行——合并去重安全、但**未签名**；在单用户机器上这等价于对索引本身的可写权。跨机传输的机密性与完整性由所选通道负责（scp=ssh，Syncthing=自身 TLS+设备 ID）。

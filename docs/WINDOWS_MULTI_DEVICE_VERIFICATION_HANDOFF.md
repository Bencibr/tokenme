# TokenMe Windows 多设备验证交接单

> 目标：让 Windows 客户端完成 **Windows 原生** 和 **跨平台真实网络** 验证，并回传可复核证据。
>
> 当前结论：协议和面板代码已进入仓库，但 Windows 原生 ACL、Windows named pipe、Windows 防火墙/发现行为以及 Windows 参与的三端局域网互通仍是 `UNVERIFIED`。本文件是验收清单，不是“已通过”声明。

## 1. 验证边界

### 1.1 传输和信任

- 数据通道固定为 **HTTPS over TCP + TLS 1.3**。
- 证书 SHA-256 pin、一次性邀请、Ed25519 设备身份和主端批准共同构成信任链。
- mDNS/Bonjour 的 `_tokenme-sync._tcp.local.` 仅发布候选地址、设备 ID、显示名和证书 pin；不广播 invite secret、配对 token、凭据、quota、TTFT、Prompt、源码或用量数据。
- UDP 5353 只用于 mDNS 发现，不能被当成同步传输；SSH、UDP 数据通道、QUIC、NAT 打洞和云中继不属于本功能。
- 主端和从端可以互换，但切换前必须停止当前角色；旧 trust、stream、receipt 和待传队列要隔离保留，不能静默合并成另一条流。

### 1.2 不允许的证据替代

以下只能证明源码或交叉编译，不得写成 Windows 通过：

- macOS/Linux 上的 `cargo test`、`cargo check`。
- `x86_64-pc-windows-gnu` 交叉编译成功。
- 只在同一台机器 `127.0.0.1` 上完成的 TLS round-trip。
- 只看到 mDNS TXT 记录，没有完成 pin 校验和主端批准。
- 只看到 UI 按钮，没有完成上传、receipt、重启和错误恢复。

## 2. 测试环境和安装

### 2.1 环境记录

记录以下信息，回传时不要附带密码、私钥、完整邀请 URL 或 invite secret：

- Windows 版本、架构、补丁级别（Windows 10/11 x64）。
- TokenMe 版本、Git commit（若从安装包测试则记录安装包文件名和 SHA-256）。
- 安装模式：只为当前用户，或所有用户。两种模式分别验证，不要在未卸载的情况下互相覆盖。
- Windows 用户是否为普通用户；管理员权限只用于确认安装和防火墙提示。
- 对端设备和系统：macOS、Ubuntu 版本及实际局域网地址。地址回传时可遮盖最后一段。
- 测试开始/结束时间、时区和设备显示名。显示名可以相同，设备 ID 不能相同。

### 2.2 使用已构建安装包

优先使用 Gitea 提供的最新安装包，安装后确认任务管理器中的进程为 `TokenMe.exe`，并确认没有同时运行旧版 `tokenme.exe`、`tokenme-bar.exe` 或第二份 TokenMe。

### 2.3 从源码构建（需要时）

在 Windows 工作区执行：

```powershell
Set-Location apps/tokenme-bar
pnpm tauri build --bundles nsis
```

预期产物：

```text
apps/tokenme-bar/src-tauri/target/release/bundle/nsis/TokenMe_<version>_x64-setup.exe
```

构建前必须准备仓库要求的 collector 资源和 Zig/cargo-zigbuild；如果构建因工具链缺失失败，回传完整命令和退出码，不要修改协议或放宽安全门来“通过”。

## 3. Windows 本机安全边界

### 3.1 实际数据和日志位置

默认位置如下；如果系统策略重定向了目录，回传实际路径：

| 内容 | 默认位置 |
| --- | --- |
| 用量索引 | `%LOCALAPPDATA%\tokenme\index.db` |
| SQLite 辅助文件 | `%LOCALAPPDATA%\tokenme\index.db-wal`、`index.db-shm`（存在时） |
| 同步身份、证书、outbox、配置 | `%LOCALAPPDATA%\tokenme\device-sync\` |
| 面板日志 | `%LOCALAPPDATA%\tokenme\logs\panel.log` |
| 设置 | `%APPDATA%\tokenme\settings.json` |

不要把这些文件上传到公共位置。证据包只应包含脱敏后的 ACL、文件名、时间戳和日志片段。

### 3.2 ACL、重解析点和隔离

在 **普通用户** PowerShell 中执行：

```powershell
$ErrorActionPreference = 'Stop'
$Data = Join-Path $env:LOCALAPPDATA 'tokenme'
$Sync = Join-Path $Data 'device-sync'
Get-Acl $Data | Format-List Path,Owner,AccessToString
Get-Acl $Sync | Format-List Path,Owner,AccessToString
Get-ChildItem -Force $Sync | Select-Object FullName,Length,LastWriteTime
```

验收条件：

- 数据目录 owner 是当前用户或系统受控账户；`Everyone`/普通 `Users` 不得拥有写入同步数据库、身份文件、证书私钥和 outbox 的权限。
- 同步目录、身份文件、证书私钥、SQLite/WAL/SHM 的 ACL 不得继承到可被其他用户写入的位置。
- 目录和关键文件不是重解析点或指向另一用户目录：

```powershell
fsutil reparsepoint query $Data
fsutil reparsepoint query $Sync
```

命令返回“不是重解析点”属于预期；如果确实存在重解析点，记录目标并标记 `NEEDS_CHANGE`，不要删除用户数据。

- 创建第二个低权限 Windows 用户后，该用户不能读取第一用户的 identity、证书私钥、outbox、同步配置或控制面。

### 3.3 Windows 控制面（named pipe）

`sync stop/status` 的 Windows 实现应使用用户私有 named pipe，不得把控制面退化为公开 TCP 端口。验证：

1. 以普通用户启动从端 runtime。
2. 以同一用户执行 `tokenme sync status` 和 `tokenme sync stop`，确认能读状态并停止。
3. 以另一低权限用户执行同样命令，必须被拒绝，且不能影响第一用户的 runtime。
4. runtime 异常退出后重新执行 `sync run`，确认旧 pipe 被清理，不能卡在“已运行”。
5. 退出/重启过程中不要删除 identity、outbox 或待传数据。

如果 Windows 版本尚未提供 named pipe ACL 或命令无法运行，回传错误和日志并标记 `UNVERIFIED/NEEDS_CHANGE`，不要用 loopback TCP 临时替代。

## 4. Windows 主端验证

### 4.1 启动、监听和邀请

在 TokenMe 面板进入“设备同步”：

1. 选择“主设备”。
2. 选择实际 LAN/VPN 网卡或填写明确的监听地址；允许局域网时不能使用 loopback。
3. 启动服务，确认界面显示运行中、实际端口和证书 pin。
4. 用 PowerShell 检查 TCP 监听（端口以面板显示为准）：

```powershell
Get-NetTCPConnection -State Listen | Where-Object LocalPort -eq 47821 |
  Format-Table LocalAddress,LocalPort,OwningProcess,State
```

5. 确认 Windows Defender 防火墙只放行用户选择的同步 TCP 端口和必要的 mDNS 发现流量；不能为了测试关闭整个防火墙。
6. 生成完整邀请和二维码文件。二维码/邀请中可以包含 endpoint、pin、证书和一次性邀请信息，但不能把内容贴到公开 issue 或聊天记录。
7. 检查邀请 endpoint 是对端可达的具体 IPv4/IPv6 地址，不是 `127.0.0.1`、`::1` 或 `0.0.0.0`。

### 4.2 mDNS 发现

在另一台设备打开配对页，观察 Windows 主端是否发布 `_tokenme-sync._tcp.local.`：

- 候选至少显示设备名、设备 ID、endpoint 和 pin。
- 同名不同 ID 要并列显示；同 ID 不同 pin 要显示身份冲突并禁止一键选择。
- 停止主端后应注销广播；等待一个 TTL 周期后候选消失。
- mDNS 发现失败必须给出可操作提示：本地网络/防火墙、UDP 5353、监听端口和二维码/完整邀请回退。
- Windows 没有类似 macOS 的系统“本地网络权限”弹窗时，记录系统实际行为；不能把“没有弹窗”写成缺陷，也不能声称 macOS 权限已通过。

## 5. Windows 从端验证

### 5.1 mDNS 一键配对

1. Windows 从端先停止已有主端服务或旧从端 runtime。
2. 打开设备同步配对页，等待扫描结束或点击取消，确认扫描不会无限等待。
3. 选择正确候选，确认界面说明“发现不等于信任”。
4. 一键配对时，客户端先使用显示的证书 pin 请求短期邀请；主端面板出现待批准请求后，人工批准。
5. 批准后确认 endpoint、server name、证书 pin、主设备 ID、stream/epoch 和本机 identity 被持久化。
6. 启动从端 runtime，确认状态为运行中，并且待传数量/最后连接时间会变化。

### 5.2 二维码/完整邀请回退

当 mDNS 无结果、跨子网、VPN 或防火墙阻断时：

- 用文件二维码或完整邀请导入，不要求用户手工填写 stream、generation 或证书路径。
- 错误 pin、过期邀请、已使用邀请、主端拒绝和 endpoint 不可达必须分别显示可操作错误。
- 邀请 secret 只在 TLS pin 校验后发送；日志和错误提示不得回显 secret。

## 6. 跨平台真实网络矩阵

每个案例都必须使用真实 LAN/VPN 地址，并保存两端的时间戳、版本、commit、脱敏日志和结果。

| 案例 | 主端 | 从端 | 必须证明 |
| --- | --- | --- | --- |
| A | macOS | Windows | Windows 原生发现/二维码、pin、批准、TLS 上传和 receipt |
| B | Windows | Ubuntu | Windows 原生监听、防火墙、Ubuntu 配对、上传和 ACK |
| C | Ubuntu | Windows | Windows 原生从端 runtime、named pipe、断线 outbox 和恢复 |
| D | Windows | macOS | Windows 主端停止广播，macOS 能看到停止并走邀请回退 |
| E | Windows | Windows | 两台 Windows 设备的不同 identity、ACL 隔离和重复幂等 |

至少完成 A、B、C；E 如果有两台 Windows 设备则必须执行。

### 6.1 数据上传和幂等

在 Windows 从端产生一条新的、可定位的测试用量事件：

1. 启动 runtime，确认首条事件在主端出现，主端 receipt/highwater 前进。
2. runtime 保持运行时再产生第二条事件，确认在下一轮自动上传，不需要手工 `export`。
3. 重启从端，确认同一 generation 不重复计数。
4. 重放同一个包，主端 receipt 幂等，事件和 token 不翻倍。
5. 主端停止期间产生事件，确认本地 outbox 保留；主端恢复后按 FIFO 补传。
6. 在主端查看设备范围筛选，确认本机、远端、全部数据边界清楚。
7. 检查同步 payload 不包含配额凭据、TTFT 原始日志、Prompt、源码、私钥或邀请 secret。

### 6.2 断线、暂停、撤销和角色互换

逐项执行并记录 UI 状态、日志和最终数据库结果：

- 拔掉网络或阻断 TCP 端口：进入离线/待传，不删除本地数据。
- 暂停同步：网络上传停止，但已绑定设备的本机采集继续；恢复后补传暂停期间事件。
- 主端暂停/撤销设备：从端收到明确错误并停止发送，不能无限重试或清空 outbox。
- 停止主端：监听端口关闭、mDNS 注销、邀请失效；从端显示主端离线。
- Windows 从端停止后切换为主端：先停止 runtime，再启动主端；旧绑定、receipt 和 outbox 仍可审计但不混入新主端流。
- 重新配对或切换回从端：要求明确动作，不能因应用重启自动把角色改回去。

### 6.3 重启恢复

分别重启 Windows 主端和从端：

- 只有之前明确启用的角色自动恢复；用户显式停止的角色保持 disabled。
- primary 的监听地址/端口、证书 pin 和 mDNS 状态恢复正确；不能恢复成 `0.0.0.0` 邀请地址。
- secondary 的 identity、primary/stream/epoch、outbox、last ACK/highwater 和 enabled 状态恢复。
- 不产生重复 generation、重复 receipt 或重复 token。
- 控制面 pipe、TCP listener 和 mDNS daemon 不残留孤儿实例。

## 7. 故障注入清单

逐项记录“操作、界面文字、panel.log 片段、退出码、恢复动作”：

- 证书 pin 错误。
- 邀请过期、重复使用、主端拒绝。
- 端口被占用。
- Windows 防火墙阻断监听或 UDP 5353。
- 主端停止、IP 改变、DNS/mDNS 超时。
- HTTP 400/403/409/413/426：应暂停并给出重新配对/对账/升级动作。
- 网络错误、429、503：应退避重试并保留 outbox。
- 磁盘空间不足、ACL 被收紧、SQLite/WAL 损坏：本机用量不可回滚或静默丢失。
- 扫描结果为空、重复设备、同 ID 不同 pin、IPv6 link-local 地址缺少接口标识。

## 8. 证据采集模板

回传一个脱敏目录或压缩包，目录名不要包含邀请 secret：

```text
windows-sync-<date>/
  README.txt                  # 版本、commit、OS、安装模式、测试矩阵结果
  environment.txt             # 命令和退出码；不含密码/私钥
  acl-data.txt                # Get-Acl 输出
  listener.txt                # Get-NetTCPConnection 输出
  firewall.txt                # 相关规则名称和状态
  panel.log.redacted          # 只保留必要时间段，删除 token/secret/path 中的敏感部分
  cases/
    A-macos-primary-windows-secondary.txt
    B-windows-primary-ubuntu-secondary.txt
    C-ubuntu-primary-windows-secondary.txt
    D-role-switch.txt
    E-replay-and-restart.txt
```

`README.txt` 对每个案例使用以下状态之一：

- `PASS`：Windows 原生操作和跨设备结果都有可复核证据。
- `PARTIAL`：链路部分完成，但有明确未完成步骤。
- `NEEDS_CHANGE`：发现可重现的代码、UI、安全或数据一致性问题。
- `UNVERIFIED`：没有可用的 Windows 原生/真实设备证据。
- `BLOCKED`：外部设备、权限或环境阻断，并附阻断原因。

## 9. Windows 客户端回传格式

请按下面格式回传，不要只说“可以/不可以”：

```text
TokenMe version:
Git commit or installer SHA256:
Windows version/arch:
Install mode: per-user | all-users
Test devices:

Case A: PASS/PARTIAL/NEEDS_CHANGE/UNVERIFIED/BLOCKED
- command or UI action:
- expected:
- actual:
- exit code:
- evidence file:

Case B: ...

Windows ACL/named pipe:
- result:
- evidence:

mDNS/firewall:
- result:
- evidence:

Reboot and role switch:
- result:
- evidence:

Sensitive-data check:
- result:
- evidence:

Open issues and exact reproduction steps:
```

不要以“交叉编译通过”“macOS loopback 通过”“看到了二维码”替代上述证据。所有未执行项目保留 `UNVERIFIED`，由主仓库维护者在合并验收报告中汇总。

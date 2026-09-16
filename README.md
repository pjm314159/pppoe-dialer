# pppoe — Windows 宽带自动拨号服务

一个常驻后台的小服务：**把指定的 PPPoE 宽带连接自动拨通，并一直保持在线**。

装好之后你基本不用再管它 —— 开机自动运行，网线没插就不拨号，掉线自动重连，
拔了网线插回来也会自己恢复。

支持 Windows 10 / 11 / Server 2016 及以上（64 位）。

---

## 它能做什么

| 能力 | 说明 |
| --- | --- |
| 开机自动拨号 | 随 Windows 启动，不需要登录桌面 |
| 网线检测 | 拨号前先看网卡的物理链路，网线没插就不拨号，插上后自动继续 |
| 不重复拨号 | 连接已经在线时直接跳过，不会产生第二个连接 |
| 断线自动重连 | 掉线后自动重拨；连续失败会让重试间隔逐步变长，不会疯狂重试 |
| 免手工建连接 | 默认按 PPPoE 模板自动创建电话簿条目，**不需要**先在「网络和共享中心」里建「宽带连接」 |
| 停止不断网 | 停止服务**不会**断开宽带连接，连接会继续在线 |
| 账号密码不外泄 | 账号与密码**永远不会**出现在日志文件或 Windows 事件日志里 |
| 省资源 | 空闲时约 1 MB 内存、CPU 占用 0%，不产生周期性网络查询 |

---

## 安装

### 1. 下载并解压

从本项目的 **Releases** 页面下载 `pppoe-<版本>-x86_64-pc-windows-msvc.zip`，
解压到任意固定目录，例如 `D:\pppoe`：

```text
D:\pppoe\
├── pppoe.exe
├── pppoe.toml
├── install.ps1
├── uninstall.ps1
└── README.md
```

`pppoe.toml` 就是配置文件本身（内含逐行注释），解压后直接编辑它即可；
`pppoe.exe` 与 `pppoe.toml` 必须放在同一个目录里。两个 `.ps1` 脚本会自动申请管理员权限。

> 下载页同时提供 `SHA256SUMS.txt`，可用它校验文件是否完整：
>
> ```powershell
> Get-FileHash .\pppoe-0.1.0-x86_64-pc-windows-msvc.zip -Algorithm SHA256
> ```
>
> 结果与 `SHA256SUMS.txt` 中的一行一致即可。

### 2. 写配置

```powershell
notepad D:\pppoe\pppoe.toml
```

最少只需要改三个值：

```toml
[dial]
entry_name = "Dr.COM"        # 连接的名称，随便起个认得出来的即可
username   = "12345678"      # ← 换成你的宽带账号
password   = "12345678"      # ← 换成你的宽带密码
```

> 配置文件包含明文密码，建议按第 3 步限制它的访问权限。
> 详细字段（重连间隔、日志级别、指定网卡等）在 `pppoe.toml` 里都有逐行注释。

### 3. 限制配置文件的权限

```powershell
icacls D:\pppoe\pppoe.toml /inheritance:r /grant:r "BUILTIN\Administrators:F" "NT AUTHORITY\SYSTEM:F"
```

只保留管理员与系统账户可读写。

### 4. 先试拨一次（推荐）

在装服务之前先确认配置正确。这条命令会拨一次号，**然后立刻挂断**，不会干扰现有网络：

```powershell
D:\pppoe\pppoe.exe dial-once
```

看到 `dial succeeded` 就说明账号、连接名都没问题；失败请对照文末的「排障速查」。
想让它拨完保留连接用于观察，加 `--keep`。

### 5. 安装并启动服务

**推荐用脚本**（普通 PowerShell 窗口即可，脚本会自己弹出 UAC 申请管理员权限）：

```powershell
cd D:\pppoe
.\install.ps1
```

脚本依次做四件事：申请管理员权限 → 检查 `pppoe.exe` 与 `pppoe.toml` 是否齐全 →
**检查账号和密码是否已填写**（缺失时会指出该改哪两行并拒绝安装；模板里的示例值 `12345678` 不算缺失）→ 注册服务并启动。
想先看它要做什么、又不改动系统：`.\install.ps1 -DryRun`。

或者手动执行（需要管理员 PowerShell）：

```powershell
D:\pppoe\pppoe.exe install     # 注册为开机自启动的服务
sc.exe start PppoeDialer
sc.exe query PppoeDialer       # 状态应为 RUNNING
```

安装完成后可以在 `services.msc` 里看到名为 **PPPoE Auto Dialer** 的服务。
服务被意外终止时，Windows 会在 5 秒后自动把它拉起来。

---

## 常用命令

```powershell
D:\pppoe\pppoe.exe dial-once          # 试拨一次（拨完挂断）
D:\pppoe\pppoe.exe dial-once --keep   # 试拨一次并保留连接
D:\pppoe\pppoe.exe list-connections   # 看当前有哪些拨号连接、是否在线
D:\pppoe\pppoe.exe list-adapters      # 看所有网卡及链路状态（排查网线检测问题）
D:\pppoe\pppoe.exe console            # 前台运行、日志直接打在屏幕上，Ctrl+C 退出
D:\pppoe\pppoe.exe help               # 全部命令

sc.exe start PppoeDialer              # 启动服务
sc.exe stop  PppoeDialer              # 停止服务（不会断网）
D:\pppoe\pppoe.exe uninstall          # 停止并卸载服务
```

`install` / `uninstall` 需要管理员权限；`stop` 同样需要。

---

## 配置说明

完整注释见 `pppoe.toml`。最常用的几项：

| 配置项 | 默认值 | 说明 |
| --- | --- | --- |
| `dial.entry_name` | 必填 | 连接名称，例如 `Dr.COM` |
| `dial.username` | 必填 | 宽带账号 |
| `dial.password` | 必填 | 宽带密码 |
| `dial.create_entry_if_missing` | `true` | 条目不存在时自动创建，一般不用改 |
| `dial.pbk_path` | 空 | 电话簿文件路径，留空表示用服务自己账户的电话簿 |
| `dial.dial_timeout_secs` | `90` | 单次拨号的最长等待，**同时也决定了 `sc stop` 的最坏耗时** |
| `link_check.enabled` | `true` | 拨号前检测网线 |
| `link_check.adapter_name` | 空 | 指定由哪块网卡决定「网线插没插」，多网卡时建议填 |
| `monitor.reconnect_delay_secs` | `5` | 失败后首次重试的等待秒数 |
| `monitor.max_backoff_secs` | `300` | 重试间隔的上限 |
| `log.dir` | `logs` | 日志目录，相对路径基于 `pppoe.exe` 所在目录 |
| `log.level` | `info` | `error` / `warn` / `info` / `debug`；排障时用 `debug` |
| `log.max_files` | `14` | 保留最近多少个日志文件 |

修改配置后需要重启服务才生效：

```powershell
sc.exe stop PppoeDialer ; sc.exe start PppoeDialer
```

---

## 三个最容易踩的坑

### 1. 提示找不到连接 / 错误码 623、624、651

服务是以**系统账户**运行的，它使用的电话簿和你桌面登录账户的**不是同一个文件**。
所以你在「网络和共享中心」里建的连接，服务默认是看不见的。

**默认配置已经帮你处理了**：`create_entry_if_missing = true` 会让程序在启动时
按 Windows「宽带 (PPPoE)」向导的规格，自动在服务自己的电话簿里把这条连接建出来 ——
所以正常情况下你只要在 `pppoe.toml` 里填对连接名、账号、密码就够了。

如果你想复用自己的那条连接（比如里面配过特殊参数），把 `create_entry_if_missing` 设为 `false`，
并让它复用共享电话簿：

1. 打开「网络连接」，右键你的宽带连接 → 属性 → 选项 → 勾选
   **「允许其他用户使用此连接」**；
2. 配置：

   ```toml
   [dial]
   pbk_path = 'C:\ProgramData\Microsoft\Network\Connections\Pbk\rasphone.pbk'
   create_entry_if_missing = false
   ```

   > 单引号字符串里的反斜杠不需要转义；用双引号则要写成 `\\`。

### 2. 网线检测判错（多网卡环境）

一台普通电脑上往往有几十个「虚拟网卡」（WFP、Npcap、QoS Packet Scheduler、
Hyper-V、VMware 等），它们**也会报告「已连接」**，拿它们判断「网线插没插」是错的。

程序会先排除这些虚拟过滤层，再优先使用真实物理网卡。如果机器上有多块真实网卡
（比如同时有板载网卡和 USB 网卡），请明确指定用哪一块：

```powershell
D:\pppoe\pppoe.exe list-adapters
```

输出里 `hw` 列为 `true` 且 `media` 为 `Connected` 的就是真实网卡，把它的名字填进配置：

```toml
[link_check]
adapter_name = "Realtek PCIe GbE"
```

不想做网线检测（例如认为它误判），可以直接关掉：

```toml
[link_check]
enabled = false
```

### 3. 停止服务后网络还是通的

这是**刻意设计**的：连接由 Windows 的拨号管理器持有，服务退出后连接会继续保持在线，
下次启动服务会识别为「已在线」并跳过拨号，不会产生重复连接。

副作用是：停止服务时**不会打断**正在进行的拨号（强行打断可能误杀刚建立的连接），
所以 `sc stop` 最坏要等 `dial.dial_timeout_secs`（默认 90 秒）才返回。
把该值调小可以缩短这个时间。

如果你确实想停下来就断网：

```powershell
rasdial "Dr.COM" /disconnect
```

---

## 日志与排障

日志默认写在 `pppoe.exe` 同目录的 `logs\` 文件夹，按天分文件、自动清理旧文件。

```powershell
# 实时查看（-Encoding UTF8 是必须的，否则中文网卡名会显示成乱码）
Get-Content D:\pppoe\logs\pppoe.log.2026-09-15 -Encoding UTF8 -Tail 100 -Wait

# 事件查看器：Windows 日志 → 应用程序，来源 = PppoeDialer
Get-EventLog -LogName Application -Source PppoeDialer -Newest 20
```

一条正常的启动日志长这样：

```text
2026-09-15 15:53:54.777 INFO  pppoe: pppoe 0.1.0 console mode - press Ctrl+C to stop
2026-09-15 15:53:54.777 INFO  pppoe: effective configuration: ... user:"***", password:"***" ...
2026-09-15 15:53:54.792 INFO  pppoe::worker: connection "Dr.COM" is already online - no dial needed, waiting for events
```

日志中的级别与事件日志 ID 对应关系：`1000` = 错误、`1001` = 警告、`1002` = 信息、`1003` = 调试。

### 排障速查

| 现象 | 处理方向 |
| --- | --- |
| 服务启动后马上停止 | 多半是配置有误。日志（或事件查看器）里会指出**字段名**，例如 `dial.username must not be empty` |
| 反复出现 `cable not connected` | 用 `list-adapters` 确认网卡的 `media` 是否为 `Connected`；多网卡时设置 `link_check.adapter_name` |
| 反复出现 `code=623` / `624` / `621` | 电话簿问题，见上文「坑 1」 |
| 反复出现 `code=691` | 账号或密码不对（出于安全，日志不会回显它们） |
| 反复出现 `code=678` / `651` | 对端无应答或网卡/驱动异常，检查网线、光猫与网卡驱动 |
| 一整天只有 `safety re-check` 之类的记录 | 本机网卡驱动不上报链路变化，把 `monitor.safety_recheck_secs` 设为 `300` 作为兜底 |
| `install` 报 `OpenSCManagerW failed` | 没有用管理员权限运行 |
| 日志里看到 `created the phone book entry "..."` | 正常，表示服务首次为你在系统电话簿里建好了连接 |

### 确认账号密码没有落到日志里

```powershell
Select-String -Path D:\pppoe\logs\*.log.* -Pattern "你的账号","你的密码"
```

结果应为空。

---

## 升级与卸载

**升级**：停止服务 → 用新版本的 `pppoe.exe` 覆盖旧的 → 启动服务。
`pppoe.toml` 不用动，新增的配置项会自动使用默认值。

```powershell
sc.exe stop PppoeDialer
Copy-Item <新版>\pppoe.exe D:\pppoe\pppoe.exe -Force
sc.exe start PppoeDialer
```

> ⚠️ 新版的压缩包里带的是**示例配置** `pppoe.toml`（账号密码是示例值 `12345678`）。
> 解压时不要直接覆盖 `D:\pppoe\pppoe.toml`，否则会把已有配置冲掉 ——
> 只取 `pppoe.exe`（以及脚本）即可，或者先把配置文件备份出来。

**卸载**（推荐用脚本，同样自动申请管理员权限）：

```powershell
cd D:\pppoe
.\uninstall.ps1
```

它会先检查服务是否已安装、请求停止、再删除注册。也可以手动执行：

```powershell
D:\pppoe\pppoe.exe uninstall      # 先停止再删除服务，不会断网
Remove-Item -Recurse D:\pppoe     # 确认不再使用后，删掉目录（含配置与日志）
```

> 卸载**不会**断开宽带连接（连接由 Windows 拨号管理器持有，不归服务管），
> 脚本会提示如何立即断开：`rasdial "Dr.COM" /disconnect`。

---

## 隐私说明

- 宽带**账号与密码只用于拨号**，不会被写入日志文件，也不会写入 Windows 事件日志；
  配置摘要里它们一律显示为 `***`。
- 程序不联网回传任何数据、不做遥测；它只调用 Windows 自带的拨号（RAS）接口。
- 配置文件中保存的是明文密码，请按「安装」第 3 步限制文件权限。

---

## 许可证

本项目以 **GNU General Public License v3.0 或更新版本（GPL-3.0-or-later）** 发布，
完整条文见代码仓库中的 [`LICENSE`](LICENSE)。

> 发布压缩包里只含 `pppoe.exe`、`pppoe.toml` 和 `README.md`，不含许可证文件；
> 许可证全文请从代码仓库或发布页获取。

你可以自由地使用、修改和再分发本程序；但再分发时必须以同样的许可证开放源代码，
并且本程序**不提供任何担保**。

源代码可从本项目的代码仓库获取 —— 每个发布版本都有对应的 tag，
`pppoe.exe` 的版本号（`pppoe.exe version`）与 tag 一致。

# Windows 应用内更新

Windows x64 MSI 安装版可在设置的更新页面检查版本、下载更新，然后选择“安装并重启”。启动时的自动检查开关只控制自动检查；手动检查始终可用。新版本提示条与设置页使用同一更新状态，多窗口不会重复发起下载或安装。

应用读取最新正式发布附件中的 `latest.json` 获取版本，不消耗匿名 GitHub REST API 的共享 IP 配额。发布工作流等待各平台构建结束后，根据实际上传的附件生成清单，包含版本、官方下载地址、精确大小和 GitHub 提供的 SHA-256。只有清单不存在（HTTP 404）时才查询 REST API，以兼容未提供清单的发布；API 限流时，本次运行内按 `Retry-After` 或配额重置时间暂停 API 请求，并在手动检查时显示可重试的时间。没有有效恢复时间的限流响应等待至少一分钟。等待期间仍可读取静态清单或手动打开发布页面。

MSI 安装版只选择对应版本的官方 Windows x64 MSI。下载按发布元数据校验大小和 SHA-256，安装辅助进程在接管时再次验证文件。网络失败、包不完整、校验不匹配或发布中尚未提供 MSI 时，程序继续运行并显示可重试的错误；发布页面仍可手动打开。

安装前会要求用户确认关闭所有窗口和连接，并提醒保存编辑中的文件。辅助进程就绪后，应用使用已有退出流程保存窗口布局、停止连接并退出；辅助进程等待退出完成后，调用 Windows Installer 安装 MSI。覆盖升级、原安装位置和安装回滚由现有 MSI 定义负责。安装可能需要管理员授权，不会自动重启 Windows。

安装结束或取消后，辅助进程删除本次下载包及辅助临时文件，并尝试从原安装位置重新启动应用，保留当前数据目录。安装失败会显示 Windows Installer 返回的错误。若安装返回需要重启 Windows，程序会提示用户。

临时文件位于应用缓存目录的 `updates/<UUID>/` 下。下载和安装各自持有同一目录的共享读取句柄；清理需要独占访问，因此不会删除仍在进行中的更新。正常退出会清理未安装的下载；下次启动会重试清理异常退出或文件暂时被占用留下的文件。清理只删除已知临时文件，不递归删除目录内容，也不触碰 Windows Installer 系统缓存或用户配置。

便携版、源码运行和其他系统保留通过发布页面更新的方式。应用内安装要求当前程序路径与 MSI 写入的安装位置匹配。

## Windows 运行依赖

更新模块使用 Windows 10 / Windows 11 x64 都提供的系统组件。辅助脚本调用系统自带的 Windows PowerShell 5.1 和 .NET Framework，通过 `Start-Process -Verb RunAs -Wait` 启动 `msiexec.exe /i /passive /norestart`。脚本保留 UTF-8 BOM，以正确读取中文文本；无需 PowerShell 7、额外安装的 .NET 或 winget。[PowerShell 5.1](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_windows_powershell_5.1)、[Start-Process 5.1](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.management/start-process?view=powershell-5.1) 和 [msiexec](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/msiexec) 的要求见微软文档。

安装位置检测使用 `RegGetValueW`，临时文件互斥使用 Windows 文件共享模式；这些接口在 Windows 10 上可用。下载沿用项目已有的 `ureq` 和 SHA-256 依赖。更新代码不依赖 Windows 11 专属接口，也不按系统名称或版本号选择安装路径。

当前真实 MSI 测试环境为 Windows 11 x64、Windows PowerShell 5.1。Windows 10 的接口和脚本兼容性已经核对，Windows 10 真机或虚拟机上的安装、回滚和清理仍待验证；此核对范围仅为新增更新模块。

## 代码位置

- `src/update.rs`：发布信息、安装包选择和流式下载校验。
- `src/update/windows.rs`：识别 MSI 安装、暂存文件、辅助进程握手和清理。
- `src/update/install.ps1`：等待退出、启动 MSI、清理和重新打开应用。文件使用 UTF-8 BOM，以兼容 Windows PowerShell 5.1。
- `src/app/update.rs`：共享更新状态以及 Slint 回调。

## 更新检查验证

验证环境：Windows 11 x64（系统构建 26200）、Rust / Cargo 1.98.1；验证日期为 2026-10-22。定向命令 `cargo test --locked --bin meatshell update::tests` 共通过 14 个用例，覆盖静态清单、旧发布回退、限流恢复时间及连续检查、无效清单、版本比较、官方 MSI 选择和大小 / SHA-256 校验。HTTP 用例使用本机回环服务器，不消耗 GitHub API 配额。

发布工作流通过 actionlint 1.7.7 检查。使用 jq 1.8.1 对工作流中的实际生成和验证表达式进行了测试：仅包含成功上传的当前版本附件，排除未完成上传、其他版本和清单自身；保留精确大小和摘要；拒绝缺少校验值、空附件列表、草稿及预发布清单。GitHub Actions 的真实发布任务尚未运行，需要正式发布提供 `latest.json` 后，已包含本实现的客户端才会使用静态清单。0.7.8 客户端仍使用原有 API。

本次检查不改变安装、回滚或临时包清理流程；没有重新执行 MSI 安装测试，也没有在 Windows 10 真机上运行。更新检查使用已有跨平台依赖，不新增 Windows 11 专用接口。

## 定向验证

```powershell
cargo check --locked --bin meatshell
cargo test --locked --bin meatshell update::
powershell.exe -NoProfile -ExecutionPolicy Bypass -File tests/update/installer.ps1 -ScratchDirectory <临时测试目录>
```

Rust 用例覆盖正式版本比较、安装包来源、校验值、下载中断、磁盘写入失败及清理锁。PowerShell 用例用模拟安装器覆盖成功、需要重启、取消、安装失败、管理员授权取消、校验失败和重启参数转义，不会安装软件。

真实 MSI 测试需要 Windows、WiX 3、MSVC Rust 工具链以及系统自带的 .NET Framework 编译器：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File tests/update/msi.ps1 -WixBin <WiX目录> -ScratchDirectory <临时测试目录>
```

脚本从 `wix/main.wxs` 生成独立产品标识的测试 MSI，载荷为可记录启动版本的测试程序，使用实际更新辅助脚本和 Windows Installer 执行安装。测试覆盖成功升级、文件替换后取消（1602）、文件替换后失败（1603）及失败后重试；取消和失败由 MSI 内的延迟操作触发，安装回滚由 Windows Installer 实际执行。

核对内容包括磁盘和重新启动的版本、产品注册状态、自定义安装位置、含空格和中文的数据目录、配置文件以及每次更新暂存目录的删除。测试需要管理员授权，结束时卸载测试产品；构建产物、安装日志和 `result.json` 保留在指定测试目录。这组测试覆盖安装链路，真实 MeatShell 界面交互和人工拒绝 UAC 的操作仍需交互验收。

## 当前分支的测试结果

验证日期：2026-10-14（Asia/Shanghai）。以下结果对应本分支的更新实现和 `tests/update/` 用例，未执行全仓库测试。

### 环境

| 项目 | 实际环境 |
| --- | --- |
| 操作系统 | Windows 11 x64；安装测试日志记录的内核版本为 `10.0.26200.0` |
| 安装辅助进程及 PowerShell 测试 | 系统 Windows PowerShell `5.1.26100.9444`，Desktop Edition；使用 `System32/WindowsPowerShell/v1.0/powershell.exe` |
| CLR | `.NET Framework CLR 4.0.30319.42000` |
| Rust / Cargo | `rustc 1.98.1` / `cargo 1.98.1`，Windows MSVC 工具链 |
| MSI 构建 | WiX `3.14.1.8722`，x64；沿用仓库 `wix/main.wxs` 的安装及 MajorUpgrade 定义 |
| MSI 执行 | 系统 Windows Installer；测试运行器以管理员身份运行，辅助脚本实际调用 `msiexec.exe` |
| 测试产品及数据 | 独立产品和组件标识；带空格的自定义安装目录；带空格和中文的数据目录；合成配置文件 |

### 编译与定向测试

| 检查 | 结果 | 验证内容 |
| --- | --- | --- |
| `cargo check --locked --bin meatshell` | 通过 | 当前桌面二进制及 Slint 界面代码能够通过编译检查 |
| `cargo test --locked --bin meatshell update::` | 10 个通过，0 个失败；405 个无关用例被过滤 | 发布解析和下载校验 7 个，Windows 清理行为 3 个 |
| `tests/update/installer.ps1` | 7 项检查通过 | 模拟成功、需要重启、安装取消、安装失败、UAC 取消、校验失败以及启动参数转义 |

Rust 用例具体覆盖：只选择较新的正式版本、拒绝错误的发布响应、匹配官方 MSI 并要求有效校验值、拒绝非预期域名或发布路径、校验下载内容并报告进度、拒绝截断/超长/损坏内容、传播读取和写入错误、GUI 与辅助进程持锁时禁止清理、只删除已知临时文件、丢弃未交接下载时删除安装包。读取中断和磁盘写入失败通过测试对象注入，没有实际断网或填满磁盘。

PowerShell 模拟测试直接执行生产辅助脚本的逻辑，并替换安装器启动及消息框。其成功和错误码是模拟值，不能代替下面的实际 MSI 安装结果。

### 实际 Windows Installer 测试

`tests/update/msi.ps1` 先安装测试基线 `1.0.0`，再依次执行下列四个场景。测试包的载荷为会记录启动版本的 `update-fixture.exe`。每个场景将本地构建的 MSI 放入独立暂存目录，由生产 `install.ps1` 完成校验、ready/go 握手、等待父进程退出、安装、清理和重启；测试包装器只增加 MSI 日志并捕获提示消息，不模拟安装器进程或退出码。

| 场景 | MSI 实际退出码 | 磁盘版本 / 重启版本 | 实际结果 |
| --- | --- | --- | --- |
| `1.0.0` 升级到 `1.0.1` | `0` | `1.0.1.0` / `1.0.1.0` | 升级成功，旧产品注册被移除，临时目录已删除 |
| 升级到 `1.0.2` 时取消 | `1602` | `1.0.1.0` / `1.0.1.0` | Windows Installer 执行回滚，恢复旧版本并重新打开，临时目录已删除 |
| 升级到 `1.0.2` 时失败 | `1603` | `1.0.1.0` / `1.0.1.0` | Windows Installer 执行回滚，恢复旧版本并重新打开，临时目录已删除 |
| 失败后重新升级到 `1.0.2` | `0` | `1.0.2.0` / `1.0.2.0` | 重试成功，旧产品注册被移除，临时目录已删除 |

取消和失败由 MSI 的延迟操作在 `InstallFiles` 之后分别返回 `1602`、`1603` 触发。安装日志确认 Windows Installer 从 `.rbf` 回滚备份恢复可执行文件；这属于真实安装事务的回滚测试，不是手工点击取消按钮。

四个场景均核对通过：自定义安装位置保持一致；重启仍使用同一数据目录；配置文件哈希不变；产品注册状态与成功或回滚结果一致；暂存目录整体不存在，因此其中的 MSI、脚本和握手文件均已删除。测试结束后，测试产品已卸载，测试安装目录、注册表项和快捷方式均不存在，更新暂存目录数量为 0。未删除 Windows Installer 的系统缓存。

### 验证边界

- Windows 10 已做接口和 PowerShell 兼容性核对，尚未在 Windows 10 真机或虚拟机运行上述安装测试。
- 实际 MSI 测试使用独立测试产品和本地构建包，没有覆盖从 GitHub 下载官方发行包、点击真实 MeatShell 界面并重启真实 MeatShell 的完整交互流程。
- 人工拒绝 UAC 和返回 `3010` 的重启提示路径只有模拟测试覆盖；实际 MSI 测试没有触发系统重启。
- 未实测断电、强制结束安装器或杀毒软件持续占用文件等外部中断。文件持锁、校验失败及清理范围的结论来自对应定向用例。

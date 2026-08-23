# 轻量级 Windows 应用基础方案比较

## 可先确定的共同底座

UI 框架不会改变采集能力的来源。三类主候选都能使用同一组 Windows 11 桌面 API：`SetWinEventHook` 可跨进程订阅并按顺序接收窗口事件；Raw Input 的 `RIDEV_INPUTSINK` 可让指定窗口在后台接收键鼠输入；`Shell_NotifyIcon` 提供通知区图标；Desktop Duplication 通过 DXGI surface 提供桌面图像。相比之下，`Windows.Graphics.Capture` 的标准流程会显示选择器和捕获边框，不适合作为 Timelens 默认的静默快照路径。([SetWinEventHook](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwineventhook), [Raw Input](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-rawinputdevice), [Shell_NotifyIcon](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shell_notifyiconw), [Desktop Duplication](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api), [Windows.Graphics.Capture](https://learn.microsoft.com/en-us/windows/uwp/audio-video-camera/screen-capture))

提权采集器与普通权限 UI 也不依赖某一 UI 技术。Windows named pipe 支持相关或无关进程通信，但默认安全描述符会向 Everyone 和匿名用户授予读取，因此实现必须显式使用当前登录 SID/会话的 DACL、拒绝远程访问，并逐条验证命令；不能把“管道可连接”等同于“权限边界安全”。([Named pipes](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipes), [pipe security](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights))

## 候选比较

| 候选 | 采集、快照与托盘 | 提权边界与 IPC | SQLite 与 AI HTTP | 部署与轻量化事实 |
| --- | --- | --- | --- | --- |
| Rust + `windows-rs` + Win32/Slint | Microsoft 的 `windows`/`windows-sys` 可直接调用 Win32、COM、WinRT；Slint 的 UI 标记会提前编译为本机代码。窗口事件、Raw Input、DXGI 和托盘可全部留在 Rust。([windows-rs](https://github.com/microsoft/windows-rs), [Slint](https://github.com/slint-ui/slint)) | 单独 Rust collector EXE 加受限 named pipe；没有额外的 UI-to-native 桥。 | `rusqlite` 可随程序编译 SQLite；`reqwest::Client` 有连接池并建议复用。([rusqlite](https://github.com/rusqlite/rusqlite), [reqwest](https://docs.rs/reqwest/latest/reqwest/struct.Client.html)) | 不必携带 WebView 或 .NET runtime，但最终 EXE、Slint renderer、SQLite、WebP 编码器和安装器的真实大小没有官方保证。Slint 的“lightweight”是设计目标，不是 Timelens 基准结果。**安装体积、内存、GPU/CPU 必须实测。** |
| Tauri 2 | 核心进程是 Rust，可复用上一行的 Windows API；时间轴由 WebView 前端绘制。Tauri 有官方托盘 API，但其 Windows UI 使用 WebView2，且明确是 core + WebView 的多进程模型。采集不应放进前端 JS。([process model](https://v2.tauri.app/concept/process-model/), [tray](https://v2.tauri.app/learn/system-tray/)) | Tauri 自带 IPC 只覆盖 WebView 与 Tauri core 的信任边界；提权 collector 仍需独立 EXE 和 OS named pipe，不能用内置 command IPC 代替。([Tauri IPC](https://v2.tauri.app/concept/inter-process-communication/), [security](https://v2.tauri.app/security/)) | 可在 Rust core 直接用 `rusqlite`/`reqwest`；官方 SQL 插件也支持 SQLite，但把数据库或密钥能力暴露给前端会扩大授权面。([SQL plugin](https://v2.tauri.app/plugin/sql/), [capabilities](https://v2.tauri.app/security/capabilities/)) | Windows 11 已预装 WebView2，Tauri 不把 WebView 动态库放进应用；官方安装器支持 NSIS/WiX，默认 WebView bootstrapper 增量为 0 MB，离线或固定 runtime 会增加约 127/180 MB。由此只能证明 ≤20 MB **有可能**，不能证明进程内存或 CPU 达标。([WebView version](https://v2.tauri.app/reference/webview-versions/), [Windows installer](https://v2.tauri.app/distribute/windows-installer/)) |
| WinUI 3 + .NET | WinUI 3 是 Windows App SDK 的本机桌面 UI；C# 可用 P/Invoke 调用相同 User32/DXGI/Shell API。托盘需走 `Shell_NotifyIcon` 互操作；长期 callback 还必须正确固定，避免 GC 移动或回收。([Windows App SDK](https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/), [P/Invoke](https://learn.microsoft.com/en-us/dotnet/standard/native-interop/pinvoke), [interop practices](https://learn.microsoft.com/en-us/dotnet/standard/native-interop/best-practices)) | collector 可同为 .NET 或另用本机 EXE，并通过受限 named pipe 通信；无功能阻塞，但 managed/native 边界与运行时成本要纳入样机。 | `Microsoft.Data.Sqlite` 是轻量 ADO.NET SQLite provider；`HttpClient` 原生支持 HTTP，官方建议复用长寿命 client。([SQLite](https://learn.microsoft.com/en-us/dotnet/standard/data/sqlite/), [HttpClient](https://learn.microsoft.com/en-us/dotnet/fundamentals/networking/http/httpclient-guidelines)) | Windows App SDK 默认 framework-dependent，官方称其部署较小但要求目标机有 runtime；self-contained 会携带 Windows App SDK 和 .NET 依赖，官方明确输出显著增大。20 MB 目标对 self-contained 方案构成已知风险，framework-dependent 的安装器、runtime 下载量和安装后占用仍要分别量化。([deployment](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/deploy-overview), [unpackaged WinUI](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/unpackage-winui-app)) |
| WPF + .NET（对照候选） | 同样经 P/Invoke 使用采集 API；WPF 提供成熟的 XAML、控件、数据绑定和硬件加速矢量 UI，但不是新的 Windows App SDK UI。([WPF](https://learn.microsoft.com/en-us/dotnet/desktop/wpf/overview/)) | 与 WinUI 3 相同。 | 与 WinUI 3 相同。 | framework-dependent 发布较小但要求已安装匹配的 .NET runtime；self-contained/single-file 会包含 runtime，官方明确其文件较大。它值得作为 .NET 对照样机，仍没有文档能证明 Timelens 的 20 MB/100 MB 指标。([.NET deployment](https://learn.microsoft.com/en-us/dotnet/core/deploying/), [single file](https://learn.microsoft.com/en-us/dotnet/core/deploying/single-file/overview)) |

SQLite 本体通常小于 1 MB，但官方同时说明尺寸随编译器、平台、优化和可选特性变化；这不能替代对各候选最终安装包的测量。([SQLite footprint](https://www.sqlite.org/footprint.html))

## 必须用样机回答的问题

官方资料只能证明可行性和运行时构成，不能证明 CPU ≤0.5%/1%、常驻内存 ≤100 MB、安装包 ≤20 MB。应让三类主候选实现同一条最小竖切，在同一台干净 Windows 11 x64 VM、Release 构建和相同电源计划下各重复至少三次：

1. UI 未启动、collector 记录窗口与键鼠 30 分钟；UI 隐藏到托盘 30 分钟；时间轴加载 10,000 段并连续平移缩放 5 分钟。
2. 在正常输入、1000 Hz 鼠标和每 5 分钟一次 1280 px WebP 快照三种负载下，按 Timelens 全部相关进程合计 CPU、context switch/唤醒、磁盘写入、峰值与稳态内存。
3. 用 WPR/WPA 采集 CPU、磁盘、GPU 和唤醒；内存除 working set/private bytes 外记录 reference set，因为 Microsoft 明确指出瞬时 working set 会受内存压力和系统裁剪影响。([background trace procedure](https://learn.microsoft.com/en-us/windows/apps/develop/performance/power), [reference sets](https://learn.microsoft.com/en-us/windows-hardware/test/wpt/wpa-reference-set))
4. 分别记录安装器字节、首次安装的网络下载、安装后磁盘占用、冷启动时间和进程数。系统 WebView2 可从“安装包 ≤20 MB”中排除，但 WebView2 进程的 CPU/内存不能从运行成本中排除；framework-dependent runtime 也要单列，避免用外置依赖掩盖总交付成本。

## 研究结论

三类主候选都没有功能性阻塞。Rust 原生路线的运行时组成最少但 UI/可访问性与工程成本待证；Tauri 2 能借 Windows 11 自带 WebView2 控制安装包，却有必须实测的多进程 WebView 运行成本；WinUI 3/.NET 拥有官方 Windows UI 栈，但 self-contained 体积已有明确风险，framework-dependent 则转化为 runtime 部署问题。现有一手证据不足以淘汰或选定任何一项，下一步必须以同功能样机的实测数据作架构决策。

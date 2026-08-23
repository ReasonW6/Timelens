# 比较轻量级 Windows 应用基础方案

Type: research
Status: resolved
Blocked by:

## Question

基于官方文档与可验证事实，比较适合 Timelens 的 Windows 11 x64 技术基础，至少覆盖 Rust 加原生 Windows API 与轻量 UI、Tauri 2、WinUI 3/.NET 等合理候选。比较窗口事件与输入钩子、屏幕捕获、托盘、提权采集器和 IPC、SQLite、本地 HTTP AI 客户端、安装包体积及可测资源成本；不得用未实测的性能印象代替事实。

## Answer

三类主候选都能使用相同的 Windows 采集、截图、托盘和受限 named-pipe IPC 原语，没有功能性阻塞。Rust 原生的运行时组成较少，Tauri 可复用 Windows 11 自带 WebView2，WinUI 3/.NET 提供官方 Windows UI 栈；但官方资料无法证明任何一项满足 Timelens 的 CPU、内存和安装体积指标。WinUI self-contained 体积与 Tauri 多进程 WebView 成本分别构成已知待验证风险，最终选型必须由同功能竖切样机在统一 WPR/WPA 夹具中的数据决定。完整证据与基准方案见[研究记录](../research/lightweight-windows-foundations.md)。

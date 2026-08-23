# 核验 Windows 窗口状态的可观测性

Type: research
Status: resolved
Blocked by:

## Question

在 64 位 Windows 11 上，哪些官方 API 能以低开销可靠观察用户顶层窗口的创建、销毁、显示、隐藏、最小化、cloaking、焦点变化、虚拟桌面归属，以及锁屏、睡眠、恢复和正常关机？如何在不把进程变成用户领域对象的前提下，将窗口归组为应用，并延续隐藏到通知区域后的应用会话？必须区分官方保证、合理推断和只能通过原型验证的行为。

## Answer

- **官方保证**：用 out-of-context `SetWinEventHook` 接收 create/destroy/show/hide/minimize/cloak/foreground 增量，以 `EnumWindows`、`IsIconic`、`DWMWA_CLOAKED`、`GetForegroundWindow` 和 `IVirtualDesktopManager` 查询当前事实；用 WTS、suspend/resume 通知与 `WM_ENDSESSION` 记录锁屏、电源及正常会话结束。Windows 只保证一个 foreground HWND，且正常结束时不能区分关机与重启。
- **合理推断**：采用事件驱动加启动/恢复/低频对账；用户领域只保留应用和窗口，进程仅作为 HWND→路径/AppUserModelID 解析及已出现窗口应用的托盘退出传感器。隐藏、最小化、cloak 继续计后台；同一路径先归组，显式 AppUserModelID 只作跨路径提示。
- **原型门槛**：WinEvent 对各 UI 框架的完整性、虚拟桌面切换触发、分屏/遮挡的多窗口“显示中”估算、宿主/打包应用归组、托盘移交辅助进程、Modern Standby 与强制结束缺口，以及最终对账周期和资源开销。公开 API 无法保证精确遮挡或任意应用的通知区域归属。

完整证据、限制与测试矩阵见[研究记录](../research/windows-window-observability.md)。

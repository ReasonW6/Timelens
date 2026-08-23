# Windows 11 窗口状态可观测性

## 结论

V1 应采用“事件驱动 + 状态查询 + 低频对账”：交互用户会话中的采集器用 `SetWinEventHook(..., WINEVENT_OUTOFCONTEXT)` 订阅窗口事件，并在启动、解锁、恢复及低频看门狗时用 `EnumWindows` 重建快照。Microsoft 保证 out-of-context 事件异步排队且顺序交付，但也要求接收线程有消息循环，并提醒应对具体 UI 元素实测实际产生的 WinEvent；因此 WinEvent 不能单独充当无缺口审计日志。([SetWinEventHook](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwineventhook), [Event constants](https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants), [EnumWindows](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-enumwindows))

## 官方可保证的观测面

| 状态 | V1 使用的公开 API | 保证与边界 |
| --- | --- | --- |
| 创建、销毁、显示、隐藏 | `EVENT_OBJECT_CREATE/DESTROY/SHOW/HIDE`；只接受 `OBJID_WINDOW/CHILDID_SELF`，再以 `EnumWindows` 对账 | USER 为标准 HWND 元素提供 WinEvent，但自定义 UI 的事件完整性仍需验证；Windows 8 以后 `EnumWindows` 文档只保证枚举桌面应用顶层窗口。([WinEvent generation](https://learn.microsoft.com/en-us/windows/win32/winauto/generating-appropriate-winevents), [EnumWindows](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-enumwindows)) |
| 最小化 | `EVENT_SYSTEM_MINIMIZESTART/END`，并以 `IsIconic` 查询当前值 | `IsIconic` 明确定义为窗口是否最小化；事件只作增量触发，查询值作事实源。([Event constants](https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants), [IsIconic](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-isiconic)) |
| cloaking | `EVENT_OBJECT_CLOAKED/UNCLOAKED`，并以 `DwmGetWindowAttribute(DWMWA_CLOAKED)` 查询 | 可区分 app、Shell 和继承导致的 cloak；被 cloak 的窗口仍存在但对用户不可见。([Event constants](https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants), [DWMWA_CLOAKED](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/ne-dwmapi-dwmwindowattribute)) |
| 活动窗口/焦点切换 | `EVENT_SYSTEM_FOREGROUND`，并以 `GetForegroundWindow` 查询 | Windows 只给出一个“用户当前正在操作”的 foreground HWND，切换瞬间可为 `NULL`。这不等于分屏中所有可见窗口。([GetForegroundWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getforegroundwindow)) |
| 虚拟桌面归属 | `IVirtualDesktopManager::GetWindowDesktopId` 与 `IsWindowOnCurrentVirtualDesktop` | 可查询顶层窗口所属 GUID 及是否位于当前虚拟桌面；公开接口只有查询/移动方法，没有切换通知。([IVirtualDesktopManager](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nn-shobjidl_core-ivirtualdesktopmanager), [GetWindowDesktopId](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ivirtualdesktopmanager-getwindowdesktopid)) |
| 锁屏/解锁 | 隐藏顶层接收窗调用 `WTSRegisterSessionNotification(NOTIFY_FOR_THIS_SESSION)`，处理 `WTS_SESSION_LOCK/UNLOCK` | 通知带 session ID；若注册早于 TermService 依赖就绪，须等待 `Global\\TermSrvReadyEvent` 后重试。([WTSRegisterSessionNotification](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsregistersessionnotification), [WM_WTSSESSION_CHANGE](https://learn.microsoft.com/en-us/windows/win32/termserv/wm-wtssession-change)) |
| 睡眠/恢复 | `RegisterSuspendResumeNotification` 或 `PowerRegisterSuspendResumeNotification`，处理 `PBT_APMSUSPEND`、`PBT_APMRESUMEAUTOMATIC`、`PBT_APMRESUMESUSPEND` | 官方提供窗口或回调两种订阅；自动唤醒不表示用户已返回。Modern Standby 会暂停桌面应用，Windows 为选择接收的桌面进程提供同类 suspend/resume 通知。([RegisterSuspendResumeNotification](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registersuspendresumenotification), [PBT_APMRESUMEAUTOMATIC](https://learn.microsoft.com/en-us/windows/win32/power/pbt-apmresumeautomatic), [Desktop Activity Moderator](https://learn.microsoft.com/en-us/windows/win32/w8cookbook/desktop-activity-moderator)) |
| 正常会话结束 | 隐藏顶层接收窗处理 `WM_QUERYENDSESSION`，快速返回；仅在 `WM_ENDSESSION(wParam=TRUE)` 写入最终标记 | `lParam=0` 时官方明确无法区分关机与重启，故标记应叫“系统关机/重启”。消息专用窗口不接收广播，不能用 `HWND_MESSAGE`；强制关机仍可能直接终止进程，所以数据要平时落盘。([WM_QUERYENDSESSION](https://learn.microsoft.com/en-us/windows/win32/shutdown/wm-queryendsession), [Shutting down](https://learn.microsoft.com/en-us/windows/win32/shutdown/shutting-down), [Message-only windows](https://learn.microsoft.com/en-us/windows/win32/winmsg/window-features)) |

`IsWindowVisible` 只证明窗口及其祖先带 `WS_VISIBLE`，即使被其他窗口完全遮住也可能返回真。因此 Timelens 可以保证“活动窗口”这一单值事实，却不能用公开低开销 API 保证“屏幕上实际露出了哪些窗口”。分屏的多窗口前台应建模为推断状态：窗口已显示、未最小化、未 cloak、位于当前虚拟桌面且框架矩形与显示器相交；不要声称已精确计算遮挡。([IsWindowVisible](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-iswindowvisible))

## 应用归组与通知区域连续性

官方能把 HWND 映射到创建它的 PID，再以有限查询权限取得可执行文件完整路径；显式 `AppUserModelID` 可把不同进程和窗口关联到同一应用，但它是可选的，Microsoft 明确说明系统按启发式生成的内部 AppUserModelID 无法读取。因此 Windows 没有对任意经典、打包和宿主应用都正确的“应用身份”保证。([GetWindowThreadProcessId](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getwindowthreadprocessid), [QueryFullProcessImageName](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-queryfullprocessimagenamew), [AppUserModelIDs](https://learn.microsoft.com/en-us/windows/win32/shell/appids), [SHGetPropertyStoreForWindow](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shgetpropertystoreforwindow))

V1 的合理推断是：用户领域仍只有“应用”和“窗口”；内部传感层以 `(PID, process creation time)` 防 PID 复用，同一规范化可执行路径先归为一张应用卡片，显式窗口/进程 AppUserModelID 只作为跨路径归组提示，最终规则由后续归组票决定。窗口 `HIDE`、最小化或 cloak 后 HWND 仍存在时，应用会话继续并记为后台；若最后一个已追踪 HWND 被销毁，可只为“曾产生用户窗口”的所属进程保留一个不可见的退出等待。进程终止时其内核对象变为 signaled，`RegisterWaitForSingleObject` 可低开销结束该推断会话。进程传感器不展示、不形成独立记录。([GetProcessTimes](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesstimes), [Terminating a process](https://learn.microsoft.com/en-us/windows/win32/procthread/terminating-a-process), [RegisterWaitForSingleObject](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-registerwaitforsingleobject))

这仍不是“通知区域图标存在”的保证。Microsoft 公开的 Shell 通知区域 API 面向图标所有者执行添加、修改和删除，并未提供外部观察者的归属通知/枚举合同；若应用销毁 UI 进程并把托盘职责移交给另一个辅助可执行文件，Timelens 无法仅凭公开窗口 API 可靠延续会话。([The Taskbar](https://learn.microsoft.com/en-us/windows/win32/shell/taskbar))

## 只能由原型验收的项目

1. 在 Windows 11 当前支持版本上，以 Win32、WPF、WinUI 3、UWP/宿主窗口、Electron、Chromium、Qt、管理员窗口和受保护系统窗口组成矩阵，核对每种 create/destroy/show/hide/minimize/cloak/foreground 事件与 `EnumWindows` 快照；验证回调重入、HWND/PID 复用和监控重启修复。
2. 切换、创建、删除虚拟桌面并移动窗口，验证是否伴随 cloak 或 `EVENT_SYSTEM_DESKTOPSWITCH`；公开文档没有保证二者代表 Windows 虚拟桌面切换，生产逻辑必须以 `IVirtualDesktopManager` 重查为准。
3. 验证分屏、完全/部分遮挡、透明/置顶窗口、最小化和其他虚拟桌面；若产品把“前台”定义为多窗口可见集合，必须在规格中标注为估算，或改名为“显示中”。
4. 覆盖“隐藏 HWND”“销毁 HWND 但原进程存活”“托盘移交辅助进程”三类应用；最后一类若无稳定显式 AppUserModelID，应结束窗口会话或标为不确定，不得猜测归属。
5. 实机验证锁定、传统 S3/S4、Modern Standby、自动唤醒、正常关机/重启、强制结束和掉电；恢复后对账并把不可确认的区间记为“监控中断”，不补猜窗口关闭时间。
6. 用事件速率、对账周期、CPU、内存和句柄数实测确定看门狗频率；“WinEvent + 30–60 秒对账”只是起始候选，不是已验证性能结论。

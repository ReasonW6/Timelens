# 校准窗口状态分类器

Type: task
Status: resolved
Blocked by: 05, 06

## Question

用聚焦的 Windows 11 观测工具覆盖 Win32、WPF、WinUI 3、打包应用、Electron/Chromium、Qt、提权窗口、分屏、遮挡、虚拟桌面、最小化、托盘、锁屏、睡眠恢复和监控重启，并覆盖显式/缺失 AppUserModelID、路径变更、多可执行文件窗口及关闭窗口后进程滞留。核对 WinEvent 增量、身份归组、公开状态查询与低频对账是否能稳定实现已决定的显示、聚焦、后台和会话边界；记录无法保证的应用类别与降级规则，为正式实现提供可验证的支持矩阵。

## Answer

- 已构建不读取窗口标题的 Rust 校准观测器、Win32 多窗口夹具和 WPF 夹具，并复用项目现有 WinUI 3 样机。Win32、WPF、WinUI 3 的 create/show/hide/destroy、foreground、minimize start/end 与定期对账均在当前 Windows 11 主机通过；打包应用、Electron/Chromium、Qt、Gecko 和宿主窗口完成本机快照覆盖。
- 分类器必须分成“结构候选、用户窗口 onboarding、已追踪状态”三层。create 或宽松顶层样式不能建立会话；窗口至少一次成为可见、有效矩形、可切换且不是当前桌面 cloaked 占位窗后才可 onboarding。进入后最小化、隐藏、cloak 和其他虚拟桌面继续后台，最后 HWND 销毁后只跟随同一 `(PID, process creation time)` 的原主进程。
- 应用身份按窗口显式 AppUserModelID、可读进程/包 AUMID、包身份、规范化路径降级。经典应用仅调用进程显式 AUMID 在本机不能被外部观测器读到；写入窗口 property store 后可稳定跨路径归组。两个进程、四个重叠主窗可归为一个 identity，同时最多四个 displayed、一个 focused，应用时长仍取并集。
- 启动时窗口已显示可由首次 `EnumWindows` 在约 11ms 修复；所有用户窗已隐藏时，仅靠公开快照无法分辨托盘应用和从未展示的辅助 HWND。正式核心必须把已知活动进程键和身份回种给重启采集器；没有种子就等待真实 show，不作猜测。
- WTS 与 suspend/resume 通知注册成功，但没有擅自触发锁屏、睡眠、关机、第二虚拟桌面或额外 UAC。提权/受保护窗口、跨虚拟桌面、真实锁屏/睡眠/重启列为正式里程碑验收项，并已有明确的未知状态与不归组降级规则。

完整算法、支持矩阵、限制、原始 JSONL 与 SHA-256 见[本机校准报告](../calibration/window-classifier/support-matrix.md)。

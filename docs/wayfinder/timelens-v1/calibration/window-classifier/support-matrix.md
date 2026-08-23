# Windows 11 窗口状态分类器本机校准报告

## 结论

Timelens V1 可以采用“WinEvent 只作增量触发、公开查询作当前事实源、启动/恢复/低频对账修复缺口”的方案，但必须增加一个有历史的用户窗口 onboarding 门槛。只按顶层关系、owner 和扩展样式判断会误收大量 WPF/WinForms/Electron/系统隐藏消息窗；窗口只有至少一次成为可见、有效矩形、可切换的用户表面后，才可建立窗口和应用会话。进入会话后，最小化、隐藏、cloak、其他虚拟桌面和最后窗口销毁后的同一主进程滞留都可继续记后台。

最终全桌面 3 秒基线包含 1,149 个 HWND 快照，其中 411 个满足宽松结构条件；加入可见历史、有效矩形、`WS_EX_NOACTIVATE` 和当前桌面 cloak 门槛后，只有 ChatGPT 与 Zen 的 2 个真实用户窗口被 onboarding，1,143 个快照保持 `ignored`。这证明窗口类白名单不是必要条件，也证明“只记录进程”或“枚举所有顶层 HWND”都不符合产品语义。

## 校准环境与工具

- Windows 11 Pro 25H2，build 26200.8246，AMD64。注册表兼容字段仍显示 `Windows 10 Pro`，报告以 build 和 DisplayVersion 为准。
- Rust 1.94.1，`windows` 0.62.2，外加 Windows 自带 .NET Framework WPF 夹具。
- 观测器在交互用户会话运行；WinEvent 回调只入队，窗口查询和 JSONL 写入在消息循环线程完成。
- 已成功安装 4 组 out-of-context WinEvent hook；WTS 当前会话通知和 suspend/resume 通知均注册成功。
- 观测器从不读取窗口标题。报告中的应用类别来自可执行路径、窗口类和公开 Windows 身份。

## 已锁定的分类算法

### 1. 结构候选

一个 HWND 只有同时满足以下条件，才进入“可能是用户窗口”的候选集：

- `GetAncestor(hwnd, GA_ROOT) == hwnd`；
- 无 owner，或明确带 `WS_EX_APPWINDOW`；
- 不带 `WS_EX_TOOLWINDOW`，除非同时明确带 `WS_EX_APPWINDOW`；
- 不带 `WS_EX_NOACTIVATE`。

结构候选不是会话。WPF 动态夹具在显示主窗前创建了 6 个同类 `HwndWrapper`，它们全部满足宽松结构条件，但起初不可见；因此 create 事件绝不能直接创建应用卡片。

### 2. 用户窗口 onboarding

首次建立窗口会话需要结构候选同时满足：

- `IsWindowVisible == true`；
- 窗口矩形有正面积；
- `MonitorFromWindow(..., MONITOR_DEFAULTTONULL)` 非空；
- 当前没有被 DWM cloak，或明确位于其他虚拟桌面，或它就是当前 foreground HWND。

当前桌面上已 cloak 的打包/系统占位窗不能在采集器冷启动时新开会话。其他虚拟桌面的已知窗口允许 onboarding，是因为产品明确把它记为后台；此分支仍需有第二虚拟桌面的实机验收。

### 3. 已追踪窗口的事实状态

onboarding 后，窗口在 HWND 销毁前持续受追踪：

- `focused = hwnd == GetForegroundWindow()`；全系统最多一个；
- `displayed = visible && !minimized && !cloaked && on_current_desktop && monitor_present && positive_rect`；
- 其他状态为窗口后台，包括最小化、隐藏、cloak 和其他虚拟桌面；
- 完全或部分遮挡不改变 `displayed`，因为 V1 不做像素遮挡计算。

调试输出的 `classifierState` 使用 `focused > displayed > background > ignored` 的显示优先级。正式数据必须保存独立的 displayed 集合和唯一 focused 身份；focused 窗口通常也同时属于 displayed 集合。

### 4. 应用级聚合

- 应用“打开”是至少一个已追踪窗口存在，或最后窗口销毁后同一 `(PID, process creation time)` 的原主应用仍存活；
- 应用“显示中”是其任一已追踪窗口 displayed；
- 应用“聚焦中”是 foreground HWND 归属该应用；
- 应用“后台”是会话仍打开但没有 displayed 窗口，或系统已锁定；
- 同一应用多个窗口和进程的区间取并集，不按窗口或进程数重复累计。

## 受控场景结果

| 场景 | 本机结果 | 生产约束 | 权威证据 |
| --- | --- | --- | --- |
| Win32 双主窗、tool、owned 窗 | 2 个主窗 onboarding；tool 与 owned 窗持续 ignored | 只展示可独立切换用户窗 | `fixture-classifier-v2.jsonl` |
| 最小化、恢复、隐藏、再显示 | start/end、show/hide 均到达；查询值与状态一致 | 事件只触发重查，`IsIconic`/visible 是事实源 | `fixture-classifier-v2.jsonl` |
| 最后窗口销毁、进程滞留 | 连续 9 次判为 inferred tray background，进程退出后形成 1 次 exit | 只延续曾产生用户窗口的原主进程 | `fixture-classifier-v2.jsonl` |
| 无窗口辅助进程 | 仅 observer started/stopped 两行；0 快照、0 会话、0 托盘延续 | 从未产生用户窗口的进程永不记录 | `fixture-hidden-only.jsonl` |
| 仅设置进程显式 AUMID | 外部观测器读不到 process/window AUMID，正确退回路径键 | 不能假定经典应用的进程显式 AUMID可从外部读取 | `fixture-process-aumid-only.jsonl` |
| 窗口属性显式 AUMID | 读取到 `Timelens.Calibration.Sample`，身份键改为 AUMID | 优先读 HWND property store 的 `PKEY_AppUserModel_ID` | `fixture-window-aumid.jsonl` |
| 缺失 AUMID | 使用规范化完整路径键 | 禁止按名称、图标、类名模糊合并 | `fixture-no-aumid.jsonl` |
| 路径变化但窗口 AUMID相同 | 原路径和 copy 路径得到相同 AUMID 身份 | 明确 AUMID 可跨路径归组 | `fixture-copy-aumid.jsonl` |
| 两进程、每进程两主窗、完全重叠 | 2 PID、4 HWND、1 identity；最多同时 4 displayed、1 focused | 多窗可重叠，应用时长取并集 | `fixture-multiprocess-overlap.jsonl` |
| 观测器在窗口已显示时启动 | 没有 create 事件，首次对账约 11ms 即 onboarding | 启动、解锁、恢复必须立即 EnumWindows | `restart-while-visible.jsonl` |
| 观测器在全部用户窗已隐藏时启动 | show 前 14 个隐藏结构快照全部 ignored；首次 show 约 1314ms 才 onboarding | 核心必须把已知活动 `(PID, creation time, identity)` 回种给重启采集器；无种子不得猜托盘归属 | `restart-while-all-windows-hidden-v2.jsonl` |
| WPF 动态窗口 | 6 个隐藏 HwndWrapper ignored；真正显示的主窗被追踪；minimize/hide/show/destroy 和进程滞留均通过 | onboarding 必须有可见历史，不能按框架类名收全 | `wpf-fixture.jsonl` |
| WinUI 3 动态窗口 | create/show/foreground、minimize start/end、恢复和 destroy 均通过；最小化期每次 200ms 对账稳定为 background | WinUI 3 无需专用传感器 | `winui3-spike-v2.jsonl` |
| 当前桌面全量基线 | 1,149 快照最终只 onboarding 2 个真实用户窗口、2 个应用身份 | 默认忽略未 onboarding 的隐藏消息窗和当前桌面 cloaked 占位窗 | `baseline-classifier-final.jsonl` |

## 框架与应用类型支持矩阵

| 类型 | 覆盖级别 | 结果与限制 |
| --- | --- | --- |
| Win32 | 受控动态通过 | create/destroy/show/hide/minimize/foreground、双主窗、owned/tool、进程滞留均覆盖。 |
| WPF | 受控动态通过 | 多个隐藏 `HwndWrapper` 证明 onboarding 门槛必要；主窗口状态链通过。 |
| WinUI 3 | 受控动态通过 | 项目现有 WinUI 3 样机完成创建、显示、最小化、恢复和关闭。 |
| 打包应用/宿主窗口 | 本机快照通过 | ChatGPT、Settings、ApplicationFrameHost 等可取得包身份或窗口 AUMID；宿主 HWND 只有读到真实窗口 AUMID 才可归到被宿主应用。无 AUMID 的 shell frame 不得归到 `explorer.exe` 卡片。 |
| Electron/Chromium/WebView2 | 本机快照通过 | ChatGPT、Typeless、Steam WebHelper 和 WebView2 的 Chrome 类窗口均可查询；隐藏 crashpad、tray host 和 power message HWND 不 onboarding。未逐一操控用户现有应用。 |
| Qt | 本机快照通过 | GameViewer 的 `Qt51512QWindowIcon` 与 PixPin 的 Qt tray 消息窗被观测；只有真实显示的用户窗可 onboarding，tray 消息窗本身不能成为卡片。未操控用户现有 Qt 应用。 |
| Firefox/Gecko | 额外快照通过 | Zen 的 `MozillaWindowClass` 被正确 onboarding，隐藏 Mozilla helper HWND ignored。 |
| 提权/受保护窗口 | 部分覆盖 | 普通权限观测器对若干高完整性/系统 HWND 可查 class/state，但路径查询失败；没有为了校准额外触发 UAC。正式最高权限采集器需复测，受保护系统窗口仍可能只有状态没有身份，此时记录诊断而不创建未知应用卡片。 |
| 分屏与遮挡 | 语义通过 | 同屏多窗和完全重叠时多个 displayed 可同时为真，focused 始终最多一个；不计算真实露出像素。 |
| 虚拟桌面 | 查询面通过，跨桌面未实测 | COM manager 创建成功，当前桌面查询和 desktop GUID 可读；本机测试期间没有第二桌面，未验证移动/切换。查询失败时状态标未知并在下次事件/对账重试，不伪造“当前桌面”。 |
| 托盘 | 同进程推断通过 | 隐藏 HWND 持续后台；最后 HWND 销毁后仅跟随原主进程。公开 API 不能可靠观察任意第三方 tray 图标或跨辅助进程移交。 |
| 锁屏、睡眠、恢复 | 注册通过，真实转换未执行 | WTS 与 suspend/resume 注册成功。为避免中断当前任务，没有擅自锁屏或睡眠；发布前必须实机验收。收到锁屏时所有打开应用转后台，睡眠时冻结计时，恢复/解锁立即对账。 |
| 正常关机/重启 | 接收窗就绪，真实关机未执行 | 隐藏顶层接收窗可处理 session-end；没有擅自关机。正式标记只能叫“系统关机或重启”，强制断电尾部标监控中断。 |

## 关键发现与降级规则

1. **WinEvent 不是事实本身。** out-of-context foreground 事件可能排队到达；处理时当前 foreground 已再次变化。回调只提供“该重查了”的信号，所有区间边界使用处理时公开查询结果和单调时钟。
2. **create 不能建立用户会话。** Win32 create 时窗口尚不可见；WPF 还会创建多个永不显示的同类 HWND。只有 onboarding 成功才能形成用户窗口。
3. **destroy 查询不到旧身份。** HWND 销毁后快照通常不可得；传感层必须在此前保存 `HWND -> process key -> identity`，destroy 只关闭已有映射。
4. **经典进程显式 AUMID 不保证外部可读。** 本夹具调用 `SetCurrentProcessExplicitAppUserModelID` 后，外部 `GetApplicationUserModelId` 和窗口 property store 仍为空；显式写到窗口 property store 后才稳定可读。读取顺序应为窗口 AUMID、可读进程/包 AUMID、包身份、规范化路径。
5. **当前桌面 cloaked 占位窗不能冷启动 onboarding。** Settings、InputHost 和旧 ApplicationFrame 会保留 visible style 与有效矩形但被 DWM cloak；若没有旧会话，必须忽略。已知活动会话可以在 cloak 后继续后台。
6. **完全隐藏后的采集器重启需要种子。** 仅靠公开窗口快照无法分辨“此前真实用户窗隐藏到托盘”和“从未展示的辅助消息窗”。核心持有的活动会话与进程创建时间必须在握手时回种；没有种子就等下次真实 show。
7. **宿主进程不得成为应用身份。** ApplicationFrameHost、RuntimeBroker、WebView helper 等只有在窗口/包身份指向真实应用时才归组；无明确身份时忽略宿主窗口，不生成宿主应用卡片。
8. **公开状态不等于像素可见。** `displayed` 是低开销估算。被完全遮挡但未最小化的窗口仍 displayed，符合已确认的产品语义。

## 正式实现验收项

以下项目不阻止实现，但必须在首个可运行里程碑或发布候选上完成，不得把本报告的“注册成功”升级描述成“场景通过”：

- 在第二虚拟桌面移动窗口，验证 `IsWindowOnCurrentVirtualDesktop=false`、cloak 与对账边界；
- 用正式最高权限采集器观测一个明确的高完整性测试窗口，并确认路径/AUMID 查询；
- 实机锁定/解锁一次，传统睡眠或 Modern Standby 恢复一次；
- 正常重启一次，确认最终标记、恢复对账和 SQLite 尾部；
- 用正式核心回种一个已全部隐藏的活动会话，验证采集器重启后立即恢复后台而不等待 show；
- 在性能票 10 中以生产候选的 30–60 秒对账周期测 CPU、内存、句柄和事件突发，不沿用本报告的 200ms 校准周期。

原始本机证据的历史字节数及 SHA-256 见 [local-evidence-manifest.txt](local-evidence-manifest.txt)。这些 JSONL 因含本机应用路径和窗口结构未进入 Git；结论、可复现源码与哈希均已保留。官方 API 保证与引用见 [Windows 11 窗口状态可观测性研究](../../research/windows-window-observability.md)。

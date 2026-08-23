# Windows 11 快照采集边界

## 结论

Timelens V1 应把快照定义为“当前已解锁交互会话中某一显示器当时呈现的合成画面”，而不是窗口内容取证。锁屏、安全桌面、睡眠、会话断开时跳过并记录原因；受保护或主动排除捕获的内容允许缺失。多显示器必须逐显示器采集，“活动显示器”则是 Timelens 自己的选择策略，不是捕获 API 提供的概念。

监视器快照应先原型比较 `IDXGIOutput5::DuplicateOutput1` 与 Windows Graphics Capture（WGC）。桌面复制更贴近“静默的显示器快照”，WGC 默认有系统捕获边框，只有取得一次明确同意并满足清单能力要求后才能请求无边框。两者都没有微软给出的 CPU、内存或单帧延迟上限，不能仅凭文档宣称达到 Timelens 的轻量指标。

## 官方保证

| 边界 | Windows Graphics Capture | Desktop Duplication |
| --- | --- | --- |
| 目标与多显示器 | WGC 获取一个显示器或应用窗口；Win32 互操作可按 `HMONITOR` 或 `HWND` 创建单个目标。多显示器需为各目标分别建会话。[Screen capture](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture) · [CreateForMonitor](https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nf-windows-graphics-capture-interop-igraphicscaptureiteminterop-createformonitor) · [CreateForWindow](https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nf-windows-graphics-capture-interop-igraphicscaptureiteminterop-createforwindow) | 按输出/显示器边界复制；完整多屏桌面必须为每个活动输出各建一个 duplication，接口不负责跨输出同步。[Desktop duplication](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/desktop-duplication-api) · [DuplicateOutput](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput) |
| 同意与提示 | Picker 路径由用户在安全系统 UI 中选目标，捕获时系统为每个目标画黄色边框。请求无边框会再次显示同意提示，且包清单必须声明 `graphicsCaptureWithoutBorder`；程序化 `TryCreateFromWindowId` 也要求先请求 Programmatic 访问并声明相应能力。[Screen capture](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture) · [IsBorderRequired](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired) · [TryCreateFromWindowId](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscaptureitem.trycreatefromwindowid) | 官方 DXGI 契约没有 Picker、同意提示或捕获边框流程；它只校验调用者能否访问当前桌面图像。[DuplicateOutput](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput) |
| 锁屏与安全桌面 | WGC 文档不保证锁屏或安全桌面可捕获；其 OneCore Capture Service 是按登录用户创建的 per-user service。[Per-user services](https://learn.microsoft.com/en-us/windows/application-management/per-user-services-in-windows) | 桌面切换会使现有 duplication 失效；只有以 `LOCAL_SYSTEM` 运行的应用才有权访问安全桌面。会话断开会返回 `DXGI_ERROR_SESSION_DISCONNECTED`。[IDXGIOutputDuplication](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nn-dxgi1_2-idxgioutputduplication) · [DuplicateOutput](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput) |
| 受保护内容 | 应用可用 `WDA_EXCLUDEFROMCAPTURE` 让顶层窗口不出现在公共捕获结果中；该机制不是 DRM 保证。[SetWindowDisplayAffinity](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity) | 微软明确说明桌面复制会防止访问受保护视频内容。[Desktop duplication](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/desktop-duplication-api) |
| HDR | Windows HD Color 下使用 BGRA8 可能出现过度裁剪/泛白；微软建议全链路用 `R16G16B16A16_FLOAT`，再按需要保存 HDR 或做 HDR→SDR 色调映射。[Screen capture](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture) | 旧 `DuplicateOutput` 总是给 BGRA8；`DuplicateOutput1` 可请求高色深 scan-out 格式、避免转换并保留高色域，但输出 WebP 前仍需由应用完成 SDR 映射。[Desktop Duplication API](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api) · [DuplicateOutput1](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_5/nf-dxgi1_5-idxgioutput5-duplicateoutput1) |
| 性能 | 帧可在后台线程处理，但官方没有量化资源保证。[Screen capture](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture) | 帧留在 GPU，并提供 dirty rect、移动区域和光标元数据供优化；这说明其设计支持低开销处理，不构成 Timelens 资源预算保证。[Desktop duplication](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/desktop-duplication-api) |

## 合理推断

- “活动显示器”可定义为唯一聚焦窗口与显示器相交面积最大的那个：`GetForegroundWindow` 给出用户正在操作的窗口，`MonitorFromWindow` 给出交叠面积最大的显示器；分屏时仍只有一个该策略下的活动显示器。[GetForegroundWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getforegroundwindow) · [MonitorFromWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-monitorfromwindow)
- 显示器捕获只代表当时的合成桌面，因此被别的窗口遮挡或已最小化的窗口内容不会独立出现。WGC 的窗口目标有微软工具文档所述的“被遮挡仍可捕获”实现，但这不是 WGC API 对所有应用与驱动的正式契约；最小化、隐藏到托盘后的结果更没有契约保证。[WinApp CLI UI automation](https://learn.microsoft.com/en-us/windows/apps/dev-tools/winapp-cli/ui-automation)
- 计划任务中的“最高权限”管理员令牌不是 `LOCAL_SYSTEM`，所以不能据此越过安全桌面边界；Timelens 也不应尝试这样做。
- DXGI 文档把远程桌面访问列为用途，且定义了会话断开错误，但没有保证“在任意已连接 RDP/RemoteApp/VM 会话内”都能捕获。WGC 也只要求运行时检查 `GraphicsCaptureSession.IsSupported()`，因此远程会话支持不能从文档推定。[Desktop duplication](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/desktop-duplication-api) · [Screen capture](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture)

## 必须原型验证

1. 在支持的 Windows 11 版本上比较 WGC 与 `DuplicateOutput1` 的首帧延迟、创建/销毁开销、空闲 CPU、峰值 CPU、GPU/内存，以及每 5 分钟单帧采集是否出现可见边框。
2. 覆盖单屏、不同 DPI/旋转的双屏、跨屏聚焦窗口、显示器热插拔；明确无聚焦窗口时回退到主显示器还是光标所在显示器。
3. 覆盖窗口部分/完全遮挡、最小化、托盘隐藏、独占全屏、HDR→SDR、`WDA_EXCLUDEFROMCAPTURE` 与受保护视频，检查结果是旧帧、黑块、透明、缺失还是错误。
4. 覆盖锁屏/解锁、UAC 安全桌面、睡眠/恢复、快速用户切换、RDP 连接/断开/重连和 VM；预期产品行为均为“跳过并记录状态”，不得保存旧帧冒充新快照。
5. 分别验证普通权限 UI、同用户最高权限采集器与打包/未打包部署；尤其确认 WGC Programmatic/Borderless 能力的清单、一次同意持久性和拒绝后的降级路径。

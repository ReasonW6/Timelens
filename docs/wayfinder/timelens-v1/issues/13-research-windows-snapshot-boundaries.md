# 核验 Windows 快照采集边界

Type: research
Status: resolved
Blocked by:

## Question

基于 Microsoft 官方文档，Windows 11 x64 的 Windows Graphics Capture、桌面复制等公开方案分别能否低开销截取活动显示器、多显示器、普通桌面、锁屏、安全桌面、最小化或被遮挡窗口？说明用户同意、黄色边框、受保护内容、HDR、远程桌面和权限级别的约束，并区分官方保证、合理推断与必须实测的行为，为“定义快照与窗口状态的边界”提供事实。

## Answer

快照的可靠产品边界应限于当前已解锁交互会话中的显示器合成画面：多屏须逐显示器采集，“活动显示器”由 Timelens 另行定义；锁屏、安全桌面、睡眠和断开的远程会话应跳过并记录原因，受保护或排除捕获的内容允许缺失。管理员“最高权限”不是 `LOCAL_SYSTEM`，不能越过安全桌面。

WGC 默认显示系统捕获边框，无边框和程序化路径涉及一次用户同意及包清单能力；Desktop Duplication 没有文档化的 Picker/边框流程，且 `DuplicateOutput1` 更适合先验证显示器级静默单帧，但两者都没有量化轻量保证。HDR 必须按高色深采集并在写入 SDR WebP 前做色调映射。最小化/遮挡、RDP/VM、能力同意持久性和资源预算必须由原型覆盖，详见[研究记录](../research/windows-snapshot-boundaries.md)。

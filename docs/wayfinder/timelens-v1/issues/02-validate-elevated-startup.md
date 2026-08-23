# 验证免重复 UAC 的提权采集启动方案

Type: research
Status: resolved
Blocked by:

## Question

Windows 与 Wallpaper Engine 的官方资料分别支持什么启动机制？Timelens 如何在安装时一次授权、日常不重复弹 UAC的前提下，让窄职责采集器在交互用户会话中以所需权限启动，同时让 UI 与 AI 网络代码保持普通权限？需要记录安全边界、IPC 限制和仍须通过原型验证的风险。

## Answer

Wallpaper Engine 的“高优先级启动”由 Windows 服务实现，但官方并未证明整个交互应用持续提权。Timelens V1 采用权限分离：安装时一次 UAC 注册“登录时触发、以最高权限运行”的窄职责采集任务，UI、报告与 AI 网络代码保持 `asInvoker`；两者仅通过带 ACL 的受限本机 IPC 通信。服务仅在以后确需登录前生命周期、跨会话协调或守护恢复时再引入。普通/提权应用、锁屏、睡眠恢复、安全桌面和多会话覆盖仍须原型验证。证据与安全边界见 [研究笔记](../research/wallpaper-engine-elevated-startup.md)。

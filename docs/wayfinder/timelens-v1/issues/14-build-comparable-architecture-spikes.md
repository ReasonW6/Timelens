# 构建同功能架构样机并统一测量

Type: task
Status: resolved
Blocked by:

## Question

分别构建可丢弃的 Rust + windows-rs + Slint、Rust + windows-rs + Tauri 2、WinUI 3 + .NET 最小竖切样机，使三者执行相同的窗口事件采集、分钟输入聚合、SQLite 写入、10,000 段时间轴平移缩放、托盘隐藏和单帧显示器快照。在同一干净 Windows 11 x64 VM、相同电源计划和 Release 配置下重复测量全部相关进程的 CPU、内存、唤醒、磁盘写入、进程数、安装器体积、首次安装下载、安装后占用、冷启动和时间轴响应；记录源码、构建方式、硬件与原始测量结果。任务只产出选型证据，不演变为正式产品实现。

## Answer

三套同功能可丢弃样机、共享 Windows 工作负载驱动器、统一测量脚本和相同 IExpress 安装器外壳均已完成；Release 构建、功能运行和安装到工作区验证目录全部通过。用户批准改用当前主机后，三候选各完成三次交错 60 秒运行及一次管理员 WPR 跟踪。Slint 的峰值工作集中位数约 73.41 MiB、安装器约 6.13 MiB，是唯一同时进入 100 MB 内存和 20 MB 安装包预算的候选；Tauri 2 约 454.84 MiB，WinUI 3 约 178.83 MiB 且自包含安装器约 66.21 MiB。

完整数据、测量方法、WPR 原始证据、修正过的夹具问题和限制见[尖峰证据报告](../spikes/evidence-report.md)。当前主机没有可用的干净 Windows 11 VM，启动数据不得解释为冷启动；本机也没有 WPAExporter，ETL 尚未导出为唤醒与磁盘数值。这些限制不改变 Tauri 与 WinUI 已违反硬性体积预算的事实，V1 最终基础确定为 **Rust + windows-rs + Slint**。

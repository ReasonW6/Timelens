# 在本机完成受控决策级复测

Type: task
Status: resolved
Blocked by:

## Question

用户没有可用虚拟机，并明确选择直接在当前 Windows 主机继续。固定当前系统、显示、电源计划和运行时状态，按 Slint、Tauri 2、WinUI 3 交错顺序执行三次 60 秒 Release 运行，记录完整进程树的 CPU、内存、进程数、启动和时间轴响应；在一次 UAC 授权的管理员 PowerShell 中为每项候选追加 WPR GeneralProfile 跟踪，以保留上下文切换、唤醒与磁盘 I/O 原始证据。不得把本机热文件缓存结果描述成干净 VM 冷启动；安装体积继续使用已经实际核验的相同 IExpress 安装器。完成后更新证据报告，并以“当前本机限定”的形式给出 V1 最终基础选型。

## Answer

三候选已按 Slint、Tauri 2、WinUI 3 的交错顺序各完成三次 60 秒 Release 运行，9 次共享驱动器退出码均为 0。峰值工作集中位数分别为 73.41 MiB、454.84 MiB、178.83 MiB；全程平均 CPU 中位数分别为 0.0745%、0.2321%、0.5700%。Slint 与 Tauri 的 240 步时间轴平移中位数约为 3.90 秒和 3.84 秒，WinUI 约为 6.88 秒并有 199 个超时步。

一次 UAC 后的管理员 PowerShell 已连续生成三份 WPR GeneralProfile ETL，外层和三项状态均成功，停止后没有遗留 WPR 会话；原始文件的字节数与 SHA-256 已固定。本机没有 WPA/WPAExporter，因此没有声称已导出上下文切换、唤醒或磁盘数值。本机也没有干净 VM，启动数据明确标为热缓存主机数据。

依据硬性预算，Slint 是唯一同时满足 100 MB 峰值工作集与 20 MB 安装包目标的候选，故当前本机限定的最终选择为 **Rust + windows-rs + Slint**。完整数据和限制见[尖峰证据报告](../spikes/evidence-report.md)。

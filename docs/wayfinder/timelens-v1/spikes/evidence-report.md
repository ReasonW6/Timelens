# Timelens V1 应用基础架构尖峰证据

## 结论状态

Timelens V1 的应用与 UI 基础确定为 **Rust + windows-rs + Slint**。该结论限定于用户批准的当前主机受控对比，不声称等同于干净 VM 冷启动结果。

Slint 是三项候选中唯一同时满足当前主机实测的 100 MB 峰值工作集和 20 MB 安装包目标的方案。Tauri 2 的安装包最小，时间轴响应也合格，但完整 WebView2 进程组的峰值工作集中位数约为 454.84 MiB。WinUI 3 的完整自包含安装包约为 66.21 MiB，峰值工作集中位数约为 178.83 MiB，且 240 步时间轴动画有 199 步未跟上 16 ms 调度目标。

用户确认没有可用虚拟机，并要求直接在本机继续。最终数据来自三候选交错执行的三次 60 秒 Release 运行；此外，一次 UAC 后的管理员 PowerShell 已为每项候选成功生成 WPR GeneralProfile 原始跟踪。Tauri 与 WinUI 已在硬性内存或安装体积预算上明显失败，因此即使不把本机热缓存启动数据当成冷启动数据，也不会改变基础选型。

## 相同功能边界

三套 UI 使用同一个 Rust 工作负载驱动器，以避免把三种不同实现误当成框架差异。每次运行都执行以下工作：

1. 每 500 ms 枚举当前可见顶层窗口并记录焦点，作为正式 WinEvent 采集器之前的等价观测负载。
2. 将确定性的键盘和鼠标计数聚合到 1,440 个分钟桶，不保留原始顺序。
3. 在同一个 SQLite 模式中一次写入 10,000 个时间轴区段并查询可见范围。
4. UI 从同一份 10,000 段数据执行 240 步、目标间隔 16 ms 的确定性平移；三者都只物化可见区段，而不是创建 10,000 个控件。
5. 通过相同的 Win32 托盘实现隐藏与恢复。
6. 通过 GDI `BitBlt` 捕获主显示器，缩放到最长边 1,280 像素并编码一张 WebP。

共享合同见 [`benchmark-contract.md`](benchmark-contract.md)，完整源码见 [`workload-driver`](workload-driver)、[`slint-spike`](slint-spike)、[`tauri-spike`](tauri-spike) 与 [`winui-spike`](winui-spike)。这些都是可丢弃的选型样机，不是产品代码。

## 本机受控重复测量

下表为三次交错 60 秒运行的中位数。括号内为最小值至最大值。CPU 是候选 UI 与共享驱动器完整进程树的累计 CPU 时间占 16 个逻辑处理器总容量的比例；“稳态采集 CPU”从时间轴动画结束 1 秒后开始计算。首窗时间受本机文件缓存影响，不标作冷启动。

| 指标 | Slint | Tauri 2 | WinUI 3 |
| --- | ---: | ---: | ---: |
| 首窗时间 | 542.13 ms（509.57–603.67） | 513.42 ms（495.50–529.17） | 324.40 ms（295.01–387.19） |
| 全程平均 CPU | 0.0745%（0.0612–0.0805） | 0.2321%（0.2210–0.2394） | 0.5700%（0.5685–0.5751） |
| 稳态采集 CPU | 0.0141%（0.0122–0.0231） | 0.0306%（0.0305–0.0374） | 0.0281%（0.0206–0.0297） |
| 峰值工作集 | 73.41 MiB | 454.84 MiB | 178.83 MiB |
| 峰值私有字节 | 17.96 MiB | 270.78 MiB | 111.74 MiB |
| 最大进程数 | 4 | 10 | 3 |
| 240 步平移耗时 | 3,897.06 ms | 3,841.00 ms | 6,882.96 ms |
| 最大步间隔 | 17.57 ms | 24.30 ms | 39.64 ms |
| 超时步数 | 0 | 0（范围 0–1） | 199（范围 196–202） |

本轮运行索引位于 [`local-controlled-runs.json`](benchmark-output/local-controlled-runs.json)，聚合结果位于 [`local-controlled-summary.json`](benchmark-output/local-controlled-summary.json)。迁移时保留了两者以及候选源码；逐次运行产生的 500 ms 进程采样、可再生成的 SQLite/WebP 和早期调试运行没有进入 Git。

## 安装与体积

三种候选使用相同 IExpress 外壳，安装器均已实际执行到工作区内的独立验证目录，并以退出码 0 完成。安装后文件数与字节数逐项匹配打包清单。

| 指标 | Slint | Tauri 2 | WinUI 3 |
| --- | ---: | ---: | ---: |
| 安装器 | 6.13 MiB | 3.00 MiB | 66.21 MiB |
| 安装后载荷 | 11.02 MiB / 4 个文件 | 6.32 MiB / 4 个文件 | 169.60 MiB / 451 个文件 |
| 外部运行时处理 | VC++ DLL 随包计入 | 使用系统 WebView2，按既定预算排除 | .NET 与 Windows App SDK 自包含并计入 |
| SHA-256 | `1DBBD5A5E1BC447F5321D83A3E3856FA26D3069BD8B04984CA0BFD1FCF61E49F` | `33F63204B0B4E6514C1224FBAFDF62ABE12CC15158D8E0D904DD8281AFC9709F` | `68444C8E0B0684EC9C249204D0BEE1A8C96CE5BF3CBCE726B2B27EEBD15D479D` |

三个研究安装器均未签名，不可作为发布物。最终清单见 [`package-metrics.json`](package-output/20260822-191258/package-metrics.json)，哈希与签名状态见 [`installer-checksums.json`](package-output/20260822-191258/installer-checksums.json)，实际安装核验见 [`verification.json`](install-verification/20260822-191442/verification.json)。安装器与展开载荷可由源码重建，因此未进入 Git。

## 测量环境

- Windows 内核构建 `26200.8246`，显示版本 `25H2`。注册表仍返回旧兼容名称 `Windows 10 Pro`，因此只把内核构建号作为可复核的系统标识。
- AMD Ryzen 7 7840H with Radeon 780M Graphics，16 个逻辑处理器。
- 平衡电源计划，GUID `381b4222-f694-41f0-9685-ff5bb260df2e`。
- Rust `1.94.1`，Cargo `1.94.1`，Node.js `24.13.1`，npm `11.8.0`。
- 便携式 .NET SDK `10.0.400`，只放在本尖峰目录中，没有修改系统 PATH。
- 系统 WebView2 Runtime `144.0.3719.115`。
- Slint crate `1.17.1`、Tauri crate `2.11.5`、Windows App SDK WinUI 组件 `2.3.6`。

依赖版本由 [`Cargo.lock`](Cargo.lock) 与 [`TimelensWinUISpike.csproj`](winui-spike/TimelensWinUISpike.csproj) 固定。

## 已排除的测量错误

尖峰过程中发现并修复了四类会扭曲结论的问题，最终表格不包含修复前运行：

- 直接创建 10,000 个 UI 节点会让 Slint 启动失败，因此三套候选统一改成可见区段虚拟化。
- 进程树 CPU 首版采样混用了本地时间与 UTC，且子进程退出后会丢失累计 CPU；随后又发现整数重载截断了不足 1 秒的 CPU 增量。最终数据使用按 PID 保留的正向双精度累计值。
- Slint 首版动画计时器在 240 步后没有停止，造成伪稳态 CPU；最终三次 Slint 运行来自修复后的二进制。
- WinUI XAML 在当前主机初始化失败，样机改用等价的程序化 WinUI 视觉树；没有修改主机上的 Windhawk 或其他系统配置。

## WPR 证据与限制

一次 UAC 授权的管理员 PowerShell 已依次完成三项 `GeneralProfile`，退出码均为 0，停止后 `wpr -status collectors` 确认没有遗留记录会话：

- Slint：`slint-20260822-193820.etl`，865 MiB。
- Tauri：`tauri-20260822-193929.etl`，874 MiB。
- WinUI：`winui-20260822-194029.etl`，817 MiB。

状态见 [`local-controlled-status.json`](benchmark-output/wpr/local-controlled-status.json)，字节数与 SHA-256 见 [`local-controlled-checksums.json`](benchmark-output/wpr/local-controlled-checksums.json)。本机只有 WPR CoreSystem，没有 WPA/WPAExporter，因此本轮没有从 ETL 推断上下文切换、唤醒或磁盘数字。三个 ETL 合计约 2.5 GiB，且当前无法进一步分析；迁移时保留可核验的哈希和运行状态后删除原文件。如将来需要这些指标，应按脚本在目标发布候选上重新采集。

仍需保留以下解释边界：

- 当前主机没有可用的 Hyper-V 或 Windows Sandbox 前端，不能把本机启动时间表述成干净 VM 冷启动。
- 三个实际验证的安装器都是离线载荷，本轮没有可比的首次安装网络下载；Tauri 依赖系统已有的 WebView2 Runtime。
- 当前 GDI 单帧负载只用于框架横向比较，不能替代后续 Desktop Duplication 或 Windows Graphics Capture 的真实轻量预算原型。
- 500 ms 窗口轮询只提供等价负载，不验证最终 WinEvent 增量分类器的正确性。

## 最终选择

V1 采用 **Rust + windows-rs + Slint**。它在当前主机完整样机进程树中满足 CPU 与 100 MB 内存目标，安装器也满足 20 MB 目标。Tauri 2 只在未来明确放宽常驻内存预算时作为备选；WinUI 3 自包含方案同时越过内存与安装包预算，并在本尖峰的时间轴调度中明显落后，因此不进入 V1。

该建议不会把尖峰源码带入产品；正式代码仍应从已确定的窗口模型、权限边界和数据合同重新实现。

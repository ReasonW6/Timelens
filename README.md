# Timelens

Timelens 是一款面向 Windows 11 的轻量级、本地优先活动观察应用，用时间轴记录应用窗口状态、输入设备统计和定时快照，并支持由用户自备 API 密钥的 AI 总结。

> 当前状态（2026-09-05）：里程碑 1 至 5 的功能均已实现，103 项 Release 回归及当前宿主机的 UI、安装升级卸载、崩溃恢复、容量与资源验收通过。实际 Inno 安装包为 9.665 MiB。按用户决定跳过签名和干净 Windows 11 虚拟机验收；锁屏、睡眠、系统重启、多屏、HDR 与远程会话的真实发布矩阵仍待执行。本批实现及证据随本次本地提交归档，尚未推送。

## V1 原则

- 只记录用户窗口，不展示或统计无窗口辅助进程。
- 同一应用的多个窗口归组展示，重叠时间不重复累计。
- 不采集窗口标题、网址、文档内容、剪贴板或原始按键序列。
- UI 与 AI 网络代码保持普通权限，窄职责采集器独立提权运行。
- 数据默认保存在本机，并提供清理、清空和便携备份能力。
- 以低 CPU、低内存占用和小安装体积作为硬性设计约束。

## 技术方向

- Rust
- windows-rs
- Slint
- SQLite（单写入者）
- 受限命名管道连接普通权限核心与提权采集器

## Workspace

- `crates/timelens-app`：普通权限 Slint 核心、单写入者与本地数据目录。
- `crates/timelens-collector`：窄职责采集器；不链接数据库、UI 或网络能力。
- `crates/timelens-ipc`：版本化、限长的 Protobuf 协议和 Windows 命名管道认证。
- `crates/timelens-observer`：不读取标题的用户窗口分类、应用身份解析和虚拟桌面事实查询。
- `crates/timelens-storage`：DPAPI 封装的数据密钥、SQLCipher、WAL 与可恢复迁移。
- `crates/timelens-ai`：提供商协议、模型能力、凭据、调度和上下文规则。
- `crates/timelens-ai-worker`：按需运行的普通权限网络进程，支持三种协议与流式回答。
- `installer`：Inno Setup 安装脚本和 Limited/Highest 登录任务注册脚本。

## 本地验证

仓库由 `rust-toolchain.toml` 固定 Rust 版本。SQLCipher 的 vendored OpenSSL 在 Windows 上还需要可用的 Perl；可以把 Strawberry Perl 放入 `PATH`，或显式设置 `OPENSSL_SRC_PERL`。

```powershell
$env:OPENSSL_SRC_PERL = 'C:\path\to\perl.exe'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --release -- --test-threads=2
cargo build --workspace --release
cargo run -p timelens-collector -- --observe-windows-once
cargo run -p timelens-collector -- --observe-window-events-seconds 10
cargo run --release -p timelens-storage --example milestone2_capacity -- --data-dir C:\path\to\empty-data-dir --include-ai
& '.\docs\wayfinder\timelens-v1\performance-validation\verify-milestone2-privacy.ps1' -DataDirectory C:\path\to\capacity-data
```

DPAPI 与 Credential Manager 回归需要当前 Windows 用户的凭据上下文。完整进程组性能、按需截图和独立安装验收的前置条件及入口见[里程碑 4、5 报告](docs/wayfinder/timelens-v1/performance-validation/milestone-4-5-report.md)。

安装包使用 Inno Setup 7 编译：

```powershell
& 'C:\path\to\ISCC.exe' 'installer\Timelens.iss'
```

Core 与 Collector 只在校验同一用户、同一会话、Windows 返回的真实 PID、固定可执行文件名和同目录路径后握手。正式安装由 Inno Setup 将安装目录收紧为 SYSTEM/Administrators 可写、普通用户只读执行，并检查祖先目录不能被普通用户用于替换载荷；没有代码签名时，这是防止同名二进制替换的必要安全边界。升级由用户主动运行安装器；卸载可选择保留或删除本地数据，外部导出不随卸载删除。

## 文档

- [领域术语与产品边界](CONTEXT.md)
- [V1 Wayfinder 决策地图](docs/wayfinder/timelens-v1/map.md)
- [Wayfinder 归档入口与实施顺序](docs/wayfinder/README.md)
- [架构决策记录](docs/adr)
- [里程碑 2 实现与验收报告](docs/wayfinder/timelens-v1/performance-validation/milestone-2-report.md)
- [里程碑 3 快照与本地报告](docs/wayfinder/timelens-v1/performance-validation/milestone-3-report.md)
- [里程碑 4、5 最终实现与验收报告](docs/wayfinder/timelens-v1/performance-validation/milestone-4-5-report.md)
- [实机发布矩阵及待执行项](docs/wayfinder/timelens-v1/performance-validation/milestone-5-physical-matrix.md)

## 开发状态

仓库保存完整 Wayfinder 决策链、窗口与输入采集、加密快照、本地报告、AI 总结和分支对话、便携备份恢复、数据迁移与安装维护实现。协议为 v4，SQLCipher schema 为 v12。

当前 30 天非图片合成负载包含 90 个 AI 总结，含 Collector 预留共 78.17 MiB；三轮正常采集 CPU 最大 0.0792%，进程组峰值工作集最大 54.15 MiB，AI 流式三进程峰值 64.65 MiB。真实 UI 手动截图的采样峰值为 142.69 MiB，结束时回落至 83.41 MiB；这是单独记录的按需峰值，100 MiB 门槛用于常驻场景。所有产物均为 `NotSigned`。

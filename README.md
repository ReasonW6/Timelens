# Timelens

Timelens 是一款面向 Windows 11 的轻量级、本地优先活动观察应用，用时间轴记录应用窗口状态、输入设备统计和定时快照，并支持由用户自备 API 密钥的 AI 总结。

> 当前状态：正在开发中/未完成。

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
- `installer`：Inno Setup 安装脚本和 Limited/Highest 登录任务注册脚本。

## 本地验证

仓库由 `rust-toolchain.toml` 固定 Rust 版本。SQLCipher 的 vendored OpenSSL 在 Windows 上还需要可用的 Perl；可以把 Strawberry Perl 放入 `PATH`，或显式设置 `OPENSSL_SRC_PERL`。

```powershell
$env:OPENSSL_SRC_PERL = 'C:\path\to\perl.exe'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
cargo run -p timelens-collector -- --observe-windows-once
cargo run -p timelens-collector -- --observe-window-events-seconds 10
cargo run --release -p timelens-storage --example milestone2_capacity -- --data-dir C:\path\to\empty-data-dir
& '.\docs\wayfinder\timelens-v1\performance-validation\verify-milestone2-privacy.ps1' -DataDirectory C:\path\to\capacity-data
& '.\docs\wayfinder\timelens-v1\performance-validation\measure-milestone2.ps1'
```

安装包使用 Inno Setup 7 编译：

```powershell
& 'C:\path\to\ISCC.exe' 'installer\Timelens.iss'
```

Core 与 Collector 只在校验同一用户、同一会话、Windows 返回的真实 PID、固定可执行文件名和同目录路径后握手。正式安装由 Inno Setup 将该目录收紧为 SYSTEM/Administrators 可写、普通用户只读执行；没有代码签名时，这个安装目录 ACL 是防止同名二进制替换的必要安全边界。

## 文档

- [领域术语与产品边界](CONTEXT.md)
- [V1 Wayfinder 决策地图](docs/wayfinder/timelens-v1/map.md)
- [Wayfinder 归档入口与实施顺序](docs/wayfinder/README.md)
- [架构决策记录](docs/adr)
- [里程碑 2 实现与验收报告](docs/wayfinder/timelens-v1/performance-validation/milestone-2-report.md)

## 开发状态

仓库保存已经锁定的领域模型、完整 Wayfinder 决策链和架构决策。里程碑 2 已完成 32 MiB 加密持久断线缓冲、精确托盘连续性、分钟输入聚合、应用泳道与编号窗口详情、保留策略和协调式一键清空。协议为 v3，SQLCipher schema 为 v8；正式 30 天完整非图片产品负载投影为 73.18 MiB，三轮正常采集 CPU 最大 0.231%，进程组峰值工作集最大 53.22 MiB，Release 双二进制合计 15.92 MiB。

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Timelens 是仅面向 Windows 11 的本地优先活动观察应用（Rust 2024 + windows-rs + Slint + SQLCipher）。产品术语以 [CONTEXT.md](CONTEXT.md) 为准（如 Application identity、Focus interval、Monitoring gap、Snapshot omission 等），写代码、注释和文案时沿用这些词，不要混用 "_Avoid_" 中列出的说法。

## 构建与验证

Rust 版本由 `rust-toolchain.toml` 固定（1.94.1）。SQLCipher 的 vendored OpenSSL 需要原生 Perl（Git for Windows 自带的 Perl 不行），本机已放在 `.tools/` 下：

```powershell
$env:OPENSSL_SRC_PERL = "$PWD\.tools\strawberry-perl-5.34.3.1\perl\bin\perl.exe"
```

与 CI（`.github/workflows/ci.yml`）一致的检查：

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --release -- --test-threads=2
```

- 单个 crate / 单个测试：`cargo test --release -p timelens-storage <测试名>`；集成测试：`cargo test --release -p timelens-ai-worker --test worker_boundary`。
- 测试大多是各源文件内的 `#[cfg(test)]` 模块。DPAPI 与 Credential Manager 相关测试需要当前 Windows 用户的凭据上下文。
- AI worker 拒绝在提权环境运行，所以 `worker_boundary` 中三个测试（见 CI 的 `NON_ELEVATED_TESTS`）在管理员终端里会失败，需要以普通用户运行。
- 安装器回归：`Invoke-Pester -Script .\installer\tests\startup.Tests.ps1`（Pester 3.4.0）。
- 安装包：`& '.\.tools\innosetup-7.0.2-x64\ISCC.exe' 'installer\Timelens.iss'`，产物在 `dist/`。

本地运行与调试：

- 核心是单实例的，运行前先关闭已安装的 Timelens；用 `--data-dir <目录>`（或 `TIMELENS_DATA_DIR`）指向隔离数据。
- 采集器自检：`cargo run -p timelens-collector -- --observe-windows-once`、`-- --observe-window-events-seconds 10`。
- 调试构建可设 `TIMELENS_QA_CAPTURE=<目录>` 依次截取主要页面后自动退出；`TIMELENS_QA_VIEWPORT=WxH` 固定窗口尺寸（无头模式默认 1440×900，缩放 1.0）。
- 再加 `TIMELENS_QA_HEADLESS=1` 则在内存中用软件渲染器出图：不弹窗口、不建托盘、使用独立管道，也不占用单实例锁，所以已安装的 Timelens 可以照常运行和采集。此模式必须配合独立的 `--data-dir`（拒绝已安装实例的数据集），可先用 `design_fixture` 示例生成合成数据：

  ```powershell
  cargo run -p timelens-storage --example design_fixture -- <空目录的绝对路径>
  $env:TIMELENS_QA_CAPTURE = "<截图目录>"; $env:TIMELENS_QA_HEADLESS = "1"
  cargo run -p timelens-app -- --data-dir <同一数据目录>
  ```
- 合成数据：`crates/timelens-storage/examples/`（如 `cargo run --release -p timelens-storage --example milestone2_capacity -- --data-dir <空目录> --include-ai`，`inspect_dataset` 用于查看数据集）。

## 架构总览

进程模型（见 `docs/adr/0001`、`0002`、`0004`）：

- **`timelens-app`（`Timelens.exe`，普通权限）**：托盘 + Slint UI + 唯一的 SQLite 写入者。`main.rs` 中 `Storage` 被包在 `Arc<Mutex<_>>` 里，由 UI 线程、命名管道服务线程（接收采集器批次并 `ingest_event_batch`）、快照调度线程、AI 服务和每分钟一次的维护线程（完整性检查 + 保留期清理）共享。耗时查询放到后台线程，经 `mpsc` 回到 UI 线程；窗口隐藏时通过 `ui_refresh.rs` 推迟刷新。
- **`timelens-collector`（`Timelens.Collector.exe`，提权登录任务）**：窄职责，**不得链接数据库、UI、网络或 AI**。通过 `timelens-observer` 观察窗口与输入，把未确认的数据存在 32 MiB 带校验的环形 spool（`spool.rs`），溢出时保留最新数据并生成显式 Monitoring gap。
- **`timelens-ai-worker`（`Timelens.AI.exe`，按需子进程）**：普通权限的网络进程，由 `timelens-app/src/ai.rs` 启动，执行三种提供商协议的流式请求。

共享库：

- `timelens-ipc`：版本化、限长的 Protobuf 协议（`prost` 手写 `#[derive(Message)]`，无 .proto/build 脚本）和命名管道互认证。握手校验同用户、同会话、真实 PID、固定可执行文件名和对端目录。协议版本 `PROTOCOL_VERSION` 在 `lib.rs`，改线格式时需同步升级。
- `timelens-observer`：不读取窗口标题的窗口分类、应用身份解析、虚拟桌面查询和输入计数。
- `timelens-storage`：DPAPI 封装数据密钥 + SQLCipher + WAL。迁移是 `lib.rs` 中按版本递增的 `MIGRATIONS` 数组（部分 SQL 通过 `include_str!` 引入 `*-schema.sql`），`LATEST_SCHEMA_VERSION` 需随之更新；迁移失败会从加密备份恢复。`timeline_cache.rs` 是纯内存的视图缓存（Projection，可丢弃重建）。其余：`collection.rs` 采集数据写入、`milestone3.rs` 快照与本地报告、`snapshot_container.rs` 加密快照容器、`portable.rs` 便携备份、`recovery.rs` 损坏恢复、`relocation.rs` 数据迁移位置。
- `timelens-ai`：提供商协议、模型能力、凭据（Credential Manager）、调度和上下文（AI data envelope）规则。

UI：`crates/timelens-app/ui/*.slint` 通过 `main.rs` 里的 `slint::slint!` 引入；各页面的 Rust 绑定分散在 `*_ui.rs`（`timeline_ui`、`ai_ui`、`collection_ui`、`data_ui`、`general_ui`）。

安装器（`installer/`）：Inno Setup 7 脚本 + 注册 Limited/Highest 两个登录任务的 PowerShell 脚本。提权组件安装在 `Common Files\Timelens` 并收紧 ACL；因为没有代码签名，这个目录权限与握手中的对端路径校验是防止提权替换的安全边界，修改安装路径或握手逻辑时要一并考虑。

## 必须遵守的产品约束

- 只记录用户可切换的顶层窗口；同一应用的多个窗口重叠时间不重复累计。
- 不采集窗口标题、网址、文档内容、剪贴板或原始按键序列；输入只保留聚合计数（Minute input bucket、Daily key frequency）。
- UI 与 AI 网络代码保持普通权限；不要给采集器增加新的能力面。
- 未知/缺失数据要显式记录（Monitoring gap、Snapshot omission、Data availability record），不要推测或补齐。
- 低 CPU、低内存、小安装体积是硬性约束（常驻进程组 100 MiB 门槛）。

## 文档入口

- 规划与决策：`docs/wayfinder/README.md` → `docs/wayfinder/timelens-v1/map.md`（15 张已解决票据、数据/隐私与 AI 任务合同）。新的阻塞性决策应新建 Wayfinder 票据并从地图链接，不要改写已解决票据的结论；不要依赖 `.scratch/`。
- 架构决策：`docs/adr/`（0003 已被 0004 取代）。
- 界面验收与设计：`design-qa.md`、`docs/design/`。
- README 和文档以简体中文书写。

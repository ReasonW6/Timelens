# Timelens 窗口分类器校准工具

这里是票据 12 的一次性本机校准工具，不是正式采集器。它用于验证 WinEvent 增量、公开窗口状态查询、应用身份归组和低频对账能否实现 Timelens 已决定的窗口语义。

## 边界

- 观测器不读取或写入窗口标题，也不采集输入内容、网址、文档内容、剪贴板或截图。
- JSONL 只包含 HWND、PID、进程创建时间、可执行路径、窗口类、公开 AppUserModelID/包身份、窗口样式、矩形和状态查询结果。
- `window-fixture` 和 `WpfFixture.exe` 只创建合成测试窗口，并自动最小化、隐藏、恢复、关闭和退出。
- 200–250ms 对账只用于校准，不是生产周期。生产候选仍为事件驱动加 30–60 秒低频对账，最终周期由性能票 10 决定。
- Debug 可执行文件和原始 JSONL 仅是规划证据，不进入安装包，也不进入 Git。原始 JSONL 可能含本机应用路径和窗口结构；迁移后只保留匿名化结论及原文件的字节数和 SHA-256。

## 构建

```powershell
cargo build --offline
```

WPF 夹具使用 Windows 自带 .NET Framework 编译器：

```powershell
& 'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe' `
  /nologo /target:winexe `
  /out:'.\fixtures\WpfFixture.exe' `
  /reference:'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\WPF\PresentationCore.dll' `
  /reference:'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\WPF\PresentationFramework.dll' `
  /reference:'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\WPF\WindowsBase.dll' `
  /reference:'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\System.Xaml.dll' `
  '.\fixtures\WpfFixture.cs'
```

## 手动复现

先在交互用户会话中启动观测器：

```powershell
.\target\debug\window-observer.exe `
  --output .\manual-run.jsonl `
  --duration-seconds 20 `
  --reconcile-ms 500 `
  --class-prefix Timelens.WindowClassifier.Fixture
```

在观测期间启动夹具：

```powershell
.\target\debug\window-fixture.exe
```

夹具还支持 `--no-aumid`、`--hidden-only`、`--hide-all`、`--step-ms N` 和 `--linger-ms N`。观测器的 `--class-prefix` 仅用于缩小校准日志；生产逻辑不能依赖窗口类白名单。

结论见 [support-matrix.md](support-matrix.md)，原始本机证据的历史哈希见 [local-evidence-manifest.txt](local-evidence-manifest.txt)。正式实现必须从源码重新构建并在目标环境复跑，不依赖已删除的本机日志。

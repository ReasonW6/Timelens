# 里程碑 4、5 开发与验收工作记录

开始日期：2026-09-05。状态：功能实现与本轮宿主机验收完成；真实物理发布矩阵仍有未执行项，详见[正式报告](milestone-4-5-report.md)。

## 既有对话与基线

已通过 Codex 任务工具读取全部三个既有 Timelens 任务及所有分页：

- `规划 Timelens 监控应用`（41 个回合，5 页）
- `开始 Timelens 里程碑 1 开发`（5 个回合）
- `检查里程碑2并完成里程碑3`（1 个回合）

归档任务列表没有额外 Timelens 任务。历史要求以最终数据合同、AI 合同和 ADR 为实施依据。用户明确要求“不用搞签名”和“Windows 11 虚拟机验收跳过”，继续适用 ADR 0004 的用户主动安装器升级；不把未签名或未执行的虚拟机矩阵记作通过。

本次开始时，里程碑 3 的既有改动尚未提交。2026-09-05 原样重跑 `cargo test --workspace --release`，68 项通过。受限沙箱中的 DPAPI 返回状态 2，在当前 Windows 用户上下文运行隔离测试后通过，未更改加密实现以规避环境限制。

## 实施清单

- [x] 全部项目对话、规划合同、既有代码与里程碑 3 基线复核。
- [x] AI 提供商、模型能力、Credential Manager 和独立普通权限网络 worker。
- [x] 白名单数据包、持久任务队列、每日/间隔/手动调度、取消及重试。
- [x] 流式侧栏、提示词预设、版本、分支、上下文压缩和单次图片授权。
- [x] AI 清理、固定版本、Token 与通知、采集隔离回归。
- [x] 便携 ZIP、可选密码、完整性验证和整体恢复。
- [x] 数据位置、崩溃恢复、安装/卸载清理及用户主动升级硬化。
- [x] 完整回归、真实 UI、宿主机安装验收、容量/性能、产物大小与哈希。
- [x] 正式验收报告、路线图和 README 同步，以及 Git 发布状态核实。

## 最终核实

- 103 项 Release 测试、格式和 Clippy 检查通过；最终主程序、Collector、AI worker 和 Inno 包的 SHA-256 已记录并重新核对。
- 同源隔离安装包实际完成安装、升级、保留卸载、重装和删除卸载；最终 34 项检查全部通过，迁移数据的维护路径也单独通过。
- 三轮正常采集、全局暂停、真实 core 启动的 AI 流式 worker 均满足常驻资源门槛；手动截图峰值完成当前版本复测，并按独立口径记录。
- 正常进程退出、core/Collector 崩溃重启、便携恢复、迁移、全量校验与清空确认均保留证据；真实截图测试数据、唯一合成凭据和测试进程已清理。
- 验收期间核对的本地 HEAD 与远端 main 均为 `f938ef722e44842deee8f59766fde6a3b17c0735`。里程碑 3、4、5 的实现与证据现随本次本地提交归档，具体提交号以 Git 历史为准；尚未推送。
- 签名和干净 Windows 11 虚拟机按用户要求跳过；[锁屏、睡眠、重启及物理显示环境矩阵](milestone-5-physical-matrix.md)尚未执行，不记作通过。

## 接口依据

- [OpenAI Chat API](https://developers.openai.com/api/reference/resources/chat)
- [Claude streaming messages](https://platform.claude.com/docs/en/build-with-claude/streaming)
- [Gemini generate content](https://ai.google.dev/api/generate-content)
- [Windows WinHTTP](https://learn.microsoft.com/en-us/windows/win32/api/winhttp/nf-winhttp-winhttpsendrequest)

网络适配器测试使用合成数据与本机协议服务；真实提供商连通性需要用户在应用配置页填写其自备凭据。

# Timelens 里程碑 4、5 实现与验收报告

日期：2026-09-05。版本：0.1.0。协议：v4。SQLCipher schema：v12。

## 结论与完成范围

里程碑 3 的既有实现已经完成，本次先原样复跑其 68 项 Release 测试，再继续实现里程碑 4 和 5。最终 workspace 的 103 项 Release 测试全部通过，格式与 Clippy 检查通过；AI、便携数据、恢复、迁移及安装维护均已接入正式 Slint 应用。

里程碑 1 至 5 的功能均已实现。里程碑 4 的协议、任务、对话与真实界面验收通过；里程碑 5 的当前宿主机安装、升级、两种卸载、崩溃恢复、容量、常驻性能及按需截图复测通过。最终 Inno 安装包为 10,134,704 字节（9.665 MiB），小于 20 MiB 门槛。

完整发布矩阵仍有明确边界。用户此前要求跳过代码签名和干净 Windows 11 虚拟机验收，产物均为 `NotSigned`。真实锁屏、安全桌面、睡眠、系统重启、物理多屏、HDR 和远程会话矩阵尚未执行；相应逻辑有自动化边界测试，但这些测试不能替代物理场景。逐项状态与执行步骤见[实机发布矩阵](milestone-5-physical-matrix.md)。

本次验收基于 `f938ef722e44842deee8f59766fde6a3b17c0735` 之后的里程碑 3、4、5 改动，验收期间核对的本地 `HEAD` 和远端 `main` 均为该基线。实现、文档和精简证据现随本次本地提交归档；具体提交号以 Git 历史为准。本次尚未推送或发布 GitHub Release，实机矩阵的未执行状态保持不变。

## 历史与验收环境

已读取全部三个既有 Timelens 任务及所有分页，包括“规划 Timelens 监控应用”41 个回合、“开始 Timelens 里程碑 1 开发”5 个回合、“检查里程碑2并完成里程碑3”1 个回合；归档列表没有额外 Timelens 任务。实施以 [V1 地图](../map.md)、[数据与隐私合同](../data-retention-privacy-contract.md)、[AI 任务合同](../ai-summary-job-contract.md)及 ADR 为依据。

当前宿主机为 Windows 11 专业版 10.0.26200 x64，AMD Ryzen 7 7840H，8 核 16 线程，Rust/Cargo 1.94.1。具体环境见[环境记录](evidence/milestone45-20260905/environment.json)。需要 DPAPI、Credential Manager 或真实 Windows 进程的测试在当前用户上下文运行；没有削弱加密实现来规避受限沙箱中的 DPAPI 错误。

AI 网络验收只使用本机 loopback 服务、虚构应用数据及可精确清理的合成凭据。未调用真实提供商账户。真实窗口采集与单帧截图只进入隔离测试数据目录；实际桌面截图没有进入 Git 或发送给 AI，复测完成后已删除。

## 里程碑 4：AI 与分支对话

- 新增 `timelens-ai` 协议库与按需启动的普通权限 `timelens-ai-worker`。支持 OpenAI 兼容接口、Claude 和 Gemini 原生协议，以及自定义提供商地址、模型、能力与受约束的参数。Collector 不链接 AI、存储或网络依赖。
- 密钥及敏感请求头存入当前用户的 Windows Credential Manager。提供商配置保存凭据引用；模型、地址或凭据改变后需要重新测试。禁止远程明文 HTTP、重定向跟随及通过凭据头改写请求目标。
- 手动、每日与固定间隔任务使用持久队列，手动任务优先，计划窗口去重，单 worker 串行执行。首次启用不回填旧窗口，重启只补最新完整区间。部分输出不会因重试被静默重放，取消会终止阻塞网络 worker。
- 白名单上下文来自本地聚合数据、覆盖率和明确缺口。空区间、已清理区间及不能满足能力要求的请求在发出前处理；不采集标题、网址、文档内容或原始输入序列。
- 侧栏支持连接测试、预设和可编辑提示词、流式答案、提供商公开返回的推理字段、逐条及累计 Token、版本、固定、分支追问与上下文压缩。提供商未返回推理字段时明确显示该状态。压缩保留原始消息和最近完整问答回合。
- 图片必须逐张解密预览后授权，授权仅供当前手动请求使用。启动恢复、整体数据替换、历史清理和发送结束都会清除旧的预览与授权；计划任务没有图片授权。
- 清空与数据替换先停止 AI，再进入重任务区。界面和后台任务通过数据代次作废旧事件，避免旧消息、旧配置测试结果或旧图片在新数据集重新出现。

真实 UI 已验证连接测试、文本总结、单张合成图片授权、第二个总结版本、分支追问、逐条 Token、重启加载已存历史，以及清空/恢复后的状态刷新。证据见[图片授权](evidence/milestone45-20260905/image-consent.jpg)、[分支追问](evidence/milestone45-20260905/ai-followup.jpg)、[重启历史](evidence/milestone45-20260905/ai-restart-history.jpg)和[清空后的 AI 状态](evidence/milestone45-20260905/cleared-ai-state.jpg)。部分截图用于记录对应功能，最终二进制身份以本报告产物表为准。

## 里程碑 5：便携数据与发布硬化

### 备份、恢复和数据位置

便携 ZIP 包含明文可迁移 SQLite、选定图片和带逐文件长度/SHA-256 的 manifest；可使用 AES-256 密码加密。运行时数据库的解密仅在内存中进行。备份排除 DPAPI 密钥、Credential Manager 凭据、日志、任务注册及运行时文件，并清除连接测试状态、计划启用状态和图片授权。

恢复先完整校验归档，在新密钥下构造并检查加密候选，随后才整体替换当前数据集。错误密码、被篡改内容或不合法结构不能触发替换。移动数据目录先复制、认证并校验目标，再原子更新控制目录指针，最后只清理原位置的产品文件；失败保留原数据和已验证副本。外部导出的 ZIP、CSV 等用户文件不随迁移或卸载删除。

真实 UI 已完成普通 ZIP 导出、移动数据、通过控制目录进入恢复界面、选择便携备份并显式切换候选、在当前窗口整体恢复，以及全量完整性校验。导出的合成归档含 1 张图片、2 个 AI 版本和 2 条对话消息，manifest 全部匹配，SQLite 完整性为 `ok`，外键违规为 0，凭据和图片授权均未进入归档。可选密码、错误密码与篡改拒绝由 Release 回归验证，未另行声称完成密码输入 UI 走查。

证据见[便携内容验证](evidence/milestone45-20260905/portable-summary.json)、[迁移界面](evidence/milestone45-20260905/data-migration.jpg)、[恢复界面](evidence/milestone45-20260905/portable-recovery.jpg)、[整体恢复](evidence/milestone45-20260905/whole-dataset-restore.jpg)和[恢复后的 AI 设置](evidence/milestone45-20260905/restored-ai-settings.jpg)。

主界面的“清空全部”现在先显示包含具体删除范围的确认弹层，可选同时删除当前数据集引用的 AI 凭据。取消不会清空数据；确认后删除活动、输入、图片、本地报告、AI 总结和对话并轮换密钥，保留设置及外部导出。最终真实 UI 验证了弹层与取消保留数据，见[清空确认](evidence/milestone45-20260905/clear-confirmation.jpg)。

### 数据损坏与进程中断

启动进行只读预检并检测未完成迁移。完整性检查包含 SQLite 和外键检查；任意存储操作发现数据库损坏后，会在下一次访问前将连接锁定为只读。恢复工具在独立加密工作副本中抢救可读表，清理孤儿引用，为缺失图片与不可恢复数据记录原因，保留原始数据库和密钥，只有明确使用候选后才切换。

真实进程测试分别强制结束本次隔离实例的 core 和 Collector，再重新启动。原加密库可以重开、密钥不变，并记录采集器中断缺口；正常退出写入结束状态。证据中的 2 条 `collector_restart` 缺口包含合成种子到新 Collector 的交接和实际 Collector 重启，不能解释为两次真实崩溃。见[崩溃验收结果](evidence/milestone45-20260905/crash-summary.json)。

系统退出路径会封口当前输入分钟、窗口与托盘状态，等待有界的 Collector 交付后退出。睡眠时长使用包含/排除睡眠的 Windows 时钟差，墙钟跳变单独记录。这里的真实验证覆盖进程崩溃和正常退出；系统锁屏、睡眠与重启仍按实机矩阵处理。

### 安装、升级和卸载

最终安装器包含 core、Collector、AI worker 和维护脚本。安装路径及全部既有祖先目录均检查固定磁盘、重解析点、拥有者和可替换权限；普通用户不能通过父目录删除子项等权限绕过载荷 ACL。路径不满足保护条件时拒绝安装，不会收紧用户无关目录的权限。对应 Windows 权限依据见 [File security and access rights](https://learn.microsoft.com/en-us/windows/win32/fileio/file-security-and-access-rights)。

在 `Program Files` 下使用独立验收产品名、AppId、任务目录和数据路径实际执行两轮安装维护。复用相同安装逻辑，只隔离测试身份与数据。其中一轮覆盖已经移动的数据目录；最终一轮有 34 项检查全部通过：

- core 登录任务为 `Limited`，Collector 为 `Highest`；实际进程令牌分别为非提权与提权，空闲时没有 AI worker。
- 三个载荷完整、普通用户不能修改载荷或通过祖先目录替换，安装与升级保留原数据密钥。
- 升级先停止旧进程，再替换载荷，随后两个常驻进程重新启动。
- “保留数据”卸载移除任务、进程与载荷，保留加密数据库和密钥；重装继续保留密钥。
- “删除数据”卸载移除当前活动数据、指针、任务和载荷，并确认安装前实际存在的合成凭据已不存在。
- 两种卸载均保留用户主动导出的 ZIP，文件哈希不变。

见[最终安装维护结果](evidence/milestone45-20260905/installer-summary.json)和[已迁移数据的安装维护结果](evidence/milestone45-20260905/installer-moved-data.json)。验收安装和进程已移除。正式发布包本身未另占默认 Timelens 产品身份安装；这里验证的是同源隔离安装包的完整维护路径。

## 最终回归与资源证据

`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings` 和 `cargo test --workspace --release -- --test-threads=2` 均通过。测试分布为 AI 12、真实 worker 集成 3、应用 9、Collector 10、IPC 15、observer 13、storage 41，总计 103。故意损坏数据库的测试会输出预期 SQLCipher HMAC 错误，相关断言通过，不能将其当作运行期失败。见[Release 测试日志](evidence/milestone45-20260905/release-tests.log)与[Clippy 日志](evidence/milestone45-20260905/clippy.log)。

### 常驻与 AI 活跃进程组

每轮使用可见 Release Slint UI 与 Collector，5 秒预热后采样约 60 秒，每轮 118 个样本。正常采集跑三轮，全局暂停跑一轮；AI 场景使用由 core 实际启动的 worker 接收约 65 秒的合成流式回答，117/118 个样本包含 worker。采样覆盖完整进程组，CPU 按 16 个逻辑处理器归一化。性能轮次中的 Collector 是当前普通用户进程；已安装 Collector 的提权令牌在上述安装验收中另行实测。

| 场景 | 平均 CPU | 进程组峰值工作集 | 验收 |
| --- | ---: | ---: | --- |
| 正常采集 1 | 0.0791% | 54.15 MiB | CPU ≤ 1%，内存 ≤ 100 MiB |
| 正常采集 2 | 0.0517% | 53.70 MiB | 通过 |
| 正常采集 3 | 0.0792% | 53.59 MiB | 通过 |
| 全局暂停 | 0.0839% | 53.50 MiB | CPU ≤ 0.5%，内存 ≤ 100 MiB |
| AI 流式回答 | 0.0759% | 64.65 MiB | 三进程、内存 ≤ 100 MiB，CPU 为参考值 |

所有轮次的 UI 响应检查均通过。定时截图在这些轮次关闭，按需编码单独测量。原始 CSV、场景参数与结果见[进程组汇总](evidence/milestone45-20260905/performance-summary.json)。

### 当前版本真实手动截图

最终哈希的可见 UI core 与 Collector 运行时，从快照面板触发一张真实 `DISPLAY1` 图片，生成 1280×720 WebP，有效载荷 590,412 字节。隔离库中有 1 张合成图和本次 1 张实拍图，2 个槽均成功、无缺失；磁盘文件头均为 `TLSNAP`。

30.022 秒测量取得 662 个样本，请求间隔 20 ms、实际平均 45.286 ms、最大 58.459 ms。进程组工作集从 54.92 MiB 上升至采样峰值 142.69 MiB，测量结束时为 83.41 MiB；UI 响应检查通过。此值包含实际可见 UI 和 Collector，口径不同于里程碑 3 的旧单帧进程参考，不直接作为同口径性能回退比较。

100 MiB 为既定常驻门槛。142.69 MiB 是需要保留的按需资源特征，并非小于 100 MiB；离散采样也不能排除更短的峰值。实际图片没有进入对话预览或正式证据目录，测试库、图片和唯一合成凭据均已删除，进程已退出。见[截图采样](evidence/milestone45-20260905/snapshot-performance.json)与[截图结果和清理核实](evidence/milestone45-20260905/snapshot-result.json)。

### 30 天完整非图片负载与隐私

合成负载包含 100 个应用、240,000 次窗口更新、43,200 个输入分钟和 90 个 AI 总结版本，每份合成回答约 8 KiB。数据库为 47,362,048 字节，checkpoint 后 WAL/SHM 为 0；加上其他存储与 34,603,008 字节 Collector 保守预留，总计 81,965,654 字节（78.17 MiB），低于 100 MiB。生成与校验用时 25.300 秒。

隐私检查覆盖 7 个源文件，禁止字段/API 命中为 0；检查的 3 个加密文件未出现明文 SQLite、合成应用或采集标记。该结果是指定负载与检查规则下的验证，不表示任意无限增长的 AI 对话都具有同一容量。见[容量结果](evidence/milestone45-20260905/capacity-summary.json)和[隐私结果](evidence/milestone45-20260905/privacy-summary.json)。

## 交付产物

| 文件 | 字节数 | SHA-256 | 签名 |
| --- | ---: | --- | --- |
| `target/release/timelens.exe` | 21,774,848 | `92CEF5C8EA34FEBE0006C971CA7034BCFA41FDC209C50AEEDC389B18E0DACB56` | NotSigned |
| `target/release/timelens-collector.exe` | 656,384 | `2C1445FC5C34CA3D9626E6D16260D4E831DB1938CE526C6756EE35CB0BA8C454` | NotSigned |
| `target/release/timelens-ai-worker.exe` | 691,200 | `2D113759BBC8849CC125EBE5D902D6803FCB2B5AC068A7717F12F7155B441B9F` | NotSigned |
| `dist/Timelens-0.1.0-x64-setup.exe` | 10,134,704 | `7115EE915C6AD13D769975D964C22D69CDD12F68B83A3956FDEDF4206F103548` | NotSigned |

安装体积门槛作用于实际压缩安装包；不能用未压缩主程序大小替代这个指标。产物清单见[可机读记录](evidence/milestone45-20260905/artifacts.json)，整理报告时已重新核对四个文件的大小、SHA-256 和签名状态。编译器为本机隔离工具目录中的 Inno Setup 7.1.0。

## 复现入口与证据保留

- [合成 UI/性能数据生成器](../../../../crates/timelens-storage/examples/milestone45_fixture.rs)：只接受新的空目录，输出唯一合成凭据标识；`ui`、`normal`、`idle`、`ai` 模式分别控制暂停和队列状态。
- [本机模拟 AI](mock-ai.py)：使用 `--state-dir` 保存动态 loopback 地址，可用 `--stream-seconds 65` 生成长流；请求日志只保留时间、模型、消息数和 `store` 标志。
- [常驻性能脚本](measure-milestone45.ps1)：提供普通与长流两个 endpoint、全新 scratch 目录和输出目录，自动执行三次正常、一轮暂停及一轮 AI。
- [截图峰值脚本](measure-milestone45-snapshot.ps1)：显式提供隔离 core/Collector 的 PID，开始采样后从真实 UI 触发一次手动捕获，再用[数据检查工具](../../../../crates/timelens-storage/examples/inspect_dataset.rs)确认成功槽。
- [崩溃复现](verify-milestone45-crash.ps1)：只启动和结束自身创建的隔离实例。
- [安装维护复现](verify-milestone45-installer.ps1)：限定 `target/acceptance/84d2c6a9`、独立产品身份及 Program Files 目录，需要由用户授权的管理员上下文；前置条件为合成数据、其便携导出及相同隔离定义编译的 Inno 包。
- [实现工作记录](milestone-4-5-worklog.md)、[里程碑 3 历史报告](milestone-3-report.md)和[未执行实机矩阵](milestone-5-physical-matrix.md)。

正式保留精简 JSON、CSV、回归日志和合成数据 UI 图片。可执行文件、编译器、数据库、导出包、安装过程详细日志及测试副本留在 `target/` 或 `dist/`，不随源码进入 Git。证据保留范围见[清单](../ARTIFACTS.md)。

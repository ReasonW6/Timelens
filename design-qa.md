# Paper 原生界面验收

2026-09-06。范围为用户选定的第二张 Paper 设计及其在既有 Rust / Slint 产品中的完整实施。验收使用隔离的合成数据，记录处于暂停状态。

## 当前结论

已完成逐页操作、100% / 125% / 150% 缩放、1180×680 最小窗口、长日期、多日范围与键盘焦点检查。120 项 Release 回归已通过；最终 Release 构建、实际运行、同状态视觉对照与产物校验均已完成。

final result: passed

在本文列明的本机验收范围内，没有仍需修正的 P0 / P1 / P2 问题。真实外部 AI 服务、不同实体显示器与长期运行的验收边界见文末。

## 视觉依据与比较方法

- 视觉真值：[用户选定的 Paper 原图](docs/design/rebuild-20260905/options/display-02-paper.png)，1586×992。原图 SHA-256 为 `de764e9ae9cdf19f4d4945ca251c984f5a2d51f997933d54b5fd9ae69febaa7e`。
- 比较场景：浅色、时间线、2026-09-05、09:00–18:00、暂停记录，选择 Visual Studio Code 的 14:10–15:22 活动。四个应用、七段聚焦活动、一段 17 分钟缺口；聚焦总计 5 小时 24 分钟。
- 原生首选内容区为 1440×900 逻辑像素。100% 下含系统标题栏截图应为 1442×932；移除左侧 1 像素边框、顶部 31 像素标题栏和右下边框后，得到 1440×900 内容图。
- 本项目无 CSS viewport 或浏览器 `deviceScaleFactor`；比较使用 Slint 逻辑尺寸和实际屏幕像素。125% 和 150% 通过进程级 `SLINT_SCALE_FACTOR` 验证，不改变系统设置。
- 原图仅为 QA 创建一份 1440×900 等密度副本，保留原始文件。图中模拟窗口按钮与真实 Windows 标题栏有差异，比较不把系统边框当作产品内容。
- [compare.py](docs/design/rebuild-20260905/implementation/compare.py)记录归一化方法，并输出全景、导航与标题、右栏细节三组同输入比较图。

最终截图为 [final-timeline.png](docs/design/rebuild-20260905/implementation/final-timeline.png)，1442×932。实际同时查看的比较输入为 [全景对照](docs/design/rebuild-20260905/implementation/comparison-full.png)（2880×926）、[导航与标题](docs/design/rebuild-20260905/implementation/comparison-header.png)（2088×276）和 [右栏细节](docs/design/rebuild-20260905/implementation/comparison-details.png)（792×926）。原图与原生界面都以 1440×900 内容尺寸参与比较。

全景对照确认三栏比例、活动密度、时间带位置及连续纸色表面；局部对照确认标题、数字、图标、分隔线、详情按钮与选中状态。正文没有裁切或重叠，右栏底部操作可达。原生字体、业务数据与真实焦点样式的差异已单独分类，不宣称逐像素一致。

最终版本的其他证据：[键盘统计](docs/design/rebuild-20260905/implementation/final-keyboard.png)、[快照预览](docs/design/rebuild-20260905/implementation/final-snapshot.png)、[快照确认弹窗](docs/design/rebuild-20260905/implementation/final-snapshot-modal.png)、[恢复窗口](docs/design/rebuild-20260905/implementation/final-recovery.png)、[其他恢复方法](docs/design/rebuild-20260905/implementation/final-recovery-methods.png)、[当天空状态](docs/design/rebuild-20260905/implementation/final-empty.png)。

## 比较与修正历史

以下记录包含视觉比较与实际操作发现的问题。构建或 lint 通过不计为视觉验收通过。

| 迭代 | 严重度及问题 | 已实施的修正 | 修正后证据 |
| --- | --- | --- | --- |
| 初次原生预览 → 02 | P1：活动列表未占满中栏，时间带与底部说明间出现大段空白；图标在行内偏上，右栏窗口行挤叠 | 明确列表、头部、底部和右栏行的伸缩规则；图标置于固定宽度容器垂直居中；简化零秒时长 | [时间线 02](docs/design/rebuild-20260905/implementation/iteration02-timeline.png) |
| 02 | P1：在当前日拖动完整时间带可能查询未来时间，使未结束活动被延长 | 查询、拖选终点限制到现在，纯未来范围不替换已有选择；保留完整自然日刻度 | Release 回归中的未来拖选与范围边界测试；后续日期及范围实机检查 |
| 02 → 06 | P1：弹窗背景焦点仍可能进入普通控件 | 模态打开时禁用背景的鼠标、键盘和无障碍默认动作；Tab 在取消与确认间循环 | [模态无障碍树](docs/design/rebuild-20260905/implementation/modal-accessibility.txt)、[窄窗口弹窗](docs/design/rebuild-20260905/implementation/iteration06-narrow-modal.png) |
| 05 → 06 | P0：应用过滤后的行布局重叠，并存在显示索引与应用身份混用风险 | 在 Rust 中构建过滤模型，保留原始索引与身份，复用稳定的列表实例 | [搜索 Edge](docs/design/rebuild-20260905/implementation/iteration05-app-search.png)；点击后显示 Edge 的 1 小时 2 分钟聚焦、980 次键盘计数 |
| 05 → 10 | P2：范围按钮过宽、长日期及多日范围需要明确宽度，下午缺口分组与跨午间活动时长需要校正 | 单日范围 160px、多日范围 240px；标题 40px；旧年份独立显示；跨午间活动保持完整 40 分钟；同一缺口合并展示 | [最长日期](docs/design/rebuild-20260905/implementation/iteration10-min-long-date.png)、[七天范围](docs/design/rebuild-20260905/implementation/iteration10-min-seven-days.png) |
| 06 → 10 | P2：复选框标签行过矮、短表单被多余留白拉散，间隔按钮 12/24 的文字被截断 | 复选框按内容调整高度；表单顶部对齐；间隔单位移到标签，按钮显示完整数字 | [计划表单](docs/design/rebuild-20260905/implementation/iteration10-min-plan.png)、[关于页](docs/design/rebuild-20260905/implementation/iteration10-min-about.png) |
| 06 → 10 | P1：从 AI 设置转到全局 AI 连接时可能显示错误子页，返回后原标签丢失 | 全局连接视图独立控制可见内容，保留 AI 页最后使用的标签 | 实机顺序“AI 设置 → 全局设置 / AI 连接 → AI 总结”，连接表单与返回标签均正确 |
| 06 → 10 | P1：Escape 关闭弹窗时，焦点恢复早于背景解禁 | 先释放模态状态，再恢复设置或快照导航焦点；循环依据实际焦点 | [最小窗口弹窗](docs/design/rebuild-20260905/implementation/iteration10-min-modal.png)；Tab、Shift+Tab、Escape 实机通过，Escape 后无障碍焦点为“设置” |
| 08 → 10 | P1：125% / 150% 启动后窗口底部超出屏幕，设置和时间带被裁切 | 在首次原生 resize 事件后适配当前显示器工作区，保留已经在工作区内的位置，支持负坐标显示器几何 | [125%](docs/design/rebuild-20260905/implementation/iteration10-window-125.png)、[150%](docs/design/rebuild-20260905/implementation/iteration10-window-150.png)；工作区几何回归通过 |
| 10 → 最终 | P2：浅陶土选中背景上的 16px 文字对比度为 3.68 | 新增文字强调色 `#a24731`，按钮填色保持 `#b9573c`；选中文字对比度为 4.75 | [最终导航对照](docs/design/rebuild-20260905/implementation/comparison-header.png)、[快照选中标签](docs/design/rebuild-20260905/implementation/final-snapshot.png) |

其他已处理的功能细节：应用页右栏不沿用上一条活动的详情；快照保留天数调整保留实际容量设置；没有可导出快照时禁用对应操作；键盘 Backspace 使用可渲染的 `Back` 标签；周期刷新不重建身份未变的列表行。

## 五项视觉检查

| 表面 | 实施与验收依据 |
| --- | --- |
| 字体与排印 | Microsoft YaHei UI，默认 14px；辅助文字 13px；导航 16px；日期 40px；时长 36px。检查中文回退、数字、长日期、换行、字段标签、单位、禁用和焦点。真实字体的抗锯齿和字形不冒充生成图中的精确字体文件。 |
| 间距与布局节奏 | 左栏 228px、右栏 396px、中栏自适应，主内容内边距 26px；按钮一般为 36px 高，导航 46px。连续暖纸表面、少量分隔线、5px 控件圆角。最小窗口中主内容与右栏可独立滚动，固定导航与底部范围始终可达。 |
| 颜色与状态 | 纸色 `#fbf9f6`，侧栏 `#f5f2ec`，正文 `#302c28`，次级文字 `#736c63`，线条 `#e5dfd6`，按钮陶土色 `#b9573c`，选中背景 `#f2e1d8`，强调文字 `#a24731`。正文 / 纸色对比度 13.18，次级文字 4.93，白字主按钮 4.68，选中文字 4.75，缺口文字 4.81。禁用控件保留独立视觉与可操作状态。 |
| 图像与图标 | 统一使用 Tabler Icons v3.34.1 的正式 SVG 与 MIT 许可；应用图标来自已安装 EXE 的原生资源，保留透明度与锐度。快照直接使用业务解密结果；验收图有明确的合成内容标识。没有将整张设计图或伪造的品牌图用作界面。 |
| 文案与产品内容 | 文案解释真实数据范围、暂停、缺口、为空、失败和下一步。主屏不暴露数据库版本、PID 或完整路径。保留原生业务允许的窗口编号，不将生成图中的文档或扩展名作为已采集事实。 |

## 已检查的交互

| 页面 / 流程 | 实际验证 |
| --- | --- |
| 时间线 | 当天与前后日期、手动日期、09:00–18:00 精确范围、多日范围、拖动范围、活动选择与详情、空状态、暂停说明、缺口与跨午间活动 |
| 应用 | 四个应用的图标与名称，搜索 Edge 后的行位置与身份，右栏正确统计，应用规则入口 |
| 记录规则 | 三类排除规则、合并与历史删除页面的导航、字段、滚动与取消路径；未执行历史删除或更改用户隐私设置 |
| 快照 | 合成快照选择、解密预览、640×360 尺寸与完整性信息、缺失状态、政策显示、删除确认及取消、最小窗口滚动。最终 Release 中确认默认聚焦“取消”，背景全部禁用；Escape 取消后焦点恢复到“快照”，图片仍保留 |
| 统计与报告 | 在隔离数据中生成本地报告，覆盖率 96.9%、19,970 次键盘计数、一张快照、四个应用；键盘统计、零值与高值层级、横向滚动 |
| AI | 六个标签，未配置时的连接引导，连接表单顶部与底部、高级项和模型能力字段、提示词、计划、历史、设置；全局连接入口往返保持上下文 |
| 数据维护 | 备份 / 恢复、范围导出、数据位置、维护四个标签；执行只读完整性检查，反馈“完整性与外键检查通过”；未执行用户数据恢复或迁移 |
| 设置与弹窗 | 保留策略显示、危险操作分组、关于页；弹窗默认取消、背景隔离、Tab / Shift+Tab 循环、Escape 取消后焦点恢复 |
| 恢复窗口 | 原数据位置、恢复目录、备份路径、其他方法、滚动、忙碌与就绪条件；在隔离数据中重新检查并打开原位置成功 |
| 窗口尺寸 | 1440×900、1180×760、1180×680；100%、125%、150% 缩放。150% 修正后实际窗口截图为 2162×1348，屏幕起点为 (195, 0)，底部导航和时间带完整可见 |

## 回归与产物

- `cargo fmt --all -- --check` 通过。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` 通过；最终颜色调整后再次通过，耗时 56 秒。
- `cargo test --workspace --release -- --test-threads=2`：120 通过，0 失败，0 忽略。包括 26 项主应用测试、41 项存储测试、三种协议的真实回环 HTTP 与 AI worker 边界测试。
- `cargo build --workspace --release` 完整构建通过，耗时 7 分 15 秒。最终原生预览已实际打开，错误日志为空。
- 测试和构建日志分别为 `target/ui-rebuild/final-tests-release.log`、`target/ui-rebuild/final-clippy.log`、`target/ui-rebuild/final-build-release.log`。验收内容使用 `target/ui-rebuild/paper-fixture-data`；没有打开或更改用户正式数据集。

最终产物位于 [target/ui-rebuild/preview](target/ui-rebuild/preview)，包含主应用、AI worker 与 Collector 三个同版本程序，版本为 `0.1.0`，签名状态均为 `NotSigned`。三个预览文件的 SHA-256 均与 `target/release` 构建结果和[产物清单](docs/design/rebuild-20260905/implementation/release-artifacts.json)一致。

主应用 [timelens.exe](target/ui-rebuild/preview/timelens.exe) 大小为 28,542,976 字节（27.22 MiB），SHA-256 为 `a1aacde8afe66e23630304d611f69be3717f4e716a765a10eedb753bb4cb6769`。当前演示进程使用隔离合成数据并暂停记录，停留在 9 月 5 日的时间线。双击不带参数的程序会按正常启动逻辑使用默认数据位置；如需重开同一隔离预览，应使用下列命令：

```powershell
& 'D:\GitHub_Project\Timelens\target\ui-rebuild\preview\timelens.exe' --data-dir 'D:\GitHub_Project\Timelens\target\ui-rebuild\paper-fixture-data'
```

## 可接受差异与验收边界

- 原图中的窗口标题、三条窗口时长和时间条像素位置不能替代业务数据。实际窗口使用中性编号，统计沿用所选范围的数据，时间带根据时间戳绘制。
- 原生 Microsoft YaHei UI 的字形与生成图不同；时长采用统一字号，辅助层级仍清楚。键盘焦点使用完整的细边框，与仅表示选中状态的左侧色条各有用途。Windows 标题栏保留真实系统控件，界面采用平整纸色表面。这些差异不影响选定设计的三栏层级和操作位置。
- 当前机器日期为 9 月 6 日，因此“今”标记随真实日期显示；选择的回顾日期仍为 9 月 5 日。
- 源图没有给出设置、表单、恢复、空状态等全部页面，这些页面使用同一视觉系统并以实际业务流程验收。
- 未配置真实 AI 提供商与凭据，因此没有进行真实外部 AI 服务的端到端测试。回环协议测试不能代替该项验收。
- 当前显示器的缩放已检查；负坐标显示器采用几何回归测试，未完成跨不同真实显示器的拖动矩阵，也未重做历史安装、HDR、远程桌面与长期资源验收。
- 未提交、推送或发布安装包。历史性能与安装数据不应用于本次界面重构的最终二进制。

## 实施清单

- [x] 统一六个主页面与恢复窗口。
- [x] 修复实际操作中发现的布局、数据身份、焦点与缩放问题。
- [x] 完成主流程、空状态、字段、滚动与键盘检查。
- [x] 保存最终 Release 的同状态截图，并完成三组视觉对照。
- [x] 归档最终检查、二进制信息和剩余验收边界。

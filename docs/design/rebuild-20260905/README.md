# Timelens Paper 原生界面重构

2026-09-05 至 2026-09-06。用户选定本轮第二张「羊皮纸手记」后，已将其实施为 Rust / Slint 原生界面，并逐页进行实际操作与布局调整。保留左侧侧边栏和三栏结构，统一暖纸色、中文排印、陶土色操作、真实应用图标、表单和状态反馈。

时间线、应用、快照、统计与报告、AI 总结、设置及恢复窗口共用同一套界面组件。设计原图保留在 `options/`，真实应用截图保留在 `implementation/`。最终验收、对照图、回归结果和能力边界见项目根目录的 [design-qa.md](../../../design-qa.md)。

## 设计方向与原图

以下编号严格对应本轮三张生成结果在对话中的实际显示顺序。选择编号只能在此集合中解析。

| 显示编号 | 设计图 | 设计文件名称 |
| --- | --- | --- |
| 1 | [查看完整图片](options/display-01-porcelain.png) | 白瓷时序 |
| 2（已选定） | [查看完整图片](options/display-02-paper.png) | 羊皮纸手记 |
| 3 | [查看完整图片](options/display-03-graphite.png) | 石墨工作台 |

三张原始 PNG 均为 1586×992；提示中请求的构图尺寸为 1600×1000。保留生成器原始尺寸，未缩放、裁剪或重新排版。项目副本与生成原件的 SHA-256 一致，详情见 [options-manifest.json](options-manifest.json)。

## 设计与实施依据

- [设计简报](DESIGN_BRIEF.md)记录完整的前端范围、导航与页面对应关系、状态规则、原生业务边界及统一示例数据。
- [参考图与来源](references/sources.json)记录本轮实际查看的官方资料。视觉研究包括 [Linear 的界面更新](https://linear.app/now/behind-the-latest-design-refresh)、[Things 功能设计](https://culturedcode.com/things/features/)和 [Notion Calendar 桌面界面](https://www.notion.com/product/calendar/download/windows)。
- [生成提示](prompts.json)保存三次独立图像生成的实际提示。

生产实现不依赖这些生成图片来绘制界面。文本、表单、列表、时间带和交互均由原生组件与实际业务数据驱动。

## 实施中的产品语义

图像生成适合验证视觉和信息层级，数据与交互语义以产品模型为准。

1. 时间带与记录时长从实际时间戳计算，跨本地午夜分组，跨午间的同一活动不重复分割。当前日期的查询终点限制到现在。
2. 生成图中的“扩展：Python”“启动页”没有采集来源。实际界面使用“窗口 1”等中性编号，不读取窗口标题、网址或文档内容。
3. 应用名称和图标取自可用的本地 EXE 元数据，并以应用身份关联数据。无法读取元数据时保留存储名称和统一图标。
4. 暂停、缺口、快照、报告和 AI 连接状态来自现有业务；同一时段的活动与输入缺口在时间线上合并展示，报告仍保留原始记录数。
5. 使用 Tabler Icons 的正式 SVG 与 MIT 许可。按钮具备键盘焦点、禁用和忙碌状态；确认弹窗隔离背景操作，Escape 取消后恢复焦点。
6. 原生首选内容区为 1440×900，最小为 1180×680。已检查 100%、125%、150% 缩放，窗口会在显示后适配当前显示器工作区。

## 实施入口

- `crates/timelens-app/ui/main-window.slint`：六个主要页面、时间线、右栏与确认弹窗。
- `crates/timelens-app/ui/theme.slint`：颜色、图标与原生通用控件。
- `crates/timelens-app/ui/{ai,collection,data}-panel.slint`：AI、记录规则、键盘统计与数据维护。
- `crates/timelens-app/src/timeline_view.rs`、`timeline_ui.rs`：时间、日期、分组与选择状态。
- `crates/timelens-app/src/app_icon.rs`、`ui_model.rs`、`window_placement.rs`：应用元数据、稳定列表刷新和窗口边界。
- `crates/timelens-storage/examples/design_fixture.rs`：仅允许向已存在的空目录生成隔离验收数据。

验收数据包含四个应用、七段聚焦活动、一段缺口和一张明确标记为合成内容的快照；未添加 AI 提供商、凭据或计划。验收没有连接真实 AI 服务，也没有进行用户数据清空、备份恢复或目录迁移。后端相关能力由回归测试验证，不能据此声称真实外部服务已经端到端验收。

本次更改保留在工作区，未提交或推送 Git。Release 二进制及签名状态以最终验收记录为准，历史发布数据不代表本次重构后的性能或安装验收。

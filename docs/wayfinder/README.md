# Wayfinder 规划归档

Timelens 使用 Wayfinder 的 local-markdown tracker。V1 地图、票据及决策证据已从临时工作区迁入 Git；这里是正式开发唯一应依赖的规划入口。

当前实施进度：里程碑 1 与里程碑 2 已完成；里程碑 2 的实现范围、问题修复和可复现证据见[验收报告](timelens-v1/performance-validation/milestone-2-report.md)。

## 从这里开始

1. 打开 [Timelens V1 决策地图](timelens-v1/map.md)。地图状态为 `complete`，15 张决策票均已解决，没有待认领 frontier。
2. 按地图中的 **Implementation milestones** 开始正式实现。
3. 遇到产品或架构边界时，沿地图链接进入具名票据，再查看其研究、合同、原型、校准报告或 ADR，不要依赖 `.scratch/`。
4. 新出现且会阻止实施的决策，应新建具名 Wayfinder 票据并从地图链接；不要悄悄改写已解决票据的历史结论。

## 归档结构

- `timelens-v1/map.md`：权威地图、已决定事项、范围和实施里程碑。
- `timelens-v1/issues/`：15 张已解决的具名票据。
- `timelens-v1/research/`：Windows API、权限、基础框架、AI 和快照研究。
- `timelens-v1/prototypes/`：可直接打开的交互原型，仅用于解释已选交互。
- `timelens-v1/*-contract.md`：数据/隐私与 AI 任务合同。
- `timelens-v1/spikes/`、`calibration/`、`performance-validation/`：可复现源码与精简证据。
- [迁移与证据保留清单](timelens-v1/ARTIFACTS.md)：哪些内容进入 Git，哪些本机产物可以安全删除。

# Wayfinder 规划归档

Timelens 使用 Wayfinder 的 local-markdown tracker。V1 地图、票据及决策证据已从临时工作区迁入 Git；这里是正式开发唯一应依赖的规划入口。

当前实施进度（2026-09-05）：里程碑 1 至 5 的功能均已实现。里程碑 2 的首个可运行闭环见[验收报告](timelens-v1/performance-validation/milestone-2-report.md)；里程碑 3 的快照与本地报告见[验收报告](timelens-v1/performance-validation/milestone-3-report.md)；最终 103 项回归、AI、便携数据、安装维护、容量与资源证据见[里程碑 4、5 报告](timelens-v1/performance-validation/milestone-4-5-report.md)。签名和干净 Windows 11 虚拟机按用户要求跳过，物理系统场景的[发布矩阵](timelens-v1/performance-validation/milestone-5-physical-matrix.md)仍有未执行项；本批实现与证据随本次本地提交归档，尚未推送。

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

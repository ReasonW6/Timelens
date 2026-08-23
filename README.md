# Timelens

Timelens 是一款面向 Windows 11 的轻量级、本地优先活动观察应用，用时间轴记录应用窗口状态、输入设备统计和定时快照，并支持由用户自备 API 密钥的 AI 总结。

> 当前状态：V1 产品与架构规格已完成，正式应用代码尚未开始实现。

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

## 文档

- [领域术语与产品边界](CONTEXT.md)
- [V1 Wayfinder 决策地图](docs/wayfinder/timelens-v1/map.md)
- [Wayfinder 归档入口与实施顺序](docs/wayfinder/README.md)
- [架构决策记录](docs/adr)

## 开发状态

仓库目前保存已经锁定的领域模型、完整 Wayfinder 决策链和架构决策。正式开发从决策地图的 Implementation milestones 开始；首个里程碑将建立 Rust workspace、Slint 应用外壳、安装与权限边界、进程间通信以及加密本地存储基础。

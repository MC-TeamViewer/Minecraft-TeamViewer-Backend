# 更新日志

本文档记录 TeamViewRelay Backend 的重要变更。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。Backend 版本与网络协议版本分别演进；版本标题中的
`protoX.Y.Z` 表示该 Backend 发布时使用的协议版本，而不是 Backend 版本的一部分。

## [Unreleased]

### 文档

- 建立 Backend 发布历史。
- 增加[战区区块状态协议 vNext 设计方向](docs/battle-chunk-state-protocol-vnext.md)。

## [1.1.3-proto0.8.0] - 2026-08-27

### 变更

- 玩家位置来源以外部数据源作为稳定基线；游戏端来源连续存在 500ms 后接管，消失时立即回退。

### 修复

- 旧协议字段清除改为仅重建受影响对象，不再回退整房间快照或展开完整战区缓存。
- 修复玩家在多个游戏端来源和外部来源之间切换时产生短暂删除与地图标记跳变的问题。

## [1.1.2-proto0.8.0] - 2026-08-24

### 新增

- 增加协议 epoch 兼容矩阵和稳定投影规则，当前支持协议 `0.8.0`，最低兼容 `0.6.1`。
- 支持外部目录、关系查询和外部 Tab 数据。
- 增加由 `memory-debug` Cargo feature 独立启用的内存、CPU、heap profile 和 pprof 诊断能力；普通 release 不包含这些采样任务和 Debug API。

### 变更

- 战区历史改为每个房间独立缓存，默认保留 7,200 秒，每房间最多保留 65,536 个区块。
- 战区缓存使用无损共享值、revision 变更批次、增量补丁和缓存后的直接摘要计算，避免周期性复制、编码和扫描完整历史。
- heap profile 的符号解析改由一次性子进程执行，避免调试工具的符号缓存抬高服务进程 RSS。

### 修复

- 修复连接投递状态落后时使用错误快照作为补丁基线、继而频繁出现 `Digest mismatch` 的问题。
- 修复战区历史随来源断开而丢失，以及多个房间之间缓存状态相互影响的问题。
- 修复少量连接下战区全量展开、全量摘要和重复编码导致的持续高 CPU 与内存增长。

## [1.0.3-proto0.7.1] - 2026-08-23

### 修复

- 修复战区区块在快照、补丁和摘要之间表示不一致而触发的同步错误。

## [1.0.2-proto0.7.0] - 2026-08-23

### 变更

- 完成 Rust 后端重构并优化管理员后台。

### 修复

- 修复管理员后台统计问题。

## [1.0.1-proto0.7.0] - 2026-08-23

### 修复

- 修复管理员后台在筛选状态下的初始化数据加载。

## [1.0.0-proto0.7.0] - 2026-08-23

### 变更

- 发布首个 Rust Backend 正式版本，替代 Python 实现作为主线后端。

## [0.5.14-proto0.7.0] - 2026-08-23

### 新增

- 增加 Tab History 同步能力。
- 管理后台支持历史数据、运行状态和按连接统计 Protobuf 流量。

### 修复

- 修复管理页面选择项导入问题，并归档 Python hotfix 版本。

## [0.5.12-proto0.6.5] - 2026-08-20

### 新增

- 支持显示离线玩家最后位置。

### 变更

- 玩家位置来源仲裁优先选择高精度坐标。
- 标记 Docker 发布镜像。

### 修复

- 修复离线玩家数据处理。
- 修复增量广播无法表达字段删除的问题。

## [0.5.8-proto0.6.2.hotfix] - 2026-08-18

### 新增

- 支持官方数据源。

### 修复

- 修复来源仲裁后无可用数据的问题。
- 修复远程玩家状态滞留。

> 此版本沿用仓库中的历史 hotfix 标签名称；其提交范围包含后续协议兼容工作的早期实现。

## [0.5.8-proto0.6.2] - 2026-04-11

### 新增

- 增加 UUID 映射。

### 变更

- Backend 兼容协议 `0.6.2` 的 battle map mode。
- CI 支持重试协议子模块拉取。

## [0.5.7-proto0.6.1] - 2026-04-07

### 新增

- 管理后台增加流量统计，并区分压缩后流量与传输层流量。

### 修复

- 修复历史流量视图无法切换的问题。

## [0.5.6-proto0.6.1] - 2026-04-06

### 新增

- 增加初版后台管理页面和运行状态展示。

## [0.5.5-proto0.6.1] - 2026-04-05

### 变更

- 升级协议子模块到 `0.6.1`。
- 减少网络下行开销。

### 修复

- 修复优化后下行网络 I/O 反而增大的问题。

## [0.5.4-proto0.6.0] - 2026-04-04

### 变更

- 将主要网络消息重构为 Protobuf，并规范化协议版本追踪。
- 拒绝不兼容的旧客户端，并返回中文错误信息。
- 优化编解码性能和服务端卡顿。

### 修复

- 修复战区地图上报失败。

## [0.5.3-proto0.5.1] - 2026-04-02

### 变更

- 升级网络协议到 `0.5.1`。
- 改善核心区块显示和区块边界抖动。
- 增加 Docker 镜像自动构建尝试。

## [0.5.2-proto0.5.0] - 2026-03-27

### 新增

- 支持“绿色版”目录布局。

## [0.5.1-proto0.5.0] - 2026-03-09

### 修复

- 修复 Tab 信息被错误超时清理。

## [0.5.0-proto0.5.0] - 2026-03-08

### 新增

- 网页端支持删除玩家报点。

### 变更

- 升级网络协议到 `0.5.0`。
- 减少 Tab 状态和其他实时状态的网络流量。

### 修复

- 修复客户端退出服务器后已上报远程信息残留。

## [0.4.0-proto2] - 2026-03-07

### 新增

- 建立 TeamViewRelay Backend 初始实现。

[Unreleased]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v1.1.3-proto0.8.0...HEAD
[1.1.3-proto0.8.0]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v1.1.2-proto0.8.0...v1.1.3-proto0.8.0
[1.1.2-proto0.8.0]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v1.0.3-proto0.7.1...v1.1.2-proto0.8.0
[1.0.3-proto0.7.1]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v1.0.2-proto0.7.0...v1.0.3-proto0.7.1
[1.0.2-proto0.7.0]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v1.0.1-proto0.7.0...v1.0.2-proto0.7.0
[1.0.1-proto0.7.0]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v1.0.0-proto0.7.0...v1.0.1-proto0.7.0
[1.0.0-proto0.7.0]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.14-proto0.7.0...v1.0.0-proto0.7.0
[0.5.14-proto0.7.0]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.12-proto0.6.5...v0.5.14-proto0.7.0
[0.5.12-proto0.6.5]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.8-proto0.6.2.hotfix...v0.5.12-proto0.6.5
[0.5.8-proto0.6.2.hotfix]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.8-proto0.6.2...v0.5.8-proto0.6.2.hotfix
[0.5.8-proto0.6.2]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.7-proto0.6.1...v0.5.8-proto0.6.2
[0.5.7-proto0.6.1]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.6-proto0.6.1...v0.5.7-proto0.6.1
[0.5.6-proto0.6.1]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.5-proto0.6.1...v0.5.6-proto0.6.1
[0.5.5-proto0.6.1]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.4-proto0.6.0...v0.5.5-proto0.6.1
[0.5.4-proto0.6.0]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.3-proto0.5.1...v0.5.4-proto0.6.0
[0.5.3-proto0.5.1]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.2-proto0.5.0...v0.5.3-proto0.5.1
[0.5.2-proto0.5.0]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.1-proto0.5.0...v0.5.2-proto0.5.0
[0.5.1-proto0.5.0]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/v0.5.0-proto0.5.0...v0.5.1-proto0.5.0
[0.5.0-proto0.5.0]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/compare/Backend-v0.4.0-proto2...v0.5.0-proto0.5.0
[0.4.0-proto2]: https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Backend/tree/Backend-v0.4.0-proto2

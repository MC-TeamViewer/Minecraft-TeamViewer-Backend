# 更新日志

本文档记录 TeamViewRelay Backend 的重要变更。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。Backend 版本与网络协议版本分别演进；版本标题中的
`protoX.Y.Z` 表示该 Backend 发布时使用的协议版本，而不是 Backend 版本的一部分。

## [1.2.0-alpha.11-proto0.9.0] - 2026-09-11

### 修复

- WT 门停用 `WT-Protocol` 协商回显:生产实验实证 Chrome 146/151 只要收到带引号
  回显立即 "Opening handshake failed" 中止会话建立;不回显(规格 MAY 语义)回落
  plain 行为,浏览器脚本全链路恢复。
- SQLite `synchronous` 降为 NORMAL(WAL 下安全):消除流量统计 flush 提交的
  fsync 尖峰(慢盘上单条 upsert 曾达 1.1s)。

### 变更

- 观测补强:WT 门 accept_bi 超时/控制流缺失/状态流失败由静默改 `warn`(带远端
  地址);door session disconnected 补 `reason`(writer/reader/datagram 谁先
  结束);首帧 10s 缺失补 `warn`。

## [1.2.0-alpha.6-proto0.9.0] - 2026-09-08

### 新增

- 压缩套（suite）协商落地 QUIC 门：ALPN 扩为三套并列 `teamviewrelay/v1`（plain）、
  `teamviewrelay/v1+zstd`、`teamviewrelay/v1+zstd-dict`，rustls 按客户端偏好序选择，
  握手内 0 额外 RTT 生效；未知 ALPN 拒绝连接。`src/compress.rs` 提供套定义与连续 zstd
  分块流编解码器：发送端每连接持久 CCtx 逐 envelope `write + flush`（ZSTD_e_flush），
  压缩块以 `[varint 长度][压缩块]` 走 varint 分帧；接收端持久 DCtx 连续喂入，压缩块边界
  与 envelope 一一对应但解码不依赖该假设，跨帧共享压缩上下文（等效 permessage-deflate
  的 context takeover），解压窗口上限 2^23（8 MiB），解压结果超过帧上限视为协议违规。
- 下行压缩级别 3（服务端决策，zstd 帧自描述无需协商）。压缩为**单向语义**：仅下行（服务端→客户端）压缩，上行恒为 plain 分帧（WS 门为 plain 消息）——上行载荷小（握手、命令、回执），浏览器端解压库 fzstd 仅解压，双向压缩需在用户脚本内嵌完整压缩器，得不偿失。
- 压缩套协商落地 WS 门：`Sec-WebSocket-Protocol` 子协议 `teamviewrelay.{plain,zstd,zstd-dict}.v1`
  三套并列，服务端按客户端偏好序择一并原样回显（101 响应确认，握手内 0 额外 RTT；回显
  必须取自客户端提供的列表，RFC 6455 硬性要求——zstd-dict 原样回显不折叠，WS 门无
  datagram，其流行为与 zstd 一致）。选定 zstd/zstd-dict 时关闭 permessage-deflate
  （zstd 输出近高熵，外层 deflate 纯烧 CPU）并启用连续 zstd 分块流（每条 WS binary
  消息 = 一个压缩块）；选定 plain 时完全关闭压缩。客户端未提供任何 teamviewrelay
  子协议则维持原 permessage-deflate 协商路径（旧客户端零变化）。
- 压缩套协商落地 WT 门：extended CONNECT 请求头 `WT-Available-Protocols`（RFC 9651
  字符串 List，客户端偏好序）择一套并以 `WT-Protocol` 回显（必须取自客户端列表，
  RFC 9651 字符串 Item 带引号；协议值与 WS 子协议同名，`zstd-dict` 保留原值——
  WT 门有 datagram，两套语义有别）。当前浏览器尚未实现该头
  （Chromium issue 435589295、Firefox bug 1981483），不发即 plain 行为，与
  `1.2.0-alpha.5` 完全一致；浏览器实现后自动激活，服务端无需再改动。
- datagram 通道压缩落地 QUIC/WT 门（`+zstd`/`+zstd-dict` 套）：每块 movement 压缩为
  一个自包含 zstd 单帧（无跨 datagram 上下文，丢一块不影响后续），预算按压缩后长度
  校验；`plain` 套维持裸 envelope 零变化。
- datagram 字典模式（`+zstd-dict` 套独有，proto/v0.9.0-alpha.2 `DoorControlFrame`）：
  服务端用近期 movement 批训练 4 KiB 字典，经门专用 door-control 下行流（服务端第 2 条
  单向流，开序紧跟状态流）下发 `dict_offer(ID, 内容)`；客户端完整安装后经自己的第 1 条
  单向流回 `dict_ready(ID)`；服务端收到 ready 才激活该字典——激活前按独立 zstd 压缩，
  因果屏障保证字典字节必然先于用它压缩的任何 datagram 过线，door-control 流绝不承载
  应用层 `WireEnvelope`。新模块 `src/door_control.rs`：`DatagramDictEncoder`（训练/
  激活/编码状态机）、激活门控、current+previous 字典生命周期（≤8 KiB/连接，新 ID 激活
  即逐出更旧，连接关闭全释放）、训练策略（条数阈值 64 + 样本字节 ≥16 KiB + 最小重训
  间隔 60 秒，zstd 训练器对样本量不足报错时按一次尝试计并丢最旧样本重试）。唯一理论
  竞态（客户端安装任务滞后于 datagram 接收）表现为响亮的 zstd 错误，按丢包丢帧自愈。
  WT 门全因果链集成测试覆盖开序约定、offer/ready 握手与激活门控。

### 变更

- QUIC/WT 门在 `+zstd`/`+zstd-dict` 套下仅服务端下行流（uni）自动启用 zstd；客户端
  上行流（bi）恒为 plain varint 分帧。plain 套与 `1.2.0-alpha.5` 线路行为完全一致。
  WS 门选定 zstd 套后仅服务端→客户端消息压缩，客户端→服务端消息恒为 plain envelope。
- QUIC/WT 门 datagram 发送路径按套分流：`+zstd`/`+zstd-dict` 套逐块压缩后装入
  datagram，`plain` 套裸块直发（与 `1.2.0-alpha.5` 一致）。
- WS 门选定 zstd 套后不再协商 permessage-deflate 扩展；旧客户端（无 teamviewrelay 子协议）
  的 deflate 协商行为与 `1.2.0-alpha.5` 完全一致。

## [1.2.0-alpha.5-proto0.9.0] - 2026-09-08

### 协议（proto/v0.9.0-alpha.1）

- `WebMapHandshakeRequest.accepts_unreliable_positions`（0.8.1 布尔声明）泛化为
  `accepts_channels`（`repeated UnreliableChannel` 列表，MOVEMENT 起）；旧字段标记 deprecated，
  服务端将 `true` 映射为 `[MOVEMENT]`，0.8.1 客户端行为不变。详见协议仓库 README
  "传输门约定"：varint 流分帧、datagram 无前缀、10s 首帧握手、压缩协商命名表与 door-control 流不变式。

### 变更（不兼容）

- 可靠流分帧由 4 字节大端长度前缀改为 varint LEB128 前缀（`[varint 长度][payload]`，上限 8 MiB，
  头部最长 4 字节）；典型帧省 2 字节。帧头非法即显式断开连接（修复旧实现对超限帧静默缓冲增长的问题）。
- QUIC datagram 全版本统一去除 4 字节长度前缀：datagram 自带报文边界，plain 版为裸 protobuf
  envelope。`1.2.0-alpha.4` 的 `[4B+envelope]` datagram 格式废弃，旧客户端不兼容（alpha 期允许 break）。

### 新增

- 新增裸 QUIC 门 `src/quic_transport.rs`：quinn endpoint 监听独立端口（默认 `8767/udp`，
  默认关闭），ALPN `teamviewrelay/v1`，供 Java mod 直连（浏览器 WebTransport 仍走 WT 门）。
  两门共享同一 relay 核心、会话处理器与证书运行时（`CertRuntime` 抽出为门无关模块，多证书
  SNI 选择与热轮换行为一致）；流约定与 WT 门一致（客户端 1 条双向流上行 + 服务端 1 条单向流
  下行 + varint 分帧），movement 位置批走 QUIC datagram。
- 配置新增 `[quicTransport]` 段与 `TEAMVIEWER_QUIC_*` 环境变量（`ENABLED`/`BIND`/`CERT_PATH`/
  `KEY_PATH`/`IDENTITIES`/`POLL_INTERVAL_SEC`/`RENEW_WINDOW_SEC`），证书结构、优先级与热轮换
  与 WT 门完全同构；Docker 镜像 `EXPOSE 8767/udp`，compose 默认映射。

## [1.2.0-alpha.4-proto0.8.1] - 2026-09-07

### 新增

- WebTransport 位置分流（协议 `0.8.1`，新增 `WebMapHandshakeRequest.accepts_unreliable_positions`）：声明能力且连接 datagram 预算足够的网页地图会话，逐 tick 玩家位置改走不可靠 QUIC datagram（绝对值 upsert，丢失即被下 tick 覆盖，不重传），同时可靠补丁剥离既有玩家的位置字段、只保留真正变化的非位置增量；新玩家全字段 upsert 与低频（默认 10 秒，`[protocol] movementRefreshSec` / `TEAMVIEWER_MOVEMENT_REFRESH_SEC` 可调）保底全位置刷新仍走可靠流，兼容旧客户端与 WebSocket 连接（行为不变）。
- movement 批按 ≤1024 字节条目对齐切块编码一次、跨连接共享，单 tick 最多 16 块；datagram 帧格式与可靠流一致（4 字节长度前缀 + protobuf envelope），路径 MTU 收缩时跳过装不下的块、由下个 dirty tick 重发自愈。

## [1.2.0-alpha.3-proto0.8.0] - 2026-09-07

### 修复

- WebTransport 会话建立后立即断开、网页地图一直"待连接"：服务端误把所有下行数据（handshake_ack、状态广播）写回客户端双向控制流，而网页脚本只读服务端单向流，导致握手应答丢失、单向流控背压在数秒后杀掉会话；现下行数据统一走服务端单向流。

## [1.2.0-alpha.2-proto0.8.0] - 2026-09-07

### 新增

- WebTransport 支持多证书：TOML `identities` 数组或环境变量 `TEAMVIEWER_WT_IDENTITIES`（JSON）可配置最多 16 张证书（如域名证书 + IP 证书），TLS 握手按 SNI 精确/泛域名匹配选择，无 SNI（浏览器按 IP 直连）时使用 `default` 标记证书并依次回退到含 IP SAN 的证书；旧的单证书 `certPath`/`keyPath` 配置与 `TEAMVIEWER_WT_CERT_PATH`/`KEY_PATH` 环境变量保持兼容。
- `TEAMVIEWER_WT_POLL_INTERVAL_SEC` 与 `TEAMVIEWER_WT_RENEW_WINDOW_SEC` 支持环境变量覆盖（此前仅文档提及、实际只读编译期内置 TOML）。

### 变更

- WebTransport 证书热轮换按证书独立进行：单张证书文件损坏只保留该张旧证书，其余证书正常轮换；不再整体重建 QUIC endpoint 配置。

## [1.2.0-alpha.1-proto0.8.0] - 2026-09-07

### 新增

- 新增 WebTransport/QUIC 入口 `https://host:8766/web-map/wt`，与 WebSocket 共享房间状态，默认关闭。

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

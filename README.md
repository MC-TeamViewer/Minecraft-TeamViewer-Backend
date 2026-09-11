# TeamViewRelay Backend

TeamViewRelay 的 Rust 后端服务。它接收 Minecraft 客户端上报的数据，按房间聚合状态，并广播给游戏内客户端和网页地图端。

## 功能

- 玩家、实体、路标、战局区块及玩家标记的状态聚合
- 玩家端与外部全局上报源的来源仲裁
- 在线 Tab 状态、离线位置和 Tab History
- 基于 Protobuf 的全量快照、增量广播和版本兼容
- 内置 Vue 管理页面、实时指标、审计和 SQLite 持久化
- HTTP、SSE、Minecraft 与 Web Map WebSocket 的可信代理真实 IP 解析

相关组件：

- [Minecraft_TeamViewer](https://github.com/MC-TeamViewer/Minecraft_TeamViewer)：Minecraft 客户端 Mod
- [Minecraft-TeamViewer-Web-Script](https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Web-Script)：网页地图投影脚本

项目文档：

- [更新日志](CHANGELOG.md)
- [战区区块状态协议 vNext 设计方向](docs/battle-chunk-state-protocol-vnext.md)

## 快速启动

推荐直接使用 Docker Compose：

```bash
export TEAMVIEWER_ADMIN_USERNAME=admin
export TEAMVIEWER_ADMIN_PASSWORD=please-change-me
docker compose up -d
curl --fail http://127.0.0.1:8765/health
```

默认端点：

- `ws://127.0.0.1:8765/mc-client`：Minecraft 客户端
- `ws://127.0.0.1:8765/web-map/ws`：网页地图
- `http://127.0.0.1:8765/admin`：管理页面
- `http://127.0.0.1:8765/health`：健康检查
- `http://127.0.0.1:8765/snapshot`：状态快照

`/playeresp` 和 `/adminws` 仅作为旧客户端兼容入口保留。

## 协议兼容矩阵

服务端当前协议为 `0.8.0`，最低支持 `0.6.1`。握手要求客户端和服务端声明的
`[minimum_compatible_network_protocol_version, network_protocol_version]` 区间相交；实际功能按双方都支持的最高版本选择。

兼容策略集中在 `src/protocol_compat.rs`，领域状态始终使用当前模型，只有连接边界上的 handshake、snapshot、patch、digest
和入站报告会经过 profile 投影。管理后台会显示每个在线连接的 epoch 和命中的规则。

| 客户端 epoch | 活跃兼容规则数 | 主要适配 |
| --- | ---: | --- |
| `0.6.1` | 7 | 移除 battle mode、角色、last seen、来源元数据和 Tab History；clear-fields 回退；legacy digest |
| `0.6.2` | 6 | 角色、last seen、来源元数据、clear-fields、Tab History、legacy digest |
| `0.6.3` | 5 | last seen、来源元数据、clear-fields、Tab History、legacy digest |
| `0.6.4` | 4 | 来源元数据、clear-fields、Tab History、legacy digest |
| `0.6.5` | 2 | Tab History、legacy digest |
| `0.7.0` | 1 | legacy battle chunk digest |
| `0.7.1` | 0 | 保留实时状态、Tab 历史，不开放关系查询 |
| `0.8.0` | 0 | 当前合同，支持外部目录和关系查询 |

以后升级协议时，必须同时：新增或更新 epoch 能力、为真实投影登记稳定规则 ID、补边界与摘要向量测试，并确认管理后台能枚举新规则。
提高最低兼容版本时，应在同一提交中删除已不可触发的 epoch、规则和测试。

## Docker

构建当前源码：

```bash
docker build -t teamviewrelay-backend:local .
```

已发布镜像：

```text
professornuo/teamviewrelay-rust:v1.2.0-alpha.11-proto0.9.0
```

`docker-compose.yml` 默认使用该版本，并将 SQLite 数据目录挂载到宿主机的 `./data-rust`。

### 内存与 CPU Debug 镜像

Debug 监控只在 `memory-debug` Cargo feature 中存在，普通 release 不包含 profiler、采样任务或 Debug API。使用独立 compose overlay 构建并部署：

```bash
docker compose -f docker-compose.yml -f docker-compose.memory-debug.yml up -d --build
docker compose logs -f backend
```

它对应镜像 tag `v1.2.0-alpha.2-memory-debug-proto0.8.0`，每 10 秒把资源快照写入
`./data-rust/memory-debug/samples-YYYY-MM-DD.jsonl`，heap 原始 dump、pprof、手动 CPU pprof 和 SVG
火焰图写入 `./data-rust/memory-debug/profiles/`。首次 heap profile 默认在启动 2 分钟后生成；内存相对
上次 profile 增长 8 MiB 时会自动追加抓取。heap 的符号解析由一次性子进程完成，解析缓存不会进入
服务进程 RSS。采样文件持续记录进程、线程和 cgroup CPU，默认不会自动启动进程内 CPU profiler。

登录管理页后可使用以下鉴权接口：

- `GET /admin/api/debug/resources/current`：最近一次资源快照与 profiler 状态。
- `GET /admin/api/debug/resources/profiles`：可下载的 profile 列表。
- `POST /admin/api/debug/resources/profiles/cpu`：手动抓取 30 秒 CPU profile。
- `POST /admin/api/debug/resources/profiles/heap`：手动抓取 heap profile。
- `GET /admin/api/debug/resources/profiles/{filename}`：下载 profile。

本机直接构建等价二进制时使用：

```bash
RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --profile memory-debug --features memory-debug
```

常用环境变量：

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `TEAMVIEWER_PORT` | `8765` | 服务监听端口 |
| `TEAMVIEWER_DB_PATH` | `./data/teamviewer-admin.db` | SQLite 数据库路径 |
| `TEAMVIEWER_ADMIN_USERNAME` | `admin` | 管理员用户名 |
| `TEAMVIEWER_ADMIN_PASSWORD` | `admin` | 管理员密码，生产环境必须覆盖 |
| `TEAMVIEWER_ADMIN_SESSION_TTL_SEC` | `43200` | 管理会话有效期 |
| `TEAMVIEWER_WT_ENABLED` | `false` | 是否启用 WebTransport/QUIC；启用需显式配置证书 |
| `TEAMVIEWER_WT_BIND` | `0.0.0.0:8766` | QUIC UDP 监听地址 |
| `TEAMVIEWER_WT_CERT_PATH` | 空 | PEM fullchain 路径；生产使用 Let’s Encrypt fullchain.pem |
| `TEAMVIEWER_WT_KEY_PATH` | 空 | PEM private key 路径；生产使用 Let’s Encrypt privkey.pem |
| `TEAMVIEWER_WT_IDENTITIES` | 空 | 多证书 JSON 数组，设置后整体覆盖 CERT_PATH/KEY_PATH 与 TOML 证书配置 |
| `TEAMVIEWER_WT_POLL_INTERVAL_SEC` | `300` | 证书文件检查间隔，范围 30–86400 |
| `TEAMVIEWER_WT_RENEW_WINDOW_SEC` | `604800` | 到期提前拒绝窗口，范围 1–30 天 |
| `TEAMVIEWER_QUIC_ENABLED` | `false` | 是否启用裸 QUIC 门（Java mod 直连）；默认关闭 |
| `TEAMVIEWER_QUIC_BIND` | `0.0.0.0:8767` | 裸 QUIC UDP 监听地址 |
| `TEAMVIEWER_QUIC_CERT_PATH` | 空 | 与 WT 门同款证书配置，可共享同一张证书 |
| `TEAMVIEWER_QUIC_KEY_PATH` | 空 | 与 WT 门同款证书配置 |
| `TEAMVIEWER_QUIC_IDENTITIES` | 空 | 与 WT 门同款多证书 JSON 数组 |
| `TEAMVIEWER_QUIC_POLL_INTERVAL_SEC` | `300` | 证书文件检查间隔 |
| `TEAMVIEWER_QUIC_RENEW_WINDOW_SEC` | `604800` | 到期提前拒绝窗口 |
| `TEAMVIEWER_TRUST_PROXY_HEADERS` | `false` | 是否读取可信反代转发的真实 IP |
| `TEAMVIEWER_TRUSTED_PROXY_CIDRS` | 本机与 Docker 私网段 | 可被信任的直连反代地址段 |
| `RUST_LOG` | `info` | Rust 日志过滤规则 |
| `TZ` | 系统时区 | 管理统计使用的时区 |

Debug overlay 还支持 `TEAMVIEWER_DEBUG_SAMPLE_INTERVAL_SEC`、`TEAMVIEWER_DEBUG_AUTO_CPU_PROFILE`、
`TEAMVIEWER_DEBUG_CPU_TRIGGER_PERCENT`、`TEAMVIEWER_DEBUG_CPU_PROFILE_SEC`、`TEAMVIEWER_DEBUG_MEMORY_GROWTH_MIB`、
`TEAMVIEWER_DEBUG_STARTUP_PROFILE_DELAY_SEC`、
`TEAMVIEWER_DEBUG_PERIODIC_PROFILE_SEC`、`TEAMVIEWER_DEBUG_PROFILE_COOLDOWN_SEC`、
`TEAMVIEWER_DEBUG_RETENTION_DAYS`、`TEAMVIEWER_DEBUG_MAX_PROFILES` 和
`TEAMVIEWER_DEBUG_MAX_DISK_MIB`。默认保留 7 天采样、48 组 profile，并限制 profile 总量为 512 MiB。
设置 `TEAMVIEWER_DEBUG_AUTO_CPU_PROFILE=true` 才会恢复启动、高 CPU 和周期 CPU profile。进程内 CPU
profile 会显著增加常驻符号缓存并短暂干扰业务吞吐；诊断长期内存增长时应保持默认关闭，只在内存样本
收集完成后手动触发。

## 反向代理与真实 IP

启用可信代理头时，后端只会在 TCP 直连来源属于 `TEAMVIEWER_TRUSTED_PROXY_CIDRS` 时读取代理头，并按以下优先级选择第一个合法 IP：

1. `CF-Connecting-IP`
2. `X-Real-IP`
3. `X-Forwarded-For`

示例：

```bash
export TEAMVIEWER_TRUST_PROXY_HEADERS=true
export TEAMVIEWER_TRUSTED_PROXY_CIDRS=127.0.0.1/32,::1/128,172.16.0.0/12
docker compose up -d
```

OpenResty / Nginx 示例位于 `deploy/openresty-teamviewer.conf.example`。

### WebTransport 与 QUIC

WebTransport 默认关闭。启用时必须提供 PEM 证书和私钥，后端只热检测并替换证书，不内嵌
ACME。UDP 端口必须直连或在容器/防火墙上显式映射；Nginx 与 OpenResty 不能按普通 HTTP 反代
WebTransport。

单证书（向后兼容）：

```bash
export TEAMVIEWER_WT_ENABLED=true
export TEAMVIEWER_WT_BIND=0.0.0.0:8766
export TEAMVIEWER_WT_CERT_PATH=/app/certs/fullchain.pem
export TEAMVIEWER_WT_KEY_PATH=/app/certs/privkey.pem
```

多证书（域名证书 + IP 证书等）：通过 `TEAMVIEWER_WT_IDENTITIES`（JSON 数组）或 TOML
`identities` 配置，最多 16 张：

```bash
export TEAMVIEWER_WT_IDENTITIES='[
  {"certPath": "/app/certs/fullchain.pem", "keyPath": "/app/certs/privkey.pem"},
  {"certPath": "/app/certs/ip.crt.pem", "keyPath": "/app/certs/ip.key.pem", "default": true}
]'
```

优先级：`TEAMVIEWER_WT_IDENTITIES` > `TEAMVIEWER_WT_CERT_PATH`/`KEY_PATH` > TOML
`identities` > TOML `certPath`/`keyPath`；环境变量任一形式存在时整体替换 TOML 证书配置。

每个 TLS 握手按以下顺序选证书：

1. SNI 精确命中某张证书的 DNS SAN（大小写不敏感）；
2. SNI 泛域名命中（仅最左单标签 `*.example.com`）；
3. `default = true` 标记的证书；
4. 第一张含 IP SAN 的证书；
5. 第一张证书兜底。

浏览器按 IP 直连时因 RFC 6066 不发送 SNI，因此需要一条 `default` 标记或 IP SAN 证书承接。
SNI 未命中任意证书时回退到默认证书并记录 warn 日志。

热轮换按证书独立进行：单张文件读取或解析失败只影响该张（保留旧证书，其余正常轮换）；新证书
有效期落入 `renewWindowSec` 内则拒绝替换并保持旧证书，下个轮询周期复查。

浏览器入口为 `https://host:8766/web-map/wt`。WS 路径保持不变；Java mod 第一版继续使用
WebSocket。证书更新成功只影响新 QUIC 连接，已有连接保留旧 TLS 配置并按客户端重连收敛。

### 裸 QUIC 门（Java mod 直连）

`[quicTransport]`（默认关闭）在独立端口（默认 `8767/udp`）提供裸 QUIC 门，供 Java mod 客户端
直连——浏览器 WebTransport 需要 HTTP/3 + extended CONNECT 封装，裸 QUIC 没有 H3 层，两者
线路互不兼容，因此是并行监听的两扇门。两门共享同一个 relay 核心与同一套 WireEnvelope
应用层语义；裸 QUIC 无 URL path，会话类型由首个握手消息的通道字段自识别。

- ALPN 三套并列：`teamviewrelay/v1`（plain）、`teamviewrelay/v1+zstd`、
  `teamviewrelay/v1+zstd-dict`（压缩套语义见下文"压缩"），rustls 按客户端偏好序选择，
  未知 ALPN 的握手直接拒绝。
- 证书配置与 `[webTransport]` 完全同构（TOML `identities` 数组、`certPath`/`keyPath`、
  环境变量 `TEAMVIEWER_QUIC_IDENTITIES`/`CERT_PATH`/`KEY_PATH`），可与 WT 门共享同一张
  证书；SNI 选择与热轮换逻辑也完全一致。
- 流约定与 WT 门一致：客户端开 1 条双向流上行、服务端开 1 条单向流下行，分帧
  `[varint 长度][payload]`；movement 位置批走 QUIC datagram（RFC 9221）。
- Docker 镜像已 `EXPOSE 8767/udp`，compose 默认映射 `${TEAMVIEWER_QUIC_PORT:-8767}`。

### 压缩（zstd 压缩套，1.2.0-alpha.6 起）

压缩以"套"为单位在**门原生协商载体**上商定（0 额外 RTT，服务端确认制），全部搭现有
握手便车，不做应用层协商：

- `plain`：流与 datagram 均不压缩；
- `zstd`：可靠流走连续 zstd 分块流（见下），datagram 逐块单帧独立压缩；
- `zstd-dict`：可靠流同 `zstd`，datagram 走字典模式（见下）。

各门的协商载体与当前落地状态：

- **QUIC 门**（ALPN）：`teamviewrelay/v1`（plain）、`teamviewrelay/v1+zstd`、
  `teamviewrelay/v1+zstd-dict`，rustls 按**服务端列表序**取客户端也提供的第一个
  （服务端序即偏好序：zstd-dict 优先），未知 ALPN 拒绝握手；
- **WS 门**（子协议）：`Sec-WebSocket-Protocol` 三套并列 `teamviewrelay.plain.v1`、
  `teamviewrelay.zstd.v1`、`teamviewrelay.zstd-dict.v1`，服务端按偏好序择一并在 101
  响应原样回显（RFC 6455 要求回显取自客户端提供的列表；zstd-dict 在 WS 门下流行为
  与 zstd 一致——无 datagram，客户端按收到 dict 回显即 zstd 流语义解释）。选定
  zstd 套时不协商 permessage-deflate（外层 deflate 对 zstd 输出零收益）；客户端未提供
  teamviewrelay 子协议则维持原 deflate 协商路径（旧客户端零变化）；
- **WT 门**（extended CONNECT 目标 URL 的 query 参数，`?suite=plain|zstd|zstd-dict`）：
  query 是协商的**唯一权威**——浏览器不把 `WT-Protocol` 响应回执暴露给脚本
  （Chromium issue 435589295），响应头协商通道在浏览器侧断裂，故改走两端共享的
  URL。未携带 `suite`、值为空或无法识别时一律取 `zstd-dict`（压缩率最高）；
  服务端仍以裸 token 回显 `WT-Protocol` 响应头，仅作未来浏览器实现协商语义后
  的前向兼容。

可靠流的 zstd 语义是一条**连续 zstd 流的分块切片**：发送端每连接一个持久 CCtx，逐
envelope `write + flush` 保证即时可解码，压缩块作为一帧 payload 走 varint 分帧（WS 门
的压缩块即整条 binary 消息，消息边界天然对齐）；接收端把逐帧压缩块持续喂进同一条持久
DCtx——压缩块边界与 envelope 一一对应，跨帧共享压缩上下文（等效 permessage-deflate
的 context takeover）。解压窗口上限 8 MiB。压缩为**单向语义**：仅下行（服务端→客户端）
压缩，上行恒为 plain varint 分帧（WS 门为 plain 消息）——上行载荷小（握手、命令、回执），
浏览器端解压库 fzstd 仅解压，双向压缩需在用户脚本内嵌完整压缩器，得不偿失。
datagram 不适用该模型（自包含单元，丢弃互不影响）。

datagram 的压缩语义（QUIC/WT 门，`+zstd`/`+zstd-dict` 套）：每块压缩为**一个自包含
zstd 单帧**（无跨 datagram 上下文，丢一块不影响后续），无应用层长度前缀。
`+zstd-dict` 套叠加**字典模式**：服务端用近期 movement 批训练 4 KiB 字典，经门专用
door-control 下行流（服务端第 2 条单向流，开序紧跟状态流）下发 `dict_offer(ID, 内容)`；
客户端完整安装后经自己的第 1 条单向流回 `dict_ready(ID)`；服务端收到 ready 才把该字典
切为压缩当前字典（**激活**）——激活前按独立 zstd 压缩，因果屏障保证字典字节必然先于
用它压缩的任何 datagram 过线。唯一理论竞态（客户端 door-control 安装任务滞后于
datagram 接收）表现为响亮的 zstd 错误，按普通丢包丢帧自愈——movement 下一 tick 全量
重发兜底。字典生命周期防泄漏：每连接只保留 current + previous 两个字典（≤8 KiB），
新 ID 激活即逐出更旧，连接关闭全部释放；字典按连接独立训练，不跨连接共享；重训设
最小间隔（60 秒）+ 条数/字节样本阈值，防重训风暴。`plain`/`+zstd` 套不开启
door-control 流。

## 源码开发

环境要求：

- Rust 1.94+
- Node.js 24+ 与 pnpm（仅管理前端）
- 已初始化的协议 submodule

初始化并验证：

```bash
git submodule update --init --recursive

cd admin-ui
corepack enable
pnpm install --frozen-lockfile
pnpm test
pnpm build
cd ..

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo run --release
```

管理前端开发服务器：

```bash
cd admin-ui
pnpm dev
```

Vite 会把管理 API、SSE 和管理页请求代理到 `127.0.0.1:8765`。

管理页面按“概览 / 流量 / 活跃指标 / 数据管理 / 审计日志”分区。在“数据管理”中可按房间清理压测或废弃数据：房间必须离线，并需要再次输入完整房间名确认。清理范围包括该房间的审计、日/小时活跃行、Tab History、Last Seen 缓存和不再被其他房间引用的身份映射；全局聚合的历史流量无法归属到具体房间，因此始终保留。

## 压力测试

`scripts/load_test_live.py` 是独立的黑盒协议压测工具，不导入或启动任何 Python 后端。运行时会根据锁定的协议 submodule 在临时目录生成 Python binding；因此需要安装 [uv](https://docs.astral.sh/uv/) 并先初始化 submodule。

对当前 Rust 版本执行远程压测：

```bash
git submodule update --init --recursive

uv run python scripts/load_test_live.py \
  --url http://192.0.2.10:2052/mc \
  --stages 10,20,40 \
  --stage-duration 300 \
  --report-hz 10 \
  --allow-remote \
  --expected-build team-view-relay-rust-v1.2.0-alpha.3-proto0.8.0
```

`--expected-build` 必须与目标 `/health` 返回的 `buildVersion` 完全一致，而不是 Docker tag。可先检查：

```bash
curl http://192.0.2.10:2052/mc/health
```

默认压测房间为 `load-benchmark-v3`，脚本拒绝使用 `default` 房间。每档用户数会创建等量的 Mod 上报端和 Web 消费端，外加一个全局上报源，所以 `10,20,40` 分别对应 21、41、81 条 WebSocket 连接。

## 运行配置

状态超时、广播频率、拥塞降级、Tab History 和同服过滤配置位于：

```text
config/server_state_config.toml
```

该文件通过 `include_str!` 编译进二进制，修改后需要重新构建后端。

战区历史默认保留 7,200 秒，并通过 `battleChunkCacheMaxEntries = 65536` 限制每个房间的最大
区块数。缓存使用无损值共享；达到时间或数量任一上限时淘汰最老区块。

## 项目结构

```text
.
├── src/                         Rust 后端源码与测试
├── migrations/                  SQLite migration
├── config/                      服务端状态配置
├── admin-ui/                    Vue 管理页面及 Vitest 测试
├── deploy/                      反向代理配置示例
├── docs/                        设计与演进文档
├── scripts/                     独立黑盒压测工具
├── third_party/
│   └── TeamViewRelay-Protocol/  commit 锁定的共享协议 submodule
├── Cargo.toml
├── Dockerfile
└── docker-compose.yml
```

## 协议依赖

共享协议源固定来自：

```text
third_party/TeamViewRelay-Protocol/proto/teamviewer/v1/teamviewer.proto
```

仓库不会复制或手改 `.proto`。Rust binding 由 `build.rs` 在 Cargo 构建时生成到 `OUT_DIR`，不提交生成产物。

协议升级必须显式锁定 tag 或 commit：

```bash
git -C third_party/TeamViewRelay-Protocol fetch --tags
git -C third_party/TeamViewRelay-Protocol checkout proto/vX.Y.Z
git add third_party/TeamViewRelay-Protocol

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

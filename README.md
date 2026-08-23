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

## Docker

构建当前源码：

```bash
docker build -t teamviewrelay-backend:local .
```

已发布镜像：

```text
professornuo/teamviewrelay-rust:v1.0.3-proto0.7.1
```

`docker-compose.yml` 默认使用该版本，并将 SQLite 数据目录挂载到宿主机的 `./data-rust`。

常用环境变量：

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `TEAMVIEWER_PORT` | `8765` | 服务监听端口 |
| `TEAMVIEWER_DB_PATH` | `./data/teamviewer-admin.db` | SQLite 数据库路径 |
| `TEAMVIEWER_ADMIN_USERNAME` | `admin` | 管理员用户名 |
| `TEAMVIEWER_ADMIN_PASSWORD` | `admin` | 管理员密码，生产环境必须覆盖 |
| `TEAMVIEWER_ADMIN_SESSION_TTL_SEC` | `43200` | 管理会话有效期 |
| `TEAMVIEWER_TRUST_PROXY_HEADERS` | `false` | 是否读取可信反代转发的真实 IP |
| `TEAMVIEWER_TRUSTED_PROXY_CIDRS` | 本机与 Docker 私网段 | 可被信任的直连反代地址段 |
| `RUST_LOG` | `info` | Rust 日志过滤规则 |
| `TZ` | 系统时区 | 管理统计使用的时区 |

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
  --url http://36.150.231.125:2052/mc \
  --stages 10,20,40 \
  --stage-duration 300 \
  --report-hz 10 \
  --allow-remote \
  --expected-build team-view-relay-rust-v1.0.3-proto0.7.1
```

`--expected-build` 必须与目标 `/health` 返回的 `buildVersion` 完全一致，而不是 Docker tag。可先检查：

```bash
curl http://36.150.231.125:2052/mc/health
```

默认压测房间为 `load-benchmark-v3`，脚本拒绝使用 `default` 房间。每档用户数会创建等量的 Mod 上报端和 Web 消费端，外加一个全局上报源，所以 `10,20,40` 分别对应 21、41、81 条 WebSocket 连接。

## 运行配置

状态超时、广播频率、拥塞降级、Tab History 和同服过滤配置位于：

```text
config/server_state_config.toml
```

该文件通过 `include_str!` 编译进二进制，修改后需要重新构建后端。

## 项目结构

```text
.
├── src/                         Rust 后端源码与测试
├── migrations/                  SQLite migration
├── config/                      服务端状态配置
├── admin-ui/                    Vue 管理页面及 Vitest 测试
├── deploy/                      反向代理配置示例
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

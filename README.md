# TeamViewRelay Backend

TeamViewRelay 的后端聚合服务，基于 FastAPI + WebSocket。它负责接收 Minecraft 客户端上报的数据，按房间号（`roomCode`）聚合并广播给游戏内客户端和网页地图端。

相关组件：

- [Minecraft_TeamViewer](https://github.com/MC-TeamViewer/Minecraft_TeamViewer)：Minecraft 客户端 Mod
- [Minecraft-TeamViewer-Web-Script](https://github.com/MC-TeamViewer/Minecraft-TeamViewer-Web-Script)：网页地图投影脚本
- [map-nodemc-plugin-blocker](https://github.com/MC-TeamViewer/map-nodemc-plugin-blocker)：可选的 NodeMC 页面屏蔽脚本，不依赖本后端

## 项目简介

后端的核心职责：

- 接收玩家客户端上报的玩家、实体、路标、战局区块等状态
- 按 `roomCode` 分房广播
- 为网页地图脚本提供 `/web-map/ws` WebSocket 通道
- 提供状态快照、健康检查和兼容路由
- 使用共享 ProtoBuf 协议进行二进制收发

## 适用场景 / 与其他项目关系

- Mod 通过本后端共享房间内团队状态。
- 网页地图脚本通过本后端订阅同一房间的地图投影数据。
- 不运行后端时，Mod 和网页地图脚本无法跨客户端同步状态。

## 快速开始

1. 安装 Python `3.12+`。
2. 在仓库根目录执行 `uv sync` 安装依赖。
3. 在 `admin-ui/` 目录执行 `pnpm install && pnpm build` 构建后台管理页静态资源。
4. 运行 `uv run src/main.py` 启动服务。
5. 让 Mod 连接 `ws://127.0.0.1:8765/mc-client`。
6. 让网页地图脚本连接 `ws://127.0.0.1:8765/web-map/ws`。

## 安装 / 运行

环境要求：

- Python `>=3.12`
- 推荐使用 `uv`

启动命令：

```bash
uv sync
cd admin-ui && pnpm install && pnpm build && cd ..
uv run src/main.py
```

默认监听地址：

- `0.0.0.0:8765`

## 配置或使用说明

### 端点说明

当前可用端点：

- `/mc-client`：当前推荐的玩家客户端 WebSocket 入口
- `/playeresp`：兼容别名，保留给旧客户端
- `/web-map/ws`：网页地图 WebSocket 入口
- `/adminws`：已弃用兼容别名，会提示迁移到 `/web-map/ws`
- `/admin/ws`：预留管理接口，当前仅占位
- `/admin`：Basic Auth 保护的内置后台页面
- `/admin/api/overview`：当前在线概况
- `/admin/api/events`：后台管理页 SSE 实时事件流
- `/admin/api/metrics/daily`：日活趋势
- `/admin/api/metrics/hourly`：小时活跃趋势
- `/admin/api/audit`：审计日志查询，支持 `actorTypes` 多值筛选
- `/health`：健康检查
- `/snapshot`：状态快照调试接口

推荐接入方式：

- Minecraft Mod 连接 `/mc-client`
- 网页地图脚本连接 `/web-map/ws`

### 运行配置

运行时配置文件：

- `src/server/server_state_config.toml`

这个文件主要控制：

- 玩家、实体、路标、战局区块等对象超时
- digest 间隔和广播频率
- 广播拥塞降级阈值
- 同服过滤（Tab 列表归并）相关行为

如果你要调整“多久清理离线对象”“广播频率多高”“同服过滤是否默认启用”，优先看这个文件。

### 后台管理配置

后台管理依赖以下环境变量：

- `TEAMVIEWER_ADMIN_USERNAME`
- `TEAMVIEWER_ADMIN_PASSWORD`
- `TEAMVIEWER_DB_PATH`：SQLite 文件路径，默认当前工作目录下的 `teamviewer-admin.db`
- `TEAMVIEWER_TRUST_PROXY_HEADERS`：是否信任反代传入的真实客户端 IP 头，默认 `false`
- `TEAMVIEWER_TRUSTED_PROXY_CIDRS`：允许被信任的反代源地址段，默认 `127.0.0.1/32,::1/128,172.16.0.0/12`
- `TEAMVIEWER_AUDIT_RETENTION_DAYS`
- `TEAMVIEWER_HOURLY_RETENTION_DAYS`
- `TEAMVIEWER_DAILY_RETENTION_DAYS`
- `TZ`：统计切日和切小时使用的本地时区

统计口径：

- DAU 按玩家 `submitPlayerId` 去重
- 小时数据按整点小时桶统计唯一玩家
- `roomCode` 过滤时按房间统计；不传时按全局玩家去重

审计覆盖：

- 玩家握手成功/失败、断开
- web-map 握手成功/失败、断开
- 管理页/API 鉴权成功/失败
- 管理页/API 访问
- 关键后台错误

后台页面刷新策略：

- 首屏优先等待 `/admin/api/events` 的 `bootstrap` 事件，1.2 秒内未收到时才回退到普通 HTTP 拉取
- 后续通过 `/admin/api/events` 的 SSE 流实时更新当前连接状态、房间概览、DAU、小时活跃和审计日志
- SSE 连接会携带当前审计筛选和指标筛选参数，避免首屏和切换筛选时重复拉取整页数据
- 当前连接状态会细分到每个已登记连接，显示类型、名字、房间、协议版本、程序版本和地址
- 页面顶部会展示轻量可观测信息，包括 SSE 订阅数、最近一次 retention 清理结果、admin API/SSE 错误计数，以及当前是否启用可信代理头
- 管理页前端使用 `Vue 3 + Vite + Element Plus + ECharts` 构建，构建产物由后端直接托管到 `/admin`

### OpenResty / Nginx 反向代理与真实 IP

如果前面挂了 OpenResty / Nginx，后台默认只会看到反代地址。要让审计日志和当前连接状态里的 `remoteAddr` 变成真实客户端 IP，需要同时满足两件事：

- 反代把 `X-Forwarded-For` 和 `X-Real-IP` 传给后端
- 后端显式开启 `TEAMVIEWER_TRUST_PROXY_HEADERS=true`，并把你的反代源地址段写进 `TEAMVIEWER_TRUSTED_PROXY_CIDRS`

推荐 Docker Compose 环境变量：

```bash
export TEAMVIEWER_TRUST_PROXY_HEADERS=true
export TEAMVIEWER_TRUSTED_PROXY_CIDRS=127.0.0.1/32,::1/128,172.16.0.0/12
docker compose up -d --build
```

项目内附了一个可直接改的 OpenResty 示例：

- `deploy/openresty-teamviewer.conf.example`

关键点：

- 所有 HTTP、SSE、WebSocket 请求都反代到 `http://127.0.0.1:8765`
- `/admin/api/events` 关闭 `proxy_buffering`
- 统一转发 `X-Real-IP`、`X-Forwarded-For`、`X-Forwarded-Proto`

如果你的 OpenResty 前面还有 Cloudflare 之类的上游代理，应该先在 OpenResty 层把真实访客 IP 还原到 `$remote_addr`，再把它转发给本后端。

## Docker Compose 部署

项目内置了 `docker-compose.yml`，最小启动方式：

```bash
docker compose up -d --build
```

默认暴露：

- `http://127.0.0.1:8765/admin`
- `ws://127.0.0.1:8765/mc-client`
- `ws://127.0.0.1:8765/web-map/ws`

建议至少覆盖这两个变量：

```bash
export TEAMVIEWER_ADMIN_USERNAME=admin
export TEAMVIEWER_ADMIN_PASSWORD=please-change-me
docker compose up -d --build
```

Compose 默认会：

- 把 SQLite 数据库直接映射到宿主机 `./data/teamviewer-admin.db`
- 通过 `TZ` 控制报表时区，默认 `Asia/Shanghai`
- 把容器内数据库路径固定为 `/app/data/teamviewer-admin.db`

### 与其他组件如何协作

- Mod 负责上报玩家、实体、报点和共享路标
- 后端负责聚合、兼容校验、广播和快照
- 网页地图脚本负责把后端状态渲染到 squaremap 页面

## 常见问题

### Mod 连不上后端

- 先确认后端已经启动，并监听在 `8765`
- 再确认 Mod 使用的是 `ws://127.0.0.1:8765/mc-client`
- 如果 Mod 仍使用默认的 `8080` 端口，会直接连接失败

### 网页地图脚本没有投影

- 确认脚本连接的是 `/web-map/ws`
- 确认脚本和 Mod 的 `roomCode` 一致
- 可用 `/health` 和 `/snapshot` 辅助排查

### 老客户端还能不能走 `/playeresp` 或 `/adminws`

- 目前兼容路由仍然存在，但推荐迁移到 `/mc-client` 和 `/web-map/ws`

## 开发与构建

常用命令：

```bash
uv sync
uv run src/main.py
uv run pytest -q
cd admin-ui && pnpm install && pnpm build && pnpm test
docker compose up -d --build
```

管理页前端本地联调：

```bash
cd admin-ui
pnpm install
pnpm dev
```

Vite 开发服务器会把 `/admin/api/*`、`/admin/api/events` 和 `/admin` 代理到本地 `127.0.0.1:8765`。

协议代码生成：

```bash
./scripts/generate_proto_python.sh
```

主要目录：

- `src/main.py`：服务入口和路由
- `src/server/`：状态、广播、协议和编解码逻辑
- `admin-ui/`：Vue 管理端源码和 Vitest 测试
- `tests/`：后端测试

## 协议 / 版本兼容

当前协议常量位于 `src/main.py`：

- `NETWORK_PROTOCOL_VERSION = 0.6.1`
- `SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION = 0.6.1`

共享 ProtoBuf 协议源位于：

- `third_party/TeamViewRelay-Protocol/proto/teamviewer/v1/teamviewer.proto`

Python 协议产物通过下面脚本生成：

- `scripts/generate_proto_python.sh`

子模块与协议版本：

- 推荐使用 `git clone --recursive`
- 已有仓库可执行 `git submodule update --init --recursive`
- 当前依赖锁定在 `third_party/TeamViewRelay-Protocol` 的指定 commit，不会自动跟随远端更新

升级协议版本的常规流程：

```bash
git -C third_party/TeamViewRelay-Protocol fetch --tags
git -C third_party/TeamViewRelay-Protocol checkout proto/v0.6.1
git add third_party/TeamViewRelay-Protocol
./scripts/generate_proto_python.sh
uv run pytest -q
```

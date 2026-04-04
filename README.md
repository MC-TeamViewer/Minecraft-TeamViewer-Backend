# TeamViewRelay Backend

TeamViewRelay 的后端聚合服务，基于 FastAPI + WebSocket。

## 功能

- 接收客户端上报的玩家/实体/路标数据
- 按 `roomCode` 分房广播
- 提供 `/web-map/ws` 网页地图通道（状态快照、观察端指令）
- 预留 `/admin/ws` 真后台管理通道（当前仅占位）
- 支持增量同步（`snapshot_full` / `patch` / `digest`）

## 环境

- Python `>=3.12`
- 推荐使用 `uv`

## 启动

```bash
cd backend-server/src
uv run main.py
```

默认监听：`0.0.0.0:8765`

## 关键端点

- 玩家 WS：`/playeresp`
- 网页地图 WS：`/web-map/ws`
- 兼容别名：`/adminws`（仅开发期保留，正式发布前移除）
- 后台管理 WS：`/admin/ws`（当前仅占位）
- 健康检查：`/health`
- 快照调试：`/snapshot`

## 运行配置

配置文件：`src/server/server_state_config.toml`

可配置项包括：

- 对象超时（玩家 / 实体 / 路标）
- digest 间隔
- 广播频率与拥塞降级阈值
- 同服过滤开关（Tab 列表归并）

## 协议版本

当前服务端协议常量位于 `src/main.py`：

- `NETWORK_PROTOCOL_VERSION = 0.6.0`
- `SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION = 0.6.0`

共享 ProtoBuf 协议源位于 `third_party/TeamViewRelay-Protocol/proto/teamviewer/v1/teamviewer.proto`，服务端通过 `scripts/generate_proto_python.sh` 生成本地 Python 产物。

详细字段与报文结构见仓库根目录文档：`docs/PLAYER_ESP_NETWORK_PROTOCOL.md`

## 子模块与协议版本

- clone 推荐使用：`git clone --recursive`
- 已有仓库补拉子模块：`git submodule update --init --recursive`
- 当前仓库依赖的是被锁定的协议 submodule commit，不会自动跟随协议仓库远端更新
- 升级协议版本的标准流程：

```bash
git -C third_party/TeamViewRelay-Protocol fetch --tags
git -C third_party/TeamViewRelay-Protocol checkout proto/v0.6.0
git add third_party/TeamViewRelay-Protocol
./scripts/generate_proto_python.sh
UV_CACHE_DIR=/tmp/uv-cache uv run pytest -q
```

- GitHub “Download ZIP” 不包含 submodule 内容，不是推荐的开发方式

# Repository Guidelines

## Protocol Dependency

- 共享协议源来自 `third_party/TeamViewRelay-Protocol`
- 本仓库不能重新创建、复制或手改 `.proto`
- `third_party/TeamViewRelay-Protocol` 是被主仓库 commit 锁定的 submodule，不是“自动追最新”的依赖

## Protocol Upgrade Workflow

1. `git -C third_party/TeamViewRelay-Protocol fetch --tags`
2. `git -C third_party/TeamViewRelay-Protocol checkout proto/vX.Y.Z`
3. `git add third_party/TeamViewRelay-Protocol`
4. `./scripts/generate_proto_python.sh`
5. `uv sync --extra dev --extra test`
6. `UV_CACHE_DIR=/tmp/uv-cache uv run pytest -q`

如果只是修改后端业务逻辑，不要顺手升级协议 submodule。

## AI Guidance

- 看到协议问题时，优先检查 submodule 是否初始化、是否锁到预期 tag、是否重新生成代码。
- 不要在本仓库直接修改协议定义。
- 不要执行“submodule pull 最新 main”这类操作；协议升级必须显式锚定 tag 或 commit。

# 从 Python Compose 迁移到 Rust

## 版本与镜像

- Python hotfix 归档分支：`python-hotfix-v0.5.14`
- Python 镜像：`professornuo/team-view-relay:v0.5.14-proto0.7.0.hotfix`
- Rust 版本：`v1.0.0-proto0.7.0`
- Rust 镜像：`ghcr.io/mc-teamviewer/minecraft-teamviewer-backend-rust:v1.0.0-proto0.7.0`

Rust 使用 `Dockerfile.rust` 构建，并由 main 分支的 GitHub Actions 发布到独立
镜像仓库，不会覆盖 Python 镜像。

## 数据迁移原则

Rust 使用全新的 SQLite 数据库，不读取或迁移 Python 的数据库。默认目录从
`./data` 改为 `./data-rust`。旧目录必须保留，以便随时回滚 Python。

切换后，玩家、实体、路径点和 Last Seen 等内存状态由 Mod 与外部数据源重新
上报；Python 数据库中的管理统计、审计日志和 Tab History 不会出现在 Rust
数据库中。

## 原端口直接切换

main 分支的 `docker-compose.yml` 已经是 Rust 配置，仍然对外监听 8765，因此
反向代理和客户端地址不需要修改。

```bash
# 1. 在更新代码前备份旧配置和数据库；不要使用 docker compose down -v。
cp docker-compose.yml docker-compose.python.backup.yml
cp -a data data.python.backup

# 2. 停止 Python，并获取 main 的 Rust 版本。
docker compose down
git fetch origin --tags
git switch main
git pull --ff-only origin main

# 3. 保留原管理账号环境变量，启动独立 Rust 数据目录。
mkdir -p data-rust
docker compose pull
docker compose up -d

# 4. 验证版本和日志。
curl --fail http://127.0.0.1:8765/health
docker compose logs --tail=100 backend
```

健康检查中的 `buildVersion` 应为
`team-view-relay-rust-v1.0.0-proto0.7.0`。

如果 GHCR 镜像为私有包，先执行：

```bash
echo "$GHCR_TOKEN" | docker login ghcr.io -u YOUR_GITHUB_USER --password-stdin
```

也可以设置 `TEAMVIEWER_RUST_IMAGE` 使用自己构建或镜像到其他仓库的版本。

## 先运行 canary

`docker-compose.rust.yml` 默认映射到宿主机 8766，可与仍在 8765 的 Python
并行运行：

```bash
TEAMVIEWER_RUST_PORT=8766 docker compose -f docker-compose.rust.yml up -d
curl --fail http://127.0.0.1:8766/health
```

canary 同样只使用 `./data-rust`，不要把 Python 与 Rust 指向同一个 SQLite 文件。

## 回滚 Python

```bash
docker compose down
git fetch origin
git switch python-hotfix-v0.5.14
docker compose up -d
curl --fail http://127.0.0.1:8765/health
```

Python 分支仍挂载原来的 `./data`。由于 Rust 使用 `./data-rust`，回滚不需要
执行数据库恢复。

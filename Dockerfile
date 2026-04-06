FROM node:24-bookworm-slim AS admin-ui-build

WORKDIR /admin-ui

RUN corepack enable

COPY admin-ui/package.json admin-ui/pnpm-lock.yaml admin-ui/tsconfig.json admin-ui/vite.config.ts admin-ui/index.html /admin-ui/
COPY admin-ui/src /admin-ui/src

RUN pnpm install --frozen-lockfile
RUN pnpm build

FROM astral/uv:python3.13-bookworm-slim

WORKDIR /app

COPY pyproject.toml uv.lock README.md /app/
COPY src /app/src
COPY --from=admin-ui-build /admin-ui/dist /app/admin-ui/dist

RUN uv sync --frozen --no-dev

EXPOSE 8765

CMD ["uv", "run", "src/main.py"]

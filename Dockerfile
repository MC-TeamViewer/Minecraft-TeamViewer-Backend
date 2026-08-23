ARG NODE_IMAGE=node:24-bookworm-slim
ARG RUST_IMAGE=rust:1.94-bookworm
ARG DEBIAN_IMAGE=debian:bookworm-slim

FROM ${NODE_IMAGE} AS admin-ui-build
WORKDIR /admin-ui
RUN corepack enable
COPY admin-ui/package.json admin-ui/pnpm-lock.yaml admin-ui/tsconfig.json admin-ui/vite.config.ts admin-ui/index.html ./
COPY admin-ui/src ./src
RUN pnpm install --frozen-lockfile && pnpm build

FROM ${RUST_IMAGE} AS backend-build
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY migrations ./migrations
COPY config ./config
COPY third_party/TeamViewRelay-Protocol ./third_party/TeamViewRelay-Protocol
COPY --from=admin-ui-build /admin-ui/dist ./admin-ui/dist
RUN cargo build --locked --release

FROM ${DEBIAN_IMAGE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=backend-build /build/target/release/teamviewrelay-rust /app/teamviewrelay-rust
EXPOSE 8765
CMD ["/app/teamviewrelay-rust"]

ARG NODE_IMAGE=node:24-bookworm-slim
ARG RUST_IMAGE=rust:1.94-bookworm
ARG DEBIAN_IMAGE=debian:bookworm-slim

FROM ${NODE_IMAGE} AS admin-ui-build
WORKDIR /admin-ui
RUN corepack enable
COPY admin-ui/package.json admin-ui/pnpm-lock.yaml admin-ui/tsconfig.json admin-ui/vite.config.ts admin-ui/index.html ./
COPY admin-ui/src ./src
RUN pnpm install --frozen-lockfile && pnpm build

FROM ${RUST_IMAGE} AS backend-build-base
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY migrations ./migrations
COPY config ./config
COPY third_party/TeamViewRelay-Protocol ./third_party/TeamViewRelay-Protocol
COPY --from=admin-ui-build /admin-ui/dist ./admin-ui/dist

FROM backend-build-base AS backend-build
RUN cargo build --locked --release

FROM backend-build-base AS backend-memory-debug-build
ENV RUSTFLAGS="-C force-frame-pointers=yes"
RUN cargo build --locked --profile memory-debug --features memory-debug

FROM ${DEBIAN_IMAGE} AS runtime-base
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
EXPOSE 8765/tcp
EXPOSE 8766/udp
EXPOSE 8767/udp
CMD ["/app/teamviewrelay-rust"]

FROM runtime-base AS memory-debug
COPY --from=backend-memory-debug-build /build/target/memory-debug/teamviewrelay-rust /app/teamviewrelay-rust
ENV TEAMVIEWER_DEBUG_DIR=/app/data/memory-debug

FROM runtime-base AS production
COPY --from=backend-build /build/target/release/teamviewrelay-rust /app/teamviewrelay-rust

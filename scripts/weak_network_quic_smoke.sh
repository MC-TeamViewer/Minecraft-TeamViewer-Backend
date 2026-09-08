#!/usr/bin/env bash
# Phase 4 弱网验证(QUIC 门,用户态代理,无需 root)。
#
# 用 udp_loss_proxy 在 loopback 上逐包注入丢包/延迟/抖动,让 quic-smoke
# (+zstd 与 plain 两条 ALPN)在每种弱网档位下完整走一遍
# 握手 → handshake_ack → resync → snapshot_full。QUIC 在代理丢包下
# 自行重传;zstd 连续流语义不应受包序/重传影响(QUIC 流内保序)。
#
# 前提:本地后端已在 8767/udp 监听 QUIC 门。
# 用法:bash scripts/weak_network_quic_smoke.sh
set -euo pipefail

cd "$(dirname "$0")/.."

PROXY_PORT_BASE=18770
PROXY_PID=""

cleanup() {
    if [[ -n "$PROXY_PID" ]] && kill -0 "$PROXY_PID" 2>/dev/null; then
        kill "$PROXY_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

run_profile() {
    local label="$1" drop="$2" delay_ms="$3" jitter_ms="$4"
    local port=$((PROXY_PORT_BASE + RANDOM % 100))
    python3 scripts/udp_loss_proxy.py \
        --listen "127.0.0.1:$port" --upstream 127.0.0.1:8767 \
        --drop "$drop" --delay-ms "$delay_ms" --jitter-ms "$jitter_ms" \
        >"/tmp/tv-weaknet-$port.log" 2>&1 &
    PROXY_PID=$!
    sleep 1
    echo "=== 档位 $label(drop=$drop delay=${delay_ms}ms±${jitter_ms}ms,端口 $port)==="
    if TEAMVIEWER_QUIC_SMOKE_SERVER="127.0.0.1:$port" \
        ./target/debug/examples/quic-smoke; then
        echo "=== 档位 $label:PASS ==="
    else
        echo "=== 档位 $label:FAIL ==="
        exit 1
    fi
    kill "$PROXY_PID" 2>/dev/null || true
    wait "$PROXY_PID" 2>/dev/null || true
    PROXY_PID=""
}

echo "构建 quic-smoke ..."
cargo build --example quic-smoke -j 2

run_profile "基线(无损伤)" 0 0 0
run_profile "轻度弱网" 0.05 40 15
run_profile "重度弱网" 0.15 80 30
echo "QUIC 门弱网矩阵全部通过"

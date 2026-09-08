#!/usr/bin/env bash
# 突发流量压测:受限外部带宽(令牌桶 UDP 代理,默认 100KiB/s)下,
# 经 bulk 传输通道下发 10 MiB 低优先级大内容,验证:
#   ① 拥塞控制自主探测瓶颈(端点零带宽假设,全部默认参数)
#   ② bulk 饱和期高优 ping RTT 有界,不饿死
#   ③ 门控心跳 datagram 送达率(不敏感语义)
# 矩阵:QUIC / WebTransport × 直连 / 限速代理。
#
# 用法:bash scripts/quic_burst_test.sh
# 环境覆盖:RATE_KIB_PER_S(默认 100)、BULK_BYTES(默认 10 MiB)
set -euo pipefail

cd "$(dirname "$0")/.."

RATE_KIB_PER_S="${RATE_KIB_PER_S:-100}"
BULK_BYTES="${BULK_BYTES:-$((10 * 1024 * 1024))}"
ROOM="${ROOM:-burst-room}"

WS_PORT=18765      # 后端主端口(WS + admin api)
WT_PORT=18766      # WebTransport 门(udp)
QUIC_PORT=18767    # 裸 QUIC 门(udp)
WT_PROXY_PORT=18666
QUIC_PROXY_PORT=18677

CERT_DIR="/tmp/tv-burst/certs"
LOG_DIR="/tmp/tv-burst"
BACKEND_PID=""
PROXY_PIDS=()

cleanup() {
    [[ -n "$BACKEND_PID" ]] && kill "$BACKEND_PID" 2>/dev/null || true
    for pid in "${PROXY_PIDS[@]:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

wait_for() {
    local pattern="$1" file="$2" label="$3"
    for _ in $(seq 1 60); do
        grep -q "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.5
    done
    echo "等待 $label 超时" >&2
    return 1
}

trigger_bulk() {
    local jar="$LOG_DIR/admin-cookie.jar"
    curl -sf -c "$jar" -X POST http://127.0.0.1:$WS_PORT/admin/api/session/login \
        -H 'Content-Type: application/json' \
        -d '{"username":"burst","password":"burst-token"}' >/dev/null
    curl -sf -b "$jar" -X POST http://127.0.0.1:$WS_PORT/admin/api/debug/bulk-push \
        -H 'Content-Type: application/json' \
        -d "{\"bytes\":$BULK_BYTES,\"contentType\":\"x-teamviewrelay/burst\"}"
}

run_case() {
    local label="$1" client="$2" server_addr="$3"
    shift 3
    local log="$LOG_DIR/burst-$label.log"
    echo "=== 案例 $label(客户端=$client server=$server_addr)==="
    env TEAMVIEWER_BURST_SERVER="$server_addr" \
        TEAMVIEWER_BURST_ROOM="$ROOM-$label" \
        TEAMVIEWER_BURST_CERT_SHA256="$CERT_SHA256" \
        "$@" ./target/debug/examples/"$client" >"$log" 2>&1 &
    local client_pid=$!
    # 等客户端握手就绪(基线相位开始)再触发 bulk
    if ! wait_for "READY" "$log" "客户端就绪($label)"; then
        cat "$log" >&2
        kill "$client_pid" 2>/dev/null || true
        return 1
    fi
    if ! trigger_bulk >/dev/null; then
        echo "bulk 触发失败($label)" >&2
        kill "$client_pid" 2>/dev/null || true
        return 1
    fi
    if wait "$client_pid"; then
        grep "VERDICT_JSON" "$log" | tail -1
        echo "=== 案例 $label:PASS ==="
    else
        echo "案例 $label 客户端失败:" >&2
        cat "$log" >&2
        return 1
    fi
}

echo "构建 quic-burst / wt-burst 与后端(bulk 触发端点在 memory-debug 特性下)..."
cargo build --bin teamviewrelay-rust --features memory-debug \
    --example quic-burst --example wt-burst -j 2

mkdir -p "$CERT_DIR" "$LOG_DIR"
if [[ ! -f "$CERT_DIR/fullchain.pem" ]]; then
    echo "生成自签证书 ..."
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
        -keyout "$CERT_DIR/privkey.pem" -out "$CERT_DIR/fullchain.pem" \
        -days 14 -nodes -subj "/CN=burst" >/dev/null 2>&1
fi
CERT_SHA256=$(openssl x509 -in "$CERT_DIR/fullchain.pem" -outform DER | sha256sum | cut -d' ' -f1)

echo "启动后端(WS $WS_PORT / QUIC $QUIC_PORT / WT $WT_PORT)..."
TEAMVIEWER_PORT=$WS_PORT \
TEAMVIEWER_DB_PATH="$LOG_DIR/exp.db" \
TEAMVIEWER_ADMIN_USERNAME=burst TEAMVIEWER_ADMIN_PASSWORD=burst-token \
TEAMVIEWER_QUIC_ENABLED=true TEAMVIEWER_QUIC_BIND="127.0.0.1:$QUIC_PORT" \
TEAMVIEWER_QUIC_CERT_PATH="$CERT_DIR/fullchain.pem" \
TEAMVIEWER_QUIC_KEY_PATH="$CERT_DIR/privkey.pem" \
TEAMVIEWER_WT_ENABLED=true TEAMVIEWER_WT_BIND="127.0.0.1:$WT_PORT" \
TEAMVIEWER_WT_CERT_PATH="$CERT_DIR/fullchain.pem" \
TEAMVIEWER_WT_KEY_PATH="$CERT_DIR/privkey.pem" \
    ./target/debug/teamviewrelay-rust >"$LOG_DIR/backend.log" 2>&1 &
BACKEND_PID=$!
wait_for "listening\|endpoint" "$LOG_DIR/backend.log" "后端启动"
sleep 1

echo "启动限速代理(${RATE_KIB_PER_S}KiB/s,上游→客户端方向)..."
python3 scripts/udp_loss_proxy.py \
    --listen "127.0.0.1:$QUIC_PROXY_PORT" --upstream "127.0.0.1:$QUIC_PORT" \
    --drop 0 --delay-ms 0 --jitter-ms 0 --rate-kib-per-s "$RATE_KIB_PER_S" \
    >"$LOG_DIR/proxy-quic.log" 2>&1 &
PROXY_PIDS+=($!)
python3 scripts/udp_loss_proxy.py \
    --listen "127.0.0.1:$WT_PROXY_PORT" --upstream "127.0.0.1:$WT_PORT" \
    --drop 0 --delay-ms 0 --jitter-ms 0 --rate-kib-per-s "$RATE_KIB_PER_S" \
    >"$LOG_DIR/proxy-wt.log" 2>&1 &
PROXY_PIDS+=($!)
sleep 1

echo "bulk 大小:$BULK_BYTES 字节;速率上限:${RATE_KIB_PER_S}KiB/s(仅代理档)"
FAIL=0
run_case "quic-direct" quic-burst "127.0.0.1:$QUIC_PORT" || FAIL=1
run_case "wt-direct" wt-burst "127.0.0.1:$WT_PORT" || FAIL=1
run_case "quic-rate" quic-burst "127.0.0.1:$QUIC_PROXY_PORT" || FAIL=1
run_case "wt-rate" wt-burst "127.0.0.1:$WT_PROXY_PORT" || FAIL=1

echo
echo "===== 汇总 ====="
for label in quic-direct wt-direct quic-rate wt-rate; do
    echo "--- $label ---"
    grep "VERDICT_JSON" "$LOG_DIR/burst-$label.log" 2>/dev/null | tail -1 \
        | python3 scripts/burst_summary.py
done

[[ $FAIL -eq 0 ]] && echo "突发流量压测矩阵全部通过" || { echo "突发流量压测存在失败项"; exit 1; }

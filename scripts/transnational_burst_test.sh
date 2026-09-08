#!/usr/bin/env bash
# 跨国线路极端压测:10 MiB bulk @ 100KiB/s 外部带宽,叠加真实洲际线路
# 特征——5%-15% 丢包 + 保底 100ms 延迟 + 0-200ms 单边抖动(单程
# 100-300ms,RTT 200-600ms)。QUIC / WebTransport 双门 × 三档丢包。
#
# 验证点:
#   ① CC 在"带宽×RTT×丢包"三重压力下自主收敛,流数据零失配
#      (QUIC 重传完全兜底 = 不出现未恢复丢失)
#   ② 高优 ping RTT 在极端排队下的分布(p50/p95)
#   ③ 心跳 datagram 丢包率 ≈ 线路丢包率(不敏感语义,允许按比例损耗)
#
# 用法:bash scripts/transnational_burst_test.sh
# 环境覆盖:RATE_KIB_PER_S(默认 100)、BULK_BYTES(默认 10 MiB)、
#           LOSSES(默认 "5 10 15",百分数)、DELAY_MS(默认 100)、
#           JITTER_MS(默认 200)
set -euo pipefail

cd "$(dirname "$0")/.."

RATE_KIB_PER_S="${RATE_KIB_PER_S:-100}"
BULK_BYTES="${BULK_BYTES:-$((10 * 1024 * 1024))}"
LOSSES="${LOSSES:-5 10 15}"
DELAY_MS="${DELAY_MS:-100}"
JITTER_MS="${JITTER_MS:-200}"
ROOM="${ROOM:-transnational}"

WS_PORT=18765
WT_PORT=18766
QUIC_PORT=18767
WT_PROXY_PORT=18666
QUIC_PROXY_PORT=18677

CERT_DIR="/tmp/tv-burst/certs"
LOG_DIR="/tmp/tv-burst"
BACKEND_PID=""
PROXY_PIDS=()
CLIENT_PIDS=()

cleanup() {
    for pid in "${CLIENT_PIDS[@]:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done
    for pid in "${PROXY_PIDS[@]:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done
    [[ -n "$BACKEND_PID" ]] && kill "$BACKEND_PID" 2>/dev/null || true
}
trap cleanup EXIT

wait_for() {
    local pattern="$1" file="$2" label="$3"
    for _ in $(seq 1 120); do
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
    local label="$1" client="$2" server_addr="$3" loss="$4"
    shift 4
    local log="$LOG_DIR/burst-$label.log"
    echo "=== 案例 $label(客户端=$client server=$server_addr loss=${loss}% delay=${DELAY_MS}+0-${JITTER_MS}ms)==="
    env TEAMVIEWER_BURST_SERVER="$server_addr" \
        TEAMVIEWER_BURST_ROOM="$ROOM-$label" \
        TEAMVIEWER_BURST_CERT_SHA256="$CERT_SHA256" \
        TEAMVIEWER_BURST_DGRAM_LOSS_TOLERANCE="$(python3 -c "print($loss / 100 * 2 + 0.05)")" \
        "$@" ./target/debug/examples/"$client" >"$log" 2>&1 &
    local client_pid=$!
    CLIENT_PIDS+=("$client_pid")
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

echo "bulk 大小:$BULK_BYTES 字节;速率上限:${RATE_KIB_PER_S}KiB/s;延迟 ${DELAY_MS}+0-${JITTER_MS}ms(单程)"
FAIL=0
for loss in $LOSSES; do
    echo "启动 ${loss}% 丢包代理 ..."
    python3 scripts/udp_loss_proxy.py \
        --listen "127.0.0.1:$QUIC_PROXY_PORT" --upstream "127.0.0.1:$QUIC_PORT" \
        --drop "0.$(printf %02d "$loss")" --delay-ms "$DELAY_MS" --jitter-ms "$JITTER_MS" \
        --delay-jitter-mode plus --rate-kib-per-s "$RATE_KIB_PER_S" \
        >"$LOG_DIR/proxy-quic-$loss.log" 2>&1 &
    PROXY_PIDS+=($!)
    python3 scripts/udp_loss_proxy.py \
        --listen "127.0.0.1:$WT_PROXY_PORT" --upstream "127.0.0.1:$WT_PORT" \
        --drop "0.$(printf %02d "$loss")" --delay-ms "$DELAY_MS" --jitter-ms "$JITTER_MS" \
        --delay-jitter-mode plus --rate-kib-per-s "$RATE_KIB_PER_S" \
        >"$LOG_DIR/proxy-wt-$loss.log" 2>&1 &
    PROXY_PIDS+=($!)
    sleep 1

    run_case "quic-loss$loss" quic-burst "127.0.0.1:$QUIC_PROXY_PORT" "$loss" || FAIL=1
    run_case "wt-loss$loss" wt-burst "127.0.0.1:$WT_PROXY_PORT" "$loss" || FAIL=1

    for pid in "${PROXY_PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    PROXY_PIDS=()
    sleep 1
done

echo
echo "===== 跨国极端压测汇总 ====="
for loss in $LOSSES; do
    for door in quic wt; do
        label="${door}-loss$loss"
        echo "--- $label ---"
        grep "VERDICT_JSON" "$LOG_DIR/burst-$label.log" 2>/dev/null | tail -1 \
            | python3 scripts/burst_summary.py || true
    done
done

[[ $FAIL -eq 0 ]] && echo "跨国极端压测矩阵全部通过" || { echo "跨国极端压测存在失败项"; exit 1; }

#!/usr/bin/env bash
# Phase 4 弱网验证(tc netem 版,需 root)——WS(TCP) vs QUIC(UDP) 丢包对比。
#
# 用户态代理只能对 UDP(QUIC)忠实注包;TCP 的丢包效应(TCP 队头阻塞、
# 重传退避)必须在 IP 层注入才真实,故本脚本用 tc netem 对 lo 加损。
#
# 覆盖:
#   1. WS 压测短档(--stages 10 --stage-duration 15,plain 与 zstd)
#   2. QUIC 门 quic-smoke(直连 8767,+zstd 与 plain 双 ALPN)
# 每种 netem 档位(基线/5%丢+40ms/15%丢+80ms)各跑一轮,结束时恢复 qdisc。
#
# 用法:sudo bash scripts/weak_network_netem.sh
# 注意:脚本会临时修改 lo 的 root qdisc,异常中断可用
#   sudo tc qdisc del dev lo root
# 恢复(noqueue 为默认态)。
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ $EUID -ne 0 ]]; then
    echo "需要 root:sudo bash scripts/weak_network_netem.sh" >&2
    exit 1
fi

# 只有 tc 需要 root;cargo/uv/测试以调用者(sudo 前的用户)身份跑,
# 避免 root 在 target/、~/.cache/uv 留下属主污染,破坏后续普通用户构建。
# PATH 取用户登录 shell 的真实 PATH(root 的 secure_path 不含
# ~/.cargo/bin、~/.local/bin,直接降权会找不到 cargo/uv)。
RUN_AS=()
if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
    USER_PATH="$(su - "$SUDO_USER" -c 'echo "$PATH"' 2>/dev/null || true)"
    RUN_AS=(sudo -u "$SUDO_USER" env "HOME=/home/$SUDO_USER" "PATH=${USER_PATH:-$PATH}")
fi

restore() {
    tc qdisc del dev lo root 2>/dev/null || true
    echo "netem 已移除(lo 恢复默认)"
}
trap restore EXIT

apply_profile() {
    local drop="$1" delay_ms="$2" jitter_ms="$3"
    tc qdisc del dev lo root 2>/dev/null || true
    if [[ "$drop" == "0" ]]; then
        echo "--- 档位:基线(无损伤)---"
    else
        tc qdisc add dev lo root netem delay "${delay_ms}ms" "${jitter_ms}ms" loss "$drop"
        echo "--- 档位:loss=$drop delay=${delay_ms}ms±${jitter_ms}ms ---"
    fi
}

run_quic() {
    echo "[QUIC] quic-smoke(直连 8767):"
    "${RUN_AS[@]}" ./target/debug/examples/quic-smoke \
        && echo "[QUIC] PASS" || { echo "[QUIC] FAIL"; return 1; }
}

run_ws() {
    local compression="$1"
    echo "[WS:$compression] load_test_live 短档:"
    "${RUN_AS[@]}" uv run --with websockets --with grpcio-tools --with zstandard \
        python scripts/load_test_live.py \
        --url http://127.0.0.1:8765 \
        --room "weaknet-netem-$compression" \
        --stages 10 --stage-duration 15 --report-hz 10 \
        --compression "$compression" >"/tmp/tv-netem-ws-$compression.json" \
        && { echo "[WS:$compression] PASS"; "${RUN_AS[@]}" python3 - "$compression" <<'PY'
import json, sys
r = json.load(open(f"/tmp/tv-netem-ws-{sys.argv[1]}.json"))
s = r["stages"][0]
print(f"  receivedMiB={s['receivedMiB']:.2f} rttP95={s['pingRttP95Ms']:.0f}ms converged={s['consistency']['converged']}")
PY
} || { echo "[WS:$compression] FAIL"; return 1; }
}

echo "构建 quic-smoke ..."
"${RUN_AS[@]}" cargo build --example quic-smoke -j 2

FAIL=0
for profile in "0 0 0" "0.05 40 15" "0.15 80 30"; do
    set -- $profile
    apply_profile "$1" "$2" "$3"
    run_quic || FAIL=1
    run_ws plain || FAIL=1
    run_ws zstd || FAIL=1
done
# netem 由 trap 恢复
[[ $FAIL -eq 0 ]] && echo "netem 弱网矩阵全部通过" || { echo "netem 弱网矩阵存在失败项"; exit 1; }

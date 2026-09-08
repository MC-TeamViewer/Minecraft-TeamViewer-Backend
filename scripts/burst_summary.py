#!/usr/bin/env python3
"""突发流量压测单案例摘要:从 stdin 读 VERDICT_JSON 行,打印人读表格。"""
import json
import sys

line = sys.stdin.read().strip()
if not line:
    print("  (无结果)")
    raise SystemExit(0)
v = json.loads(line.removeprefix("VERDICT_JSON "))
print(f"  bulk {v['bulk_bytes']/(1024*1024):.1f} MiB in {v['bulk_seconds']:.1f}s = {v['bulk_kib_per_s']:.1f} KiB/s, integrity={v['integrity_ok']}")
print(f"  rtt p95: {v['rtt_baseline_p95_ms']:.0f}ms -> {v['rtt_during_p95_ms']:.0f}ms (x{v['rtt_during_baseline_ratio_p95']:.2f})")
print(f"  heartbeat datagram: {v['datagram_received']}/{v['datagram_expected']} = {v['datagram_delivery_ratio']*100:.1f}% (other={v['datagram_other']})")
print(f"  verdict: {v['verdict']}")

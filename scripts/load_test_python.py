#!/usr/bin/env python3
"""Deterministic in-process stabilization load test.

The workload matches the production acceptance fixture without requiring a running
server: 20 player sockets (one blocked), one external source, one web map, 400
dynamic objects, 2,000 offline positions and 2,000 Tab records.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import statistics
import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace


ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from server.core.broadcaster import Broadcaster  # noqa: E402
from server.state import ServerState  # noqa: E402
from server.tab_history.store import TabHistoryStore, TabHistoryStoreConfig  # noqa: E402
from server.ws.sender import websocket_send_hub  # noqa: E402


CONNECTED = SimpleNamespace(name="CONNECTED")


class LoadSocket:
    def __init__(self, *, blocked: bool = False) -> None:
        self.client_state = CONNECTED
        self.application_state = CONNECTED
        self.blocked = blocked
        self.sent = 0
        self.closed_at: float | None = None

    async def send_bytes(self, _payload: bytes) -> None:
        if self.blocked:
            await asyncio.Event().wait()
        self.sent += 1

    async def close(self, *, code: int, reason: str) -> None:
        del code, reason
        self.closed_at = time.monotonic()
        disconnected = SimpleNamespace(name="DISCONNECTED")
        self.client_state = disconnected
        self.application_state = disconnected


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, int(len(ordered) * fraction) - 1))]


def player_data(index: int, revision: int = 0) -> dict:
    return {
        "x": float(index) + revision * 0.25,
        "y": 64.0,
        "z": float(index),
        "dimension": "minecraft:overworld",
        "playerName": f"Player{index}",
        "playerUUID": f"00000000-0000-0000-0000-{index:012d}",
        "health": 20.0,
        "maxHealth": 20.0,
        "armor": 0.0,
        "isRiding": False,
        "width": 0.6,
        "height": 1.8,
    }


async def seed_tab_history(entries: list[dict]) -> tuple[float, int]:
    with tempfile.TemporaryDirectory(prefix="teamviewer-load-") as temp_dir:
        store = TabHistoryStore(TabHistoryStoreConfig(db_path=str(Path(temp_dir) / "load.db")))
        await store.initialize()
        started = time.perf_counter()
        await store.upsert_players("load-room", [(item["uuid"], item) for item in entries])
        write_duration = time.perf_counter() - started
        synced = await store.sync(
            "load-room",
            preferred_mode="TAB_HISTORY_SYNC_MODE_FULL",
            base_revision=None,
            base_digest=None,
            allow_full_fallback=True,
        )
        await store.close()
        return write_duration, len(synced["upsert"])


async def run(duration_sec: float) -> dict[str, object]:
    state = ServerState()
    broadcaster = Broadcaster(state)
    stamp = time.monotonic()

    for index in range(400):
        state.upsert_report(
            state.player_reports,
            f"online-{index}",
            "external-source",
            state.build_state_node("external-source", stamp, player_data(index)),
        )

    offline = {
        f"00000000-0000-0000-1000-{index:012d}": {
            "x": float(index),
            "y": 64.0,
            "z": float(index),
            "dimension": "minecraft:overworld",
            "playerName": f"Offline{index}",
            "playerUUID": f"00000000-0000-0000-1000-{index:012d}",
            "lastSeenAtUtcMs": 2_000 + index,
            "positionObservedAtUtcMs": 1_000 + index,
            "offlineDetectedAtUtcMs": 3_000 + index,
        }
        for index in range(2_000)
    }
    state.replace_last_seen_players("external-source", "load-room", offline, stamp)
    tab_entries = [
        {
            "uuid": f"00000000-0000-0000-2000-{index:012d}",
            "name": f"Tab{index}",
        }
        for index in range(2_000)
    ]
    tab_write_sec, tab_sync_count = await seed_tab_history(tab_entries)

    source = LoadSocket()
    state.connections["external-source"] = source
    state.set_player_room("external-source", "load-room")
    state.set_connection_identity(
        "external-source", state.CLIENT_ROLE_EXTERNAL_SOURCE, "Load source"
    )
    state.mark_player_capability("external-source", "0.7.0")
    state.upsert_tab_player_report("external-source", tab_entries, stamp)

    normal_sockets: list[LoadSocket] = []
    blocked_socket = LoadSocket(blocked=True)
    for index in range(20):
        player_id = f"viewer-{index}"
        websocket = blocked_socket if index == 19 else LoadSocket()
        if websocket is not blocked_socket:
            normal_sockets.append(websocket)
        state.connections[player_id] = websocket
        state.set_player_room(player_id, "load-room")
        state.mark_player_capability(player_id, "0.7.0")

    web_map = LoadSocket()
    state.web_map_connections["web-map"] = web_map
    state.set_web_map_room("web-map", "load-room")
    state.web_map_connection_protocols["web-map"] = "0.7.0"

    loop_lag: list[float] = []
    monitor_running = True

    async def monitor() -> None:
        expected = time.monotonic() + 0.02
        while monitor_running:
            await asyncio.sleep(0.02)
            now = time.monotonic()
            loop_lag.append(max(0.0, now - expected))
            expected = now + 0.02

    monitor_task = asyncio.create_task(monitor())
    full_started = time.perf_counter()
    await broadcaster.broadcast_updates()
    initial_sync_sec = time.perf_counter() - full_started

    tick_durations: list[float] = []
    deadline = time.monotonic() + max(0.1, duration_sec)
    revision = 0
    while time.monotonic() < deadline:
        revision += 1
        changed_at = time.monotonic()
        for offset in range(50):
            index = (revision * 50 + offset) % 400
            state.upsert_report(
                state.player_reports,
                f"online-{index}",
                "external-source",
                state.build_state_node(
                    "external-source", changed_at, player_data(index, revision)
                ),
            )
        started = time.perf_counter()
        await broadcaster.broadcast_updates()
        tick_durations.append(time.perf_counter() - started)
        await asyncio.sleep(0.1)

    monitor_running = False
    await monitor_task
    sender_stats = websocket_send_hub.snapshot()
    await websocket_send_hub.close()

    return {
        "fixture": {
            "players": 20,
            "externalSources": 1,
            "webMaps": 1,
            "onlineObjects": 400,
            "offlineObjects": 2_000,
            "tabRecords": tab_sync_count,
        },
        "initialSyncSec": initial_sync_sec,
        "tabHistoryWriteSec": tab_write_sec,
        "broadcastTickP95Ms": percentile(tick_durations, 0.95) * 1000.0,
        "broadcastTickMaxMs": max(tick_durations, default=0.0) * 1000.0,
        "eventLoopLagP95Ms": percentile(loop_lag, 0.95) * 1000.0,
        "eventLoopLagMaxMs": max(loop_lag, default=0.0) * 1000.0,
        "normalSocketsWithoutPayload": sum(1 for socket in normal_sockets if socket.sent == 0),
        "slowSocketDisconnected": blocked_socket.closed_at is not None,
        "sender": sender_stats,
        "ticks": len(tick_durations),
        "tickMeanMs": statistics.fmean(tick_durations) * 1000.0 if tick_durations else 0.0,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--duration", type=float, default=15.0)
    args = parser.parse_args()
    result = asyncio.run(run(args.duration))
    print(json.dumps(result, ensure_ascii=False, indent=2))
    accepted = (
        result["initialSyncSec"] <= 5.0
        and result["eventLoopLagP95Ms"] <= 100.0
        and result["normalSocketsWithoutPayload"] == 0
        and result["slowSocketDisconnected"] is True
        and result["fixture"]["tabRecords"] == 2_000
    )
    return 0 if accepted else 1


if __name__ == "__main__":
    raise SystemExit(main())

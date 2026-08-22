#!/usr/bin/env python3
"""Black-box acceptance load for the Rust canary backend."""

from __future__ import annotations

import argparse
import asyncio
import json
import statistics
import sys
import time
from pathlib import Path

import websockets

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from server.core.codec import ProtobufMessageCodec  # noqa: E402
from server.proto_generated.teamviewer.v1 import teamviewer_pb2  # noqa: E402

CODEC = ProtobufMessageCodec()
ROOM = "rust-load-room"


def handshake(player_id: str, *, external: bool = False) -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    request = envelope.player_handshake_request
    request.network_protocol_version = "0.7.0"
    request.minimum_compatible_network_protocol_version = "0.6.1"
    request.local_program_version = "rust-load-test"
    request.submit_player_id = player_id
    request.room_code = ROOM
    request.client_role = (
        teamviewer_pb2.CLIENT_ROLE_EXTERNAL_SOURCE
        if external else teamviewer_pb2.CLIENT_ROLE_PLAYER
    )
    return envelope.SerializeToString()


def web_handshake() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    request = envelope.web_map_handshake_request
    request.network_protocol_version = "0.7.0"
    request.minimum_compatible_network_protocol_version = "0.6.1"
    request.local_program_version = "rust-load-test"
    request.room_code = ROOM
    return envelope.SerializeToString()


def initial_report() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle = envelope.player_report_bundle
    bundle.submit_player_id = "load-source"
    for index in range(400):
        bundle.players_replace.players[f"online-{index}"].CopyFrom(teamviewer_pb2.PlayerData(
            x=float(index), y=64, z=float(index), dimension="minecraft:overworld",
            player_name=f"Player{index}",
            player_uuid=f"00000000-0000-0000-0000-{index:012d}",
        ))
    for index in range(2_000):
        player_uuid = f"00000000-0000-0000-2000-{index:012d}"
        bundle.tab_players_replace.tab_players.add(uuid=player_uuid, name=f"Tab{index}")
        offline_uuid = f"00000000-0000-0000-1000-{index:012d}"
        bundle.last_seen_players_replace.players[offline_uuid].CopyFrom(
            teamviewer_pb2.LastSeenPlayerData(
                x=float(index), y=64, z=float(index), dimension="minecraft:overworld",
                player_name=f"Offline{index}", player_uuid=offline_uuid,
                last_seen_at_utc_ms=2_000 + index,
                position_observed_at_utc_ms=1_000 + index,
                offline_detected_at_utc_ms=3_000 + index,
            )
        )
    return envelope.SerializeToString()


def update_report(revision: int) -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle = envelope.player_report_bundle
    bundle.submit_player_id = "load-source"
    for offset in range(50):
        index = (revision * 50 + offset) % 400
        upsert = bundle.players_patch.upsert.add(id=f"online-{index}")
        upsert.data.x = float(index) + revision * 0.25
    return envelope.SerializeToString()


def ping() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    envelope.ping.SetInParent()
    return envelope.SerializeToString()


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, int(len(ordered) * fraction) - 1))]


async def run(base_url: str, duration: float) -> dict:
    ws_url = base_url.rstrip("/").replace("http://", "ws://").replace("https://", "wss://")
    viewers = []
    received = [0] * 20
    fixture_seen = [False] * 20
    disconnected = [False] * 20
    stopping = False
    rtts: list[float] = []
    ping_sent: asyncio.Queue[float] = asyncio.Queue()
    readers = []
    source = await websockets.connect(f"{ws_url}/mc-client", max_size=16 * 1024 * 1024)
    await source.send(handshake("load-source", external=True))
    source_ack = CODEC.decode(await source.recv())
    webmap = await websockets.connect(f"{ws_url}/web-map/ws", max_size=16 * 1024 * 1024)
    await webmap.send(web_handshake())
    web_ack = CODEC.decode(await webmap.recv())
    await webmap.recv()

    async def read_viewer(index: int, socket) -> None:
        try:
            async for raw in socket:
                packet = CODEC.decode(raw)
                if packet["type"] in {"snapshot_full", "patch"}:
                    received[index] += 1
                    if len(packet.get("players", {})) >= 400:
                        fixture_seen[index] = True
                    elif len(packet.get("players", {}).get("upsert", {})) >= 400:
                        fixture_seen[index] = True
                elif index == 0 and packet["type"] == "pong" and not ping_sent.empty():
                    rtts.append(time.monotonic() - await ping_sent.get())
        except Exception:
            if not stopping:
                disconnected[index] = True

    for index in range(20):
        socket = await websockets.connect(f"{ws_url}/mc-client", max_size=16 * 1024 * 1024)
        await socket.send(handshake(f"load-viewer-{index}"))
        ack = CODEC.decode(await socket.recv())
        if not ack.get("ready"):
            raise RuntimeError(f"viewer {index} handshake rejected")
        viewers.append(socket)
        readers.append(asyncio.create_task(read_viewer(index, socket)))

    started = time.monotonic()
    await source.send(initial_report())
    deadline = time.monotonic() + 10
    while not all(fixture_seen) and time.monotonic() < deadline:
        await asyncio.sleep(0.01)
    initial_sync = time.monotonic() - started

    revision = 0
    tick_durations = []
    deadline = time.monotonic() + duration
    next_ping = time.monotonic()
    while time.monotonic() < deadline:
        revision += 1
        tick = time.monotonic()
        await source.send(update_report(revision))
        tick_durations.append(time.monotonic() - tick)
        if time.monotonic() >= next_ping:
            await ping_sent.put(time.monotonic())
            await viewers[0].send(ping())
            next_ping = time.monotonic() + 0.2
        await asyncio.sleep(0.1)

    await asyncio.sleep(0.5)
    stopping = True
    for socket in viewers:
        await socket.close()
    await webmap.close()
    await source.close()
    await asyncio.gather(*readers, return_exceptions=True)
    return {
        "fixture": {"players": 20, "externalSources": 1, "webMaps": 1,
                    "onlineObjects": 400, "offlineObjects": 2_000, "tabRecords": 2_000},
        "sourceReady": source_ack.get("ready"), "webReady": web_ack.get("ready"),
        "initialSyncSec": initial_sync,
        "viewersWithoutState": sum(count == 0 for count in received),
        "viewersWithoutFixture": sum(not seen for seen in fixture_seen),
        "viewerDisconnects": sum(disconnected),
        "minimumStateFrames": min(received), "maximumStateFrames": max(received),
        "pingRttP95Ms": percentile(rtts, 0.95) * 1000,
        "pingRttMaxMs": max(rtts, default=0) * 1000,
        "updates": revision,
        "sendMeanMs": statistics.fmean(tick_durations) * 1000 if tick_durations else 0,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", default="http://127.0.0.1:18765")
    parser.add_argument("--duration", type=float, default=15.0)
    args = parser.parse_args()
    result = asyncio.run(run(args.url, max(0.1, args.duration)))
    print(json.dumps(result, ensure_ascii=False, indent=2))
    accepted = (
        result["sourceReady"] is True and result["webReady"] is True
        and result["initialSyncSec"] <= 5 and result["viewersWithoutState"] == 0
        and result["viewersWithoutFixture"] == 0 and result["viewerDisconnects"] == 0
        and result["pingRttP95Ms"] <= 250
    )
    return 0 if accepted else 1


if __name__ == "__main__":
    raise SystemExit(main())

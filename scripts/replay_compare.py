#!/usr/bin/env python3
"""Replay a deterministic protocol trace against Python and Rust servers."""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import urllib.request
from pathlib import Path

import websockets

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from server.core.codec import ProtobufMessageCodec  # noqa: E402
from server.proto_generated.teamviewer.v1 import teamviewer_pb2  # noqa: E402

CODEC = ProtobufMessageCodec()
ROOM = "replay-room"
TAB_UUID = "00000000-0000-0000-0000-000000000456"


def handshake(player_id: str, *, role: str | None = None) -> bytes:
    payload = {
        "type": "handshake", "channel": "player",
        "networkProtocolVersion": "0.7.0",
        "minimumCompatibleNetworkProtocolVersion": "0.6.1",
        "localProgramVersion": "replay-compare", "submitPlayerId": player_id,
        "roomCode": ROOM, "clientDisplayName": "Replay source" if role else "Replay viewer",
        "positionResolution": 1.0,
    }
    if role:
        payload["clientRole"] = role
    return CODEC.encode(payload)


def web_map_handshake() -> bytes:
    return CODEC.encode({
        "type": "handshake", "channel": "web_map",
        "networkProtocolVersion": "0.7.0",
        "minimumCompatibleNetworkProtocolVersion": "0.6.1",
        "localProgramVersion": "replay-compare", "roomCode": ROOM,
    })


def initial_report() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle = envelope.player_report_bundle
    bundle.submit_player_id = "replay-source"
    bundle.players_replace.players["target"].CopyFrom(teamviewer_pb2.PlayerData(
        x=12.5, y=64.0, z=-3.0, dimension="minecraft:overworld", player_name="Target",
        player_uuid="00000000-0000-0000-0000-000000000123", health=20.0,
        max_health=20.0, armor=3.0, is_riding=False, width=0.6, height=1.8,
    ))
    bundle.entities_replace.entities["mob-keep"].CopyFrom(teamviewer_pb2.EntityData(
        x=1, y=65, z=2, dimension="minecraft:overworld",
        entity_type="minecraft:zombie", entity_name="Zombie",
    ))
    bundle.entities_replace.entities["mob-delete"].CopyFrom(teamviewer_pb2.EntityData(
        x=3, y=65, z=4, dimension="minecraft:overworld",
    ))
    bundle.waypoints_replace.waypoints["home"].CopyFrom(teamviewer_pb2.WaypointData(
        x=10, y=70, z=20, dimension="minecraft:overworld", name="Home",
        symbol="H", color=0x12_34_56,
    ))
    bundle.last_seen_players_replace.players["offline"].CopyFrom(
        teamviewer_pb2.LastSeenPlayerData(
            x=5, y=64, z=6, dimension="minecraft:overworld", player_name="Offline",
            player_uuid="00000000-0000-0000-0000-000000000789",
            last_seen_at_utc_ms=1_700_000_000_000,
            position_observed_at_utc_ms=1_700_000_000_000,
            offline_detected_at_utc_ms=1_700_000_001_000,
        )
    )
    bundle.tab_players_replace.tab_players.add(
        uuid=TAB_UUID, name="Alice", display_name="[A] Alice", scoreboard_prefix="[A] ",
        scoreboard_color_rgb=0xFF_00_00,
    )
    observation = bundle.battle_map_observation
    observation.dimension = "minecraft:overworld"
    observation.map_size = 3
    observation.snapshot_observed_at = 1_700_000_002_000
    observation.parsed_at = 1_700_000_002_010
    observation.mode = "simmc"
    observation.candidates.add(
        base_chunk_x=12, base_chunk_z=34, position_sampled_at=1_700_000_001_900,
        source="history_primary",
    )
    observation.cells.add(rel_chunk_x=0, rel_chunk_z=0, symbol="╫", color_raw="#ff0000")
    return envelope.SerializeToString()


def patch_report() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle = envelope.player_report_bundle
    bundle.submit_player_id = "replay-source"
    player = bundle.players_patch.upsert.add(id="target")
    player.data.x = 13.5
    player.clear_fields.append("playerName")
    entity = bundle.entities_patch.upsert.add(id="mob-keep")
    entity.data.x = 2
    entity.clear_fields.append("entityName")
    bundle.entities_patch.delete.append("mob-delete")
    waypoint = bundle.waypoints_patch.upsert.add(id="home")
    waypoint.data.name = "Updated Home"
    waypoint.clear_fields.append("symbol")
    bundle.last_seen_players_patch.delete.append("offline")
    return envelope.SerializeToString()


def tab_sync_request() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    envelope.tab_history_sync_request.CopyFrom(teamviewer_pb2.TabHistorySyncRequest(
        request_id="replay-full", preferred_mode=teamviewer_pb2.TAB_HISTORY_SYNC_MODE_FULL,
        max_chunk_entries=1, allow_full_fallback=True,
    ))
    return envelope.SerializeToString()


def tab_subscribe_request() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    envelope.tab_history_subscribe_request.enabled = True
    return envelope.SerializeToString()


def tab_lookup_request() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    request = envelope.tab_history_lookup_request
    request.request_id = "replay-lookup"
    request.max_chunk_entries = 1
    request.selectors.add(uuid=TAB_UUID)
    request.selectors.add(name="ＡＬＩＣＥ")
    return envelope.SerializeToString()


def battle_meta_request() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    requested = envelope.battle_chunk_meta_request.battle_chunks.add()
    requested.dimension = "minecraft:overworld"
    requested.coord.chunk_x = 12
    requested.coord.chunk_z = 34
    return envelope.SerializeToString()


def apply_map_scope(state: dict, packet: dict, scope: str) -> None:
    if packet["type"] == "snapshot_full":
        state[scope] = dict(packet.get(scope, {}))
        return
    if packet["type"] != "patch" or scope not in packet:
        return
    target = state.setdefault(scope, {})
    patch = packet[scope]
    for object_id in patch.get("delete", []):
        target.pop(object_id, None)
    for object_id, delta in patch.get("upsert", {}).items():
        current = target.setdefault(object_id, {})
        for key, value in delta.items():
            if value is None:
                current.pop(key, None)
            else:
                current[key] = value


def apply_packet(state: dict, packet: dict) -> None:
    for scope in ("players", "entities", "waypoints", "battleChunks", "lastSeenPlayers"):
        apply_map_scope(state, packet, scope)


async def receive_until(socket, predicate, *, state: dict | None = None, timeout: float = 5.0) -> dict:
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        packet = CODEC.decode(await asyncio.wait_for(socket.recv(), 3.0))
        if state is not None:
            apply_packet(state, packet)
        if predicate(packet):
            return packet
    raise TimeoutError("expected protocol packet was not received")


def normalize_tab_entry(entry: dict) -> dict:
    return {
        "player": entry.get("player"),
        "labelFirstObservedPositive": entry.get("labelFirstObservedAtUtcMs", 0) > 0,
        "lastObservedPositive": entry.get("lastObservedAtUtcMs", 0) > 0,
        "revisionPositive": entry.get("revision", 0) > 0,
        "etagBytes": len(entry.get("etagSha256", b"")),
    }


async def http_snapshot(base_url: str) -> dict:
    def load() -> dict:
        with urllib.request.urlopen(f"{base_url}/snapshot?roomCode={ROOM}", timeout=3) as response:
            return json.load(response)
    payload = await asyncio.to_thread(load)
    room = payload.get("roomView", {})
    return {
        "players": len(room.get("players", {})),
        "entities": room.get("entities_count", len(room.get("entities", {}))),
        "waypoints": room.get("waypoints_count", len(room.get("waypoints", {}))),
        "battleChunks": room.get("battleChunks_count", len(room.get("battleChunks", {}))),
        "connections": room.get("connections_count", 0),
    }


async def replay(base_url: str) -> dict:
    ws_url = base_url.rstrip("/").replace("http://", "ws://").replace("https://", "wss://")
    async with websockets.connect(f"{ws_url}/mc-client", max_size=8 * 1024 * 1024) as source:
        await source.send(handshake("replay-source", role="CLIENT_ROLE_EXTERNAL_SOURCE"))
        source_ack = CODEC.decode(await asyncio.wait_for(source.recv(), 3.0))
        async with websockets.connect(f"{ws_url}/mc-client", max_size=8 * 1024 * 1024) as viewer:
            await viewer.send(handshake("replay-viewer"))
            viewer_ack = CODEC.decode(await asyncio.wait_for(viewer.recv(), 3.0))
            state: dict = {}
            await receive_until(viewer, lambda p: p["type"] == "snapshot_full", state=state)
            async with websockets.connect(f"{ws_url}/web-map/ws", max_size=8 * 1024 * 1024) as webmap:
                await webmap.send(web_map_handshake())
                web_ack = CODEC.decode(await asyncio.wait_for(webmap.recv(), 3.0))
                await receive_until(webmap, lambda p: p["type"] == "snapshot_full")
                await webmap.send(tab_subscribe_request())
                initial_digest = await receive_until(webmap, lambda p: p["type"] == "tab_history_digest")
                await source.send(initial_report())
                await receive_until(viewer, lambda _: "minecraft:overworld|12|34" in state.get("battleChunks", {}), state=state)
                await source.send(patch_report())
                await receive_until(
                    viewer,
                    lambda _: state.get("players", {}).get("target", {}).get("x") == 13.5
                    and "mob-delete" not in state.get("entities", {})
                    and "offline" not in state.get("lastSeenPlayers", {}),
                    state=state,
                )
                if initial_digest.get("head", {}).get("recordCount", 0) == 0:
                    await receive_until(
                        webmap,
                        lambda p: p["type"] == "tab_history_digest"
                        and p.get("head", {}).get("revision", 0) >= 1,
                    )
                await webmap.send(battle_meta_request())
                battle_meta = await receive_until(webmap, lambda p: p["type"] == "battle_chunk_meta_snapshot")
                await webmap.send(tab_sync_request())
                tab_full = await receive_until(webmap, lambda p: p["type"] == "tab_history_sync_chunk" and p.get("requestId") == "replay-full")
                await webmap.send(tab_lookup_request())
                lookup_chunks = []
                while not lookup_chunks or not lookup_chunks[-1].get("final"):
                    lookup_chunks.append(await receive_until(
                        webmap,
                        lambda p: p["type"] == "tab_history_lookup_chunk" and p.get("requestId") == "replay-lookup",
                    ))

            player = state["players"]["target"]
            entity = state["entities"]["mob-keep"]
            waypoint = state["waypoints"]["home"]
            battle = state["battleChunks"]["minecraft:overworld|12|34"]
            tab_entries = [normalize_tab_entry(entry) for entry in tab_full.get("upsert", [])]
            lookup = {
                result["selectorIndex"]: [normalize_tab_entry(entry) for entry in result.get("entries", [])]
                for chunk in lookup_chunks for result in chunk.get("results", [])
            }
            snapshot = await http_snapshot(base_url)
    return {
        "sourceReady": source_ack.get("ready"), "viewerReady": viewer_ack.get("ready"),
        "webReady": web_ack.get("ready"),
        "player": player, "entity": entity, "waypoint": waypoint, "battle": battle,
        "battleMeta": battle_meta.get("battleChunks", {}),
        "tabHead": {
            "revisionPositive": tab_full.get("head", {}).get("revision", 0) > 0,
            "recordCount": tab_full.get("head", {}).get("recordCount"),
            "digestBytes": len(tab_full.get("head", {}).get("digestSha256", b"")),
        },
        "tabEntries": tab_entries, "lookup": lookup, "snapshot": snapshot,
    }


async def main_async(python_url: str, rust_url: str) -> int:
    python, rust = await asyncio.gather(replay(python_url), replay(rust_url))
    if python != rust:
        print(json.dumps({"python": python, "rust": rust}, ensure_ascii=False, indent=2))
        return 1
    print(json.dumps({"match": True, "result": python}, ensure_ascii=False, indent=2))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--python-url", default="http://127.0.0.1:18764")
    parser.add_argument("--rust-url", default="http://127.0.0.1:18765")
    args = parser.parse_args()
    return asyncio.run(main_async(args.python_url, args.rust_url))


if __name__ == "__main__":
    raise SystemExit(main())

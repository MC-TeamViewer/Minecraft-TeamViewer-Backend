from pathlib import Path
import sys

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

from server.core.broadcaster import Broadcaster
from server.core.codec import ProtobufMessageCodec
from server.proto_generated.teamviewer.v1 import teamviewer_pb2
from server.state import ServerState


CODEC = ProtobufMessageCodec()


def test_wire_envelope_payload_field_numbers_are_contiguous() -> None:
    payload = teamviewer_pb2.WireEnvelope.DESCRIPTOR.oneofs_by_name["payload"]
    field_numbers = [field.number for field in payload.fields]

    assert field_numbers == list(range(10, 27))
    assert teamviewer_pb2.WireEnvelope.DESCRIPTOR.fields_by_name["channel"].number == 1


def test_codec_roundtrip_core_outbound_payloads() -> None:
    handshake = CODEC.decode(
        CODEC.encode(
            {
                "type": "handshake",
                "channel": "player",
                "networkProtocolVersion": "0.6.1",
                "minimumCompatibleNetworkProtocolVersion": "0.6.1",
                "localProgramVersion": "client",
                "submitPlayerId": "00000000-0000-0000-0000-000000000001",
                "roomCode": "room-a",
            }
        )
    )
    assert handshake["type"] == "handshake"
    assert handshake["_wire_channel"] == "player"
    assert handshake["_payload_case"] == "player_handshake_request"
    assert handshake["submitPlayerId"] == "00000000-0000-0000-0000-000000000001"

    ping = CODEC.decode(CODEC.encode({"type": "ping"}))
    assert ping["type"] == "ping"

    pong = CODEC.decode(CODEC.encode({"type": "pong", "serverTime": 123.0}))
    assert pong["type"] == "pong"
    assert pong["serverTime"] == 123.0

    handshake_ack = CODEC.decode(
        CODEC.encode(
            {
                "type": "handshake_ack",
                "networkProtocolVersion": "0.6.1",
                "minimumCompatibleNetworkProtocolVersion": "0.6.1",
                "localProgramVersion": "server",
                "roomCode": "room-a",
                "deltaEnabled": True,
                "ready": True,
            }
        )
    )
    assert handshake_ack["type"] == "handshake_ack"
    assert handshake_ack["ready"] is True

    web_map_ack = CODEC.decode(
        CODEC.encode(
            {
                "type": "web_map_ack",
                "channel": "web_map",
                "ok": True,
                "action": "set_player_mark",
                "playerId": "player-1",
                "mark": {"team": "ally", "color": "#00ff00"},
            }
        )
    )
    assert web_map_ack["type"] == "web_map_ack"
    assert web_map_ack["_payload_case"] == "web_map_ack"
    assert web_map_ack["playerId"] == "player-1"

    snapshot_full = CODEC.decode(
        CODEC.encode(
            {
                "type": "snapshot_full",
                "players": {
                    "player-1": {
                        "x": 1.0,
                        "y": 64.0,
                        "z": 2.0,
                        "dimension": "minecraft:overworld",
                    }
                },
                "battleChunks": {
                    "minecraft:overworld|1|2": {
                        "dimension": "minecraft:overworld",
                        "chunkX": 1,
                        "chunkZ": 2,
                        "colorRaw": "#112233",
                    }
                },
            }
        )
    )
    assert snapshot_full["type"] == "snapshot_full"
    assert snapshot_full["players"]["player-1"]["dimension"] == "minecraft:overworld"
    assert snapshot_full["battleChunks"]["minecraft:overworld|1|2"]["colorRaw"] == "#112233"

    patch = CODEC.decode(
        CODEC.encode(
            {
                "type": "patch",
                "players": {
                    "upsert": {
                        "player-1": {
                            "x": 1.0,
                            "dimension": "minecraft:overworld",
                        }
                    },
                    "delete": ["player-2"],
                },
                "battleChunks": {
                    "upsert": {
                        "minecraft:overworld|1|2": {
                            "dimension": "minecraft:overworld",
                            "chunkX": 1,
                            "chunkZ": 2,
                            "colorRaw": "#445566",
                        }
                    },
                    "delete": ["minecraft:overworld|2|3"],
                },
            }
        )
    )
    assert patch["type"] == "patch"
    assert patch["_payload_case"] == "patch"
    assert patch["players"]["delete"] == ["player-2"]
    assert patch["battleChunks"]["delete"] == ["minecraft:overworld|2|3"]

    digest = CODEC.decode(CODEC.encode({"type": "digest", "hashes": {"players": "a", "entities": "b", "waypoints": "c"}}))
    assert digest["type"] == "digest"
    assert digest["players"] == "a"

    refresh_req = CODEC.decode(
        CODEC.encode(
            {
                "type": "refresh_req",
                "reason": "baseline_missing",
                "serverTime": 123.0,
                "players": ["player-1"],
                "entities": ["entity-1"],
                "battleChunks": ["minecraft:overworld|4|5"],
            }
        )
    )
    assert refresh_req["type"] == "refresh_req"
    assert refresh_req["battleChunks"] == ["minecraft:overworld|4|5"]

    report_rate_hint = CODEC.decode(
        CODEC.encode({"type": "report_rate_hint", "reportIntervalTicks": 10, "broadcastHz": 5.0, "reason": "runtime"})
    )
    assert report_rate_hint["type"] == "report_rate_hint"
    assert report_rate_hint["reportIntervalTicks"] == 10


def test_codec_decodes_inbound_only_payloads() -> None:
    web_map_command_envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    web_map_command_envelope.web_map_command.set_player_mark.player_id = "player-1"
    web_map_command_envelope.web_map_command.set_player_mark.team = "ally"
    web_map_command_envelope.web_map_command.set_player_mark.color = "#00ff00"

    web_map_command = CODEC.decode(web_map_command_envelope.SerializeToString())
    assert web_map_command["type"] == "command_player_mark_set"
    assert web_map_command["_payload_case"] == "web_map_command"
    assert web_map_command["_command_case"] == "set_player_mark"
    assert web_map_command["playerId"] == "player-1"
    assert web_map_command["_wire_channel"] == "web_map"

    bundle_envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle_envelope.player_report_bundle.submit_player_id = "player-1"
    bundle_envelope.player_report_bundle.players_patch.upsert.add(
        id="player-1",
        data=teamviewer_pb2.PlayerDelta(x=1.0, dimension="minecraft:overworld"),
    )
    bundle_envelope.player_report_bundle.waypoints_delete.waypoint_ids.extend(["wp-1"])

    bundle = CODEC.decode(bundle_envelope.SerializeToString())
    assert bundle["type"] == "player_report_bundle"
    assert bundle["submitPlayerId"] == "player-1"
    assert bundle["playersPatch"]["upsert"]["player-1"]["dimension"] == "minecraft:overworld"
    assert bundle["waypointsDelete"]["waypointIds"] == ["wp-1"]


def test_player_outbound_digest_view_matches_client_visible_battle_chunk_shape() -> None:
    state = ServerState()
    broadcaster = Broadcaster(state)

    sync_view_state = {
        "players": {
            "player-1": {
                "x": 1.0,
                "y": 64.0,
                "z": 2.0,
                "dimension": "minecraft:overworld",
                "playerName": "tester",
                "playerUUID": None,
            }
        },
        "entities": {},
        "waypoints": {
            "wp-1": {
                "x": 1.0,
                "y": 64.0,
                "z": 2.0,
                "dimension": "minecraft:overworld",
                "name": "wp",
                "roomCode": None,
            }
        },
        "battleChunks": {
            "default|minecraft:overworld|1|2": {
                "dimension": "minecraft:overworld",
                "chunkX": 1,
                "chunkZ": 2,
                "colorRaw": "#112233",
                "roomCode": "default",
                "colorMode": "raw_observed",
                "colorSemanticKey": None,
            }
        },
    }

    digest_view = broadcaster._build_player_outbound_digest_view(sync_view_state)

    assert digest_view["players"]["player-1"]["playerName"] == "tester"
    assert "playerUUID" not in digest_view["players"]["player-1"]
    assert "roomCode" not in digest_view["waypoints"]["wp-1"]
    assert "default|minecraft:overworld|1|2" not in digest_view["battleChunks"]
    assert digest_view["battleChunks"]["minecraft:overworld|1|2"] == {
        "dimension": "minecraft:overworld",
        "chunkX": 1,
        "chunkZ": 2,
        "colorRaw": "#112233",
        "roomCode": "default",
        "colorMode": "raw_observed",
    }

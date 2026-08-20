from pathlib import Path
import sys

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

from server.core.codec import ProtobufMessageCodec
from server.proto_generated.teamviewer.v1 import teamviewer_pb2
from server.core.protocol import PlayerReportBundlePacket, ScopePatchPacket
from server.ws.io import expand_player_packets


def test_expand_player_packets_assigns_internal_packet_types() -> None:
    bundle = PlayerReportBundlePacket(
        type="player_report_bundle",
        submitPlayerId="player-1",
        playersPatch=ScopePatchPacket(
            upsert={"player-1": {"x": 1.0, "y": 64.0, "z": 2.0, "dimension": "minecraft:overworld"}},
            delete=[],
        ),
    )

    expanded = expand_player_packets(bundle)

    assert len(expanded) == 1
    assert expanded[0].type == "players_patch"
    assert expanded[0].submitPlayerId == "player-1"


def test_codec_decodes_bundle_nested_messages_with_internal_types() -> None:
    codec = ProtobufMessageCodec()
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    envelope.player_report_bundle.submit_player_id = "player-1"
    envelope.player_report_bundle.source_state_clear.scopes.extend(["players", "entities"])
    envelope.player_report_bundle.battle_map_observation.dimension = "minecraft:overworld"
    envelope.player_report_bundle.battle_map_observation.map_size = 5
    envelope.player_report_bundle.battle_map_observation.anchor_row = 0
    envelope.player_report_bundle.battle_map_observation.anchor_col = 0
    envelope.player_report_bundle.battle_map_observation.snapshot_observed_at = 123
    envelope.player_report_bundle.battle_map_observation.parsed_at = 456
    envelope.player_report_bundle.battle_map_observation.mode = "simmc"

    decoded = codec.decode(envelope.SerializeToString())

    assert decoded["type"] == "player_report_bundle"
    assert decoded["sourceStateClear"]["type"] == "source_state_clear"
    assert decoded["sourceStateClear"]["scopes"] == ["players", "entities"]
    assert decoded["battleMapObservation"]["type"] == "battle_map_observation"
    assert decoded["battleMapObservation"]["dimension"] == "minecraft:overworld"
    assert decoded["battleMapObservation"]["mode"] == "simmc"


def test_present_empty_replace_scopes_are_preserved() -> None:
    codec = ProtobufMessageCodec()
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    envelope.player_report_bundle.submit_player_id = "source-1"
    envelope.player_report_bundle.players_replace.SetInParent()
    envelope.player_report_bundle.tab_players_replace.SetInParent()

    decoded = codec.decode(envelope.SerializeToString())
    expanded = expand_player_packets(PlayerReportBundlePacket(**decoded))

    assert [item.type for item in expanded] == ["players_update", "tab_players_update"]
    assert expanded[0].players == {}
    assert expanded[1].tabPlayers == []


def test_proto3_zero_coordinates_are_not_treated_as_missing() -> None:
    codec = ProtobufMessageCodec()
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle = envelope.player_report_bundle
    bundle.submit_player_id = "source-1"
    bundle.players_replace.players["target"].CopyFrom(
        teamviewer_pb2.PlayerData(
            x=0.0,
            y=64.0,
            z=0.0,
            dimension="minecraft_overworld",
        )
    )

    decoded = codec.decode(envelope.SerializeToString())
    expanded = expand_player_packets(PlayerReportBundlePacket(**decoded))

    assert expanded[0].players["target"].x == 0.0
    assert expanded[0].players["target"].z == 0.0


def test_last_seen_uuid_is_restored_from_full_map_key() -> None:
    codec = ProtobufMessageCodec()
    player_id = "d5be8d2c-548e-38f8-94b6-d275ddc220f2"
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle = envelope.player_report_bundle
    bundle.submit_player_id = "source-1"
    value = bundle.last_seen_players_replace.players[player_id]
    value.x = 30203.0
    value.y = 41.0
    value.z = 17.0
    value.dimension = "minecraft:overworld"
    value.player_name = "Player"
    value.last_seen_at_utc_ms = 1_000
    value.position_observed_at_utc_ms = 900
    value.offline_detected_at_utc_ms = 1_100

    decoded = codec.decode(envelope.SerializeToString())
    assert decoded["lastSeenPlayersReplace"][player_id]["playerUUID"] == player_id
    expanded = expand_player_packets(PlayerReportBundlePacket(**decoded))
    assert expanded[0].players[player_id].playerUUID == player_id


def test_last_seen_uuid_is_restored_from_patch_id() -> None:
    codec = ProtobufMessageCodec()
    player_id = "227c6060-7661-4c7c-8d17-f5dc0a661b35"
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle = envelope.player_report_bundle
    bundle.submit_player_id = "source-1"
    upsert = bundle.last_seen_players_patch.upsert.add()
    upsert.id = player_id
    value = upsert.data
    value.x = 27333.0
    value.y = 49.0
    value.z = 12.0
    value.dimension = "minecraft:overworld"
    value.player_name = "Player"
    value.last_seen_at_utc_ms = 2_000
    value.position_observed_at_utc_ms = 1_900
    value.offline_detected_at_utc_ms = 2_100

    decoded = codec.decode(envelope.SerializeToString())
    assert decoded["lastSeenPlayersPatch"]["upsert"][player_id]["playerUUID"] == player_id
    expanded = expand_player_packets(PlayerReportBundlePacket(**decoded))
    assert expanded[0].upsert[player_id].playerUUID == player_id

from pathlib import Path
import sys

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

from server.codec import ProtobufMessageCodec
from server.proto_generated.teamviewer.v1 import teamviewer_pb2
from main import expand_player_packets
from server.protocol import PlayerReportBundlePacket, ScopePatchPacket


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

    decoded = codec.decode(envelope.SerializeToString())

    assert decoded["type"] == "player_report_bundle"
    assert decoded["sourceStateClear"]["type"] == "source_state_clear"
    assert decoded["sourceStateClear"]["scopes"] == ["players", "entities"]
    assert decoded["battleMapObservation"]["type"] == "battle_map_observation"
    assert decoded["battleMapObservation"]["dimension"] == "minecraft:overworld"

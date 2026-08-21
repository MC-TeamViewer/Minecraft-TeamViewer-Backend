from pathlib import Path
from types import MethodType, SimpleNamespace
import sys
import time

import pytest

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

from server.core.broadcaster import Broadcaster
from server.core.codec import ProtobufMessageCodec
from server.state import ServerState
from server.state import server_state as server_state_module


def _player_data(x: float) -> dict:
    return {
        "x": x,
        "y": 64.0,
        "z": 2.0,
        "dimension": "minecraft_overworld",
        "playerName": "Target",
        "playerUUID": "00000000-0000-0000-0000-000000000999",
    }


def _node(source_id: str, timestamp: float, x: float) -> dict:
    return ServerState.build_state_node(source_id, timestamp, _player_data(x))


def _last_seen_data(uuid: str = "00000000-0000-0000-0000-000000000999") -> dict:
    return {
        "x": 10.0,
        "y": 64.0,
        "z": 20.0,
        "dimension": "minecraft:overworld",
        "playerName": "Target",
        "playerUUID": uuid,
        "lastSeenAtUtcMs": 1_000,
        "positionObservedAtUtcMs": 900,
        "offlineDetectedAtUtcMs": 1_100,
    }


def _connected_socket() -> SimpleNamespace:
    connected = SimpleNamespace(name="CONNECTED")
    return SimpleNamespace(
        client_state=connected,
        application_state=connected,
        close_code=None,
        close_reason=None,
    )


def test_external_role_is_not_counted_as_player_and_has_equal_arbitration_priority() -> None:
    state = ServerState()
    state.connections["player-source"] = _connected_socket()  # type: ignore[assignment]
    state.connections["external-source"] = _connected_socket()  # type: ignore[assignment]
    state.set_connection_identity("player-source", None)
    state.set_connection_identity("external-source", "CLIENT_ROLE_EXTERNAL_SOURCE", "SIMMC")

    assert state.get_player_connection_ids() == {"player-source"}

    state.player_reports["target"] = {
        "player-source": _node("player-source", 10.0, 1.0),
        "external-source": _node("external-source", 11.0, 2.0),
    }
    resolved = state.resolve_report_map(
        state.player_reports,
        {},
        state.SOURCE_SWITCH_THRESHOLD_SEC,
    )
    assert resolved["target"]["submitPlayerId"] == "external-source"


def test_source_stickiness_switches_only_after_receipt_time_lead() -> None:
    state = ServerState()
    selected: dict[str, str] = {}
    reports = {"target": {"source-a": _node("source-a", 10.0, 1.0)}}

    state.resolve_report_map(reports, selected, state.SOURCE_SWITCH_THRESHOLD_SEC)
    reports["target"]["source-b"] = _node("source-b", 10.2, 2.0)
    sticky = state.resolve_report_map(reports, selected, state.SOURCE_SWITCH_THRESHOLD_SEC)
    assert sticky["target"]["submitPlayerId"] == "source-a"

    reports["target"]["source-b"] = _node("source-b", 10.36, 3.0)
    switched = state.resolve_report_map(reports, selected, state.SOURCE_SWITCH_THRESHOLD_SEC)
    assert switched["target"]["submitPlayerId"] == "source-b"


def test_tab_patch_preserves_existing_complete_display_name() -> None:
    state = ServerState()
    player_uuid = "12345678-1234-5678-9234-567812345678"
    state.upsert_tab_player_report("source", [{
        "uuid": player_uuid,
        "name": "Player",
        "displayName": "[利雅得] Player",
        "scoreboardPrefix": "nt00011bf146084c",
    }], 1.0)

    report = state.patch_tab_player_report("source", {
        player_uuid: {"uuid": player_uuid, "name": "Renamed"},
    }, [], 2.0)

    entry = report["playersByKey"][player_uuid]
    assert entry["name"] == "Renamed"
    assert entry["displayName"] == "[利雅得] Player"
    assert entry["scoreboardPrefix"] == "nt00011bf146084c"


def test_player_resolution_prefers_active_self_report_over_newer_external_report() -> None:
    state = ServerState()
    target = "target"
    external = "external-source"
    state.connections[target] = _connected_socket()  # type: ignore[assignment]
    state.connections[external] = _connected_socket()  # type: ignore[assignment]
    state.set_connection_identity(target, "CLIENT_ROLE_PLAYER")
    state.set_connection_identity(external, "CLIENT_ROLE_EXTERNAL_SOURCE", "Squaremap")
    state.mark_player_capability(external, "0.6.5", position_resolution=1.0)
    state.player_selected_sources[target] = external
    state.player_reports[target] = {
        target: _node(target, 10.0, 1.25),
        external: _node(external, 100.0, 1.0),
    }

    state.refresh_resolved_states()

    resolved = state.players[target]
    assert resolved["submitPlayerId"] == target
    assert resolved["data"]["x"] == 1.25
    assert resolved["data"]["positionSourceId"] == target
    assert resolved["data"]["positionSourceKind"] == "PLAYER_POSITION_SOURCE_KIND_SELF_REPORT"
    assert "positionResolution" not in resolved["data"]


def test_player_resolution_prefers_player_report_then_falls_back_to_external() -> None:
    state = ServerState()
    observer = "player-source"
    external = "external-source"
    state.connections[observer] = _connected_socket()  # type: ignore[assignment]
    state.connections[external] = _connected_socket()  # type: ignore[assignment]
    state.set_connection_identity(observer, "CLIENT_ROLE_PLAYER")
    state.set_connection_identity(external, "CLIENT_ROLE_EXTERNAL_SOURCE", "Squaremap")
    state.mark_player_capability(external, "0.6.5", position_resolution=1.0)
    state.player_reports["target"] = {
        observer: _node(observer, 10.0, 1.25),
        external: _node(external, 100.0, 1.0),
    }

    state.refresh_resolved_states()
    assert state.players["target"]["submitPlayerId"] == observer
    assert state.players["target"]["data"]["positionSourceKind"] == "PLAYER_POSITION_SOURCE_KIND_PLAYER_REPORT"

    state.clear_source_state(observer, ["players"])
    state.refresh_resolved_states()
    resolved = state.players["target"]
    assert resolved["submitPlayerId"] == external
    assert resolved["data"]["positionSourceKind"] == "PLAYER_POSITION_SOURCE_KIND_EXTERNAL_SOURCE"
    assert resolved["data"]["positionSourceDisplayName"] == "Squaremap"
    assert resolved["data"]["positionResolution"] == 1.0


def test_scoped_arbitration_falls_back_when_global_winner_is_not_visible() -> None:
    state = ServerState()
    state.player_reports["target"] = {
        "room-a-source": _node("room-a-source", 10.0, 1.0),
        "room-b-source": _node("room-b-source", 20.0, 9.0),
    }

    state.refresh_resolved_states()
    scoped = state.resolve_states_for_sources({"room-a-source"}, "room-a")

    assert state.players["target"]["submitPlayerId"] == "room-b-source"
    assert scoped["players"]["target"]["submitPlayerId"] == "room-a-source"
    assert scoped["players"]["target"]["data"]["x"] == 1.0


@pytest.mark.asyncio
async def test_scoped_view_change_is_sent_when_global_winner_does_not_change() -> None:
    state = ServerState()
    broadcaster = Broadcaster(state)
    now = time.monotonic()
    viewer = "viewer"
    source_a = "source-a"
    source_b = "source-b"
    target = "target"

    state.same_server_filter_enabled = True
    for source_id in (viewer, source_a, source_b):
        state.connections[source_id] = _connected_socket()  # type: ignore[assignment]
        state.set_player_room(source_id, "room-a")
    state.set_connection_identity(viewer, "CLIENT_ROLE_PLAYER")
    state.set_connection_identity(source_a, "CLIENT_ROLE_EXTERNAL_SOURCE")
    state.set_connection_identity(source_b, "CLIENT_ROLE_EXTERNAL_SOURCE")
    shared_tab = [{"uuid": "00000000-0000-0000-0000-000000000111", "name": "Shared"}]
    state.upsert_tab_player_report(viewer, shared_tab, now)
    state.upsert_tab_player_report(source_a, shared_tab, now)
    state.upsert_tab_player_report(
        source_b,
        [{"uuid": "00000000-0000-0000-0000-000000000222", "name": "Other"}],
        now,
    )
    state.player_reports[target] = {
        source_a: _node(source_a, now, 1.0),
        source_b: _node(source_b, now + 10.0, 9.0),
    }
    state.refresh_resolved_states()

    first_visible = broadcaster._build_visible_state_for_player(viewer)
    broadcaster._player_last_states[viewer] = broadcaster._build_player_sync_view_state(first_visible)
    sent: list[bytes] = []

    async def capture_send(self, ws, payload: bytes, *, channel: str) -> None:
        sent.append(payload)

    async def skip_web_maps(self, force_full: bool = False) -> None:
        return None

    broadcaster._send_encoded = MethodType(capture_send, broadcaster)
    broadcaster.broadcast_web_map_updates = MethodType(skip_web_maps, broadcaster)

    state.player_reports[target][source_a] = _node(source_a, now + 1.0, 2.0)
    await broadcaster.broadcast_updates()

    assert state.players[target]["data"]["x"] == 9.0
    snapshots = [ProtobufMessageCodec().decode(payload) for payload in sent]
    assert all(packet["type"] != "snapshot_full" for packet in snapshots)
    scoped_patch = next(packet for packet in snapshots if packet["type"] == "patch")
    assert scoped_patch["players"]["upsert"][target]["x"] == 2.0


def test_legacy_field_clear_is_split_into_delete_then_full_object_upsert() -> None:
    state = ServerState()
    broadcaster = Broadcaster(state)
    current = {
        "players": {
            "target": {
                "x": 2.0,
                "y": 64.0,
                "z": 3.0,
                "dimension": "minecraft:overworld",
            }
        }
    }
    patch = {
        "players": {
            "upsert": {"target": {"x": 2.0, "playerName": None}},
            "delete": [],
        }
    }

    phases = broadcaster._split_patch_for_legacy_client(patch, current, ("players",))

    assert phases == [
        {"players": {"upsert": {}, "delete": ["target"]}},
        {"players": {"upsert": {"target": current["players"]["target"]}, "delete": []}},
    ]


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("protocol_version", "expected_packet_count"),
    (("0.6.4", 2), ("0.6.5", 1)),
)
async def test_field_clear_delivery_never_falls_back_to_snapshot_full(
    protocol_version: str,
    expected_packet_count: int,
) -> None:
    state = ServerState()
    broadcaster = Broadcaster(state)
    current = {
        "players": {
            "target": {
                "x": 2.0,
                "y": 64.0,
                "z": 3.0,
                "dimension": "minecraft:overworld",
            }
        }
    }
    patch = {
        "players": {
            "upsert": {"target": {"x": 2.0, "playerName": None}},
            "delete": [],
        }
    }
    sent: list[bytes] = []

    async def capture_send(self, ws, payload: bytes, *, channel: str) -> None:
        sent.append(payload)

    broadcaster._send_encoded = MethodType(capture_send, broadcaster)
    await broadcaster._send_compatible_patch(
        _connected_socket(),
        patch,
        current,
        ("players",),
        protocol_version,
        channel="player",
    )

    packets = [ProtobufMessageCodec().decode(payload) for payload in sent]
    assert len(packets) == expected_packet_count
    assert all(packet["type"] == "patch" for packet in packets)
    if protocol_version == "0.6.4":
        assert packets[0]["players"] == {"upsert": {}, "delete": ["target"]}
        assert packets[1]["players"] == {
            "upsert": {"target": current["players"]["target"]},
            "delete": [],
        }
    else:
        assert packets[0]["players"]["upsert"]["target"] == {
            "x": 2.0,
            "playerName": None,
        }


def test_wall_clock_jumps_do_not_change_monotonic_timeouts(monkeypatch: pytest.MonkeyPatch) -> None:
    state = ServerState()
    state.PLAYER_TIMEOUT = 2
    state.player_reports["target"] = {"source": _node("source", 100.0, 1.0)}

    monkeypatch.setattr(server_state_module.time, "time", lambda: -10_000_000.0)
    monkeypatch.setattr(server_state_module.time, "monotonic", lambda: 101.0)
    state.cleanup_timeouts()
    assert "target" in state.player_reports

    monkeypatch.setattr(server_state_module.time, "time", lambda: 10_000_000_000.0)
    monkeypatch.setattr(server_state_module.time, "monotonic", lambda: 103.0)
    state.cleanup_timeouts()
    assert "target" not in state.player_reports


def test_external_health_status_uses_wall_time_without_touching_report_liveness() -> None:
    state = ServerState()
    source_id = "external-source"
    state.set_connection_identity(source_id, "CLIENT_ROLE_EXTERNAL_SOURCE")
    state.player_reports["target"] = {source_id: _node(source_id, 123.0, 1.0)}
    state.upsert_tab_player_report(source_id, [], 123.0)

    status = state.update_external_source_status(
        source_id,
        "EXTERNAL_SOURCE_HEALTH_HEALTHY",
        received_at=456.0,
    )

    assert status["statusReceivedAt"] == 456.0
    assert status["lastHealthyAt"] == 456.0
    assert state.player_reports["target"][source_id]["timestamp"] == 123.0
    assert state.tab_player_reports[source_id]["_livenessTimestamp"] == 123.0


def test_disconnected_external_source_keeps_bounded_admin_record() -> None:
    state = ServerState()
    source_id = "external-source"
    state.connections[source_id] = _connected_socket()  # type: ignore[assignment]
    state.set_connection_identity(source_id, "CLIENT_ROLE_EXTERNAL_SOURCE", "SIMMC")
    state.set_player_room(source_id, "room-a")
    state.connection_caps[source_id] = {
        "protocol": "0.6.3",
        "programVersion": "0.1.0",
        "remoteAddr": "127.0.0.1",
    }
    state.update_external_source_status(
        source_id,
        "EXTERNAL_SOURCE_HEALTH_HEALTHY",
        received_at=456.0,
    )

    state.remove_connection(source_id)

    record = state.disconnected_external_sources[source_id]
    assert record["connected"] is False
    assert record["displayName"] == "SIMMC"
    assert record["roomCode"] == "room-a"
    assert record["status"]["lastHealthyAt"] == 456.0
    assert source_id not in state.connections


def test_last_seen_history_survives_source_disconnect_and_is_room_isolated() -> None:
    state = ServerState()
    source_id = "external-source"
    state.connections[source_id] = _connected_socket()  # type: ignore[assignment]
    state.set_connection_identity(source_id, "CLIENT_ROLE_EXTERNAL_SOURCE")
    state.set_player_room(source_id, "room-a")
    state.replace_last_seen_players(
        source_id,
        "room-a",
        {"00000000-0000-0000-0000-000000000999": _last_seen_data()},
        10.0,
    )

    state.remove_connection(source_id)

    assert len(state.resolve_states_for_sources(set(), "room-a")["lastSeenPlayers"]) == 1
    assert state.resolve_states_for_sources(set(), "room-b")["lastSeenPlayers"] == {}


def test_admin_last_seen_records_are_raw_and_delete_by_exact_composite_key() -> None:
    state = ServerState()
    player_id = "00000000-0000-0000-0000-000000000999"
    state.replace_last_seen_players("source-a", "room-a", {player_id: _last_seen_data(player_id)}, 10.0)
    state.replace_last_seen_players("source-b", "room-a", {player_id: _last_seen_data(player_id)}, 11.0)

    records = state.list_admin_last_seen_records(room_code="room-a", search="target")
    assert [(item["sourceId"], item["playerUuid"]) for item in records] == [
        ("source-a", player_id),
        ("source-b", player_id),
    ]

    deleted = state.delete_admin_last_seen_records([{
        "roomCode": "room-a",
        "sourceId": "source-a",
        "playerUuid": player_id,
    }])
    assert deleted == [{"roomCode": "room-a", "sourceId": "source-a", "playerUuid": player_id}]
    assert state.list_admin_last_seen_records() == [records[1]]
    assert state.delete_admin_last_seen_records([{
        "roomCode": "room-b",
        "sourceId": "source-b",
        "playerUuid": player_id,
    }]) == []


def test_online_player_suppresses_matching_last_seen_history() -> None:
    state = ServerState()
    source_id = "external-source"
    player_id = "00000000-0000-0000-0000-000000000999"
    state.connections[source_id] = _connected_socket()  # type: ignore[assignment]
    state.set_connection_identity(source_id, "CLIENT_ROLE_EXTERNAL_SOURCE")
    state.set_player_room(source_id, "room-a")
    state.replace_last_seen_players(
        source_id,
        "room-a",
        {player_id: _last_seen_data(player_id)},
        10.0,
    )
    state.player_reports[player_id] = {source_id: _node(source_id, 11.0, 30.0)}

    resolved = state.resolve_states_for_sources({source_id}, "room-a")

    assert player_id in resolved["players"]
    assert player_id not in resolved["lastSeenPlayers"]


def test_protocol_gate_keeps_history_out_of_old_clients() -> None:
    state = ServerState()
    broadcaster = Broadcaster(state)
    state.connection_caps["old"] = {"protocol": "0.6.3"}
    state.connection_caps["new"] = {"protocol": "0.6.4"}
    state.web_map_connection_protocols["old-web"] = "0.6.3"
    state.web_map_connection_protocols["new-web"] = "0.6.4"
    view = {"players": {}, "lastSeenPlayers": {"target": _last_seen_data()}}

    assert broadcaster._player_supports_last_seen("old") is False
    assert broadcaster._player_supports_last_seen("new") is True
    assert "lastSeenPlayers" not in broadcaster._web_map_state_for_client("old-web", view)
    assert "lastSeenPlayers" in broadcaster._web_map_state_for_client("new-web", view)

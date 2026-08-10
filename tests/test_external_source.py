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
    scoped_snapshot = next(packet for packet in snapshots if packet["type"] == "snapshot_full")
    assert scoped_snapshot["players"][target]["x"] == 2.0


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

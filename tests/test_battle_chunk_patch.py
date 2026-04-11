from copy import deepcopy
from pathlib import Path
import sys

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

from server.core.broadcaster import Broadcaster
from server.state import ServerState


def build_node(data: dict) -> dict:
    return {
        "timestamp": 1.0,
        "submitPlayerId": "player-1",
        "data": dict(data),
    }


def build_battle_chunk_data(**overrides: object) -> dict:
    data = {
        "chunkX": 12,
        "chunkZ": 34,
        "dimension": "minecraft:overworld",
        "symbol": "A",
        "colorRaw": "#ff0000",
        "markerType": "danger",
    }
    data.update(overrides)
    return data


def build_web_map_state(*, battle_chunks: dict | None = None) -> dict:
    return {
        "players": {},
        "entities": {},
        "waypoints": {},
        "battleChunks": battle_chunks or {},
        "playerMarks": {},
        "tabState": {"enabled": False, "reports": {}, "groups": []},
        "roomCode": "test-room",
        "connections": [],
        "connections_count": 0,
    }


def test_compute_scope_patch_full_replace_skips_unchanged_battle_chunk() -> None:
    chunk_id = "minecraft:overworld|12|34"
    chunk_data = build_battle_chunk_data()

    patch = ServerState.compute_scope_patch(
        {chunk_id: build_node(chunk_data)},
        {chunk_id: build_node(chunk_data)},
        full_replace=True,
    )

    assert patch == {"upsert": {}, "delete": []}


def test_compute_scope_patch_full_replace_returns_complete_changed_battle_chunk() -> None:
    chunk_id = "minecraft:overworld|12|34"
    old_data = build_battle_chunk_data(symbol="A", colorRaw="#ff0000")
    new_data = build_battle_chunk_data(symbol="B", colorRaw="#00ff00")

    patch = ServerState.compute_scope_patch(
        {chunk_id: build_node(old_data)},
        {chunk_id: build_node(new_data)},
        full_replace=True,
    )

    assert patch["delete"] == []
    assert patch["upsert"] == {chunk_id: new_data}


def test_compute_scope_patch_full_replace_reports_deleted_battle_chunk() -> None:
    chunk_id = "minecraft:overworld|12|34"
    chunk_data = build_battle_chunk_data()

    patch = ServerState.compute_scope_patch(
        {chunk_id: build_node(chunk_data)},
        {},
        full_replace=True,
    )

    assert patch["upsert"] == {}
    assert patch["delete"] == [chunk_id]


def test_compute_web_map_patch_omits_unchanged_battle_chunks() -> None:
    chunk_id = "minecraft:overworld|12|34"
    chunk_data = build_battle_chunk_data()
    old_state = build_web_map_state(battle_chunks={chunk_id: chunk_data})
    new_state = deepcopy(old_state)

    patch = Broadcaster(ServerState())._compute_web_map_patch(old_state, new_state)

    assert "battleChunks" not in patch


def test_compute_web_map_patch_keeps_changed_battle_chunk_as_full_object() -> None:
    chunk_id = "minecraft:overworld|12|34"
    old_state = build_web_map_state(battle_chunks={chunk_id: build_battle_chunk_data(symbol="A")})
    new_state = build_web_map_state(battle_chunks={chunk_id: build_battle_chunk_data(symbol="B")})

    patch = Broadcaster(ServerState())._compute_web_map_patch(old_state, new_state)

    assert patch["battleChunks"]["delete"] == []
    assert patch["battleChunks"]["upsert"] == {
        chunk_id: new_state["battleChunks"][chunk_id],
    }


def test_apply_battle_map_observation_computes_semantic_hash_without_name_error() -> None:
    state = ServerState()

    result = state.apply_battle_map_observation(
        submit_player_id="player-1",
        room_code="room-a",
        mode="nodemc",
        dimension="minecraft:overworld",
        map_size=3,
        anchor_row=0,
        anchor_col=0,
        snapshot_observed_at=123456,
        parsed_at=123460,
        candidates=[
            {
                "baseChunkX": 12,
                "baseChunkZ": 34,
                "positionSampledAt": 123450,
                "source": "history_primary",
            }
        ],
        cells=[
            {
                "relChunkX": 0,
                "relChunkZ": 0,
                "symbol": "A",
                "colorRaw": "#ff0000",
            }
        ],
        current_time=1.0,
    )

    assert result["accepted"] is True
    assert result["upserted"] == 1
    assert state.battle_map_reporter_state["player-1"]["lastSemanticProjectionHash"]


def test_apply_battle_map_observation_preserves_simmc_mode_and_core_symbol_compatibility() -> None:
    state = ServerState()

    result = state.apply_battle_map_observation(
        submit_player_id="player-1",
        room_code="room-a",
        mode="simmc",
        dimension="minecraft:overworld",
        map_size=3,
        anchor_row=0,
        anchor_col=0,
        snapshot_observed_at=123456,
        parsed_at=123460,
        candidates=[
            {
                "baseChunkX": 12,
                "baseChunkZ": 34,
                "positionSampledAt": 123450,
                "source": "history_primary",
            }
        ],
        cells=[
            {
                "relChunkX": 0,
                "relChunkZ": 0,
                "symbol": "╫",
                "colorRaw": "#ff0000",
            }
        ],
        current_time=1.0,
    )

    chunk_report = state.battle_chunk_reports["room-a|minecraft:overworld|12|34"]["player-1"]["data"]

    assert result["accepted"] is True
    assert chunk_report["mode"] == "simmc"
    assert chunk_report["markerType"] == "war_core"


def test_build_battle_chunk_sync_data_defaults_legacy_mode_to_nodemc() -> None:
    state = ServerState()

    normalized = state.build_battle_chunk_sync_data(
        {
            "chunkX": 1,
            "chunkZ": 2,
            "dimension": "minecraft:overworld",
            "colorRaw": "#112233",
        },
        include_meta=False,
    )

    assert normalized["mode"] == "nodemc"

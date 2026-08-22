import asyncio
import sys
import time
from pathlib import Path
from types import SimpleNamespace

import pytest

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

from server.core.broadcaster import Broadcaster
from server.state import ServerState
from server.ws.sender import WebSocketSendHub, websocket_send_hub


CONNECTED = SimpleNamespace(name="CONNECTED")


class _FastWebSocket:
    def __init__(self) -> None:
        self.client_state = CONNECTED
        self.application_state = CONNECTED
        self.payloads: list[bytes] = []
        self.closed: tuple[int, str] | None = None

    async def send_bytes(self, payload: bytes) -> None:
        self.payloads.append(payload)

    async def close(self, *, code: int, reason: str) -> None:
        self.closed = (code, reason)
        disconnected = SimpleNamespace(name="DISCONNECTED")
        self.client_state = disconnected
        self.application_state = disconnected


class _BlockedWebSocket(_FastWebSocket):
    async def send_bytes(self, payload: bytes) -> None:
        await asyncio.Event().wait()


def _player_data(index: int, *, x_offset: float = 0.0) -> dict:
    return {
        "x": float(index) + x_offset,
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


@pytest.mark.asyncio
async def test_target_projection_is_encoded_once_per_group_under_load() -> None:
    state = ServerState()
    broadcaster = Broadcaster(state)
    stamp = time.monotonic()

    for index in range(400):
        state.upsert_report(
            state.player_reports,
            f"online-{index}",
            "external-source",
            state.build_state_node("external-source", stamp, _player_data(index)),
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

    source_socket = _FastWebSocket()
    state.connections["external-source"] = source_socket
    state.set_player_room("external-source", "load-room")
    state.set_connection_identity(
        "external-source",
        state.CLIENT_ROLE_EXTERNAL_SOURCE,
        "Load source",
    )
    state.mark_player_capability("external-source", "0.7.0")
    state.upsert_tab_player_report(
        "external-source",
        [
            {
                "uuid": f"00000000-0000-0000-2000-{index:012d}",
                "name": f"Tab{index}",
            }
            for index in range(2_000)
        ],
        stamp,
    )

    player_sockets: list[_FastWebSocket] = []
    for index in range(20):
        player_id = f"viewer-{index}"
        websocket = _FastWebSocket()
        player_sockets.append(websocket)
        state.connections[player_id] = websocket
        state.set_player_room(player_id, "load-room")
        state.mark_player_capability(player_id, "0.7.0")

    web_map_socket = _FastWebSocket()
    state.web_map_connections["web-map"] = web_map_socket
    state.set_web_map_room("web-map", "load-room")
    state.web_map_connection_protocols["web-map"] = "0.7.0"

    started = time.perf_counter()
    await broadcaster.broadcast_updates()
    full_duration = time.perf_counter() - started
    await asyncio.sleep(0.05)

    changed_at = time.monotonic()
    for index in range(400):
        state.upsert_report(
            state.player_reports,
            f"online-{index}",
            "external-source",
            state.build_state_node(
                "external-source",
                changed_at,
                _player_data(index, x_offset=1.0),
            ),
        )
    started = time.perf_counter()
    await broadcaster.broadcast_updates()
    patch_duration = time.perf_counter() - started
    await asyncio.sleep(0.05)

    assert full_duration < 2.5
    assert patch_duration < 1.0
    assert all(len(websocket.payloads) >= 2 for websocket in player_sockets)
    assert len(web_map_socket.payloads) >= 2
    await websocket_send_hub.close()


@pytest.mark.asyncio
async def test_blocked_socket_is_disconnected_without_blocking_other_senders() -> None:
    hub = WebSocketSendHub(control_capacity=32, send_timeout_sec=0.05)
    blocked = _BlockedWebSocket()
    healthy = _FastWebSocket()

    await hub.send(blocked, b"blocked", coalesce_state=True, wait=False)
    await hub.send(healthy, b"healthy", coalesce_state=False, wait=True)

    await asyncio.sleep(0.08)
    assert healthy.payloads == [b"healthy"]
    assert blocked.closed == (1013, "slow_client_send_timeout")
    assert hub.snapshot()["slowClientDisconnects"] == 1
    await hub.close()

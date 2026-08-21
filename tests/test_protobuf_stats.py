from __future__ import annotations

from pathlib import Path
import sys

import pytest

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

from server.admin.protobuf_stats import ProtobufStatsService
from server.admin import traffic as traffic_module


@pytest.mark.asyncio
async def test_protobuf_stats_aggregates_live_session_and_connection_metrics(monkeypatch: pytest.MonkeyPatch) -> None:
    player_socket = object()
    web_map_socket = object()
    active_connections = [
        {"actorId": "player-1", "channel": "player", "websocket": player_socket},
        {"actorId": "web-map-1", "channel": "web_map", "websocket": web_map_socket},
    ]
    service = ProtobufStatsService(resolve_active_connections=lambda: active_connections, live_window_sec=10)

    service.record_nowait(
        websocket=player_socket,
        message_type="snapshot_full",
        byte_count=256 * 1024,
        occurred_at=100.0,
    )
    service.record_nowait(
        websocket=player_socket,
        message_type="patch",
        byte_count=512,
        occurred_at=104.0,
    )
    service.record_nowait(
        websocket=web_map_socket,
        message_type="snapshot_full",
        byte_count=1024,
        occurred_at=105.0,
    )
    monkeypatch.setattr("server.admin.protobuf_stats.time.time", lambda: 105.0)

    payload = await service.build_live_payload()

    assert payload["sampleWindowSec"] == 10
    assert payload["total"]["messageCount"] == 3
    assert payload["total"]["byteCount"] == 256 * 1024 + 512 + 1024
    assert payload["total"]["messagesPerSecond"] == pytest.approx(0.3)
    assert payload["snapshotFull"]["messageCount"] == 2
    assert payload["snapshotFull"]["maxPacketBytes"] == 256 * 1024
    assert payload["messageTypes"][0]["messageType"] == "snapshot_full"

    player = next(item for item in payload["connections"] if item["actorId"] == "player-1")
    assert player["total"]["messageCount"] == 2
    assert player["snapshotFull"]["byteCount"] == 256 * 1024
    assert {item["messageType"] for item in player["messageTypes"]} == {"snapshot_full", "patch"}

    active_connections.clear()
    payload_after_disconnect = await service.build_live_payload()
    assert payload_after_disconnect["connections"] == []
    assert payload_after_disconnect["total"]["messageCount"] == 3


@pytest.mark.asyncio
async def test_tracked_send_only_records_successful_protobuf_packets(monkeypatch: pytest.MonkeyPatch) -> None:
    sent: list[bytes] = []
    recorded: list[tuple[object, str | None, int]] = []

    class SuccessfulSocket:
        async def send_bytes(self, payload: bytes) -> None:
            sent.append(payload)

    class FailingSocket:
        async def send_bytes(self, _payload: bytes) -> None:
            raise RuntimeError("closed")

    async def ignore_application_traffic(**_kwargs) -> None:
        return None

    monkeypatch.setattr(traffic_module, "record_websocket_payload_traffic", ignore_application_traffic)
    monkeypatch.setattr(
        traffic_module,
        "record_protobuf_packet_nowait",
        lambda *, websocket, message_type, byte_count: recorded.append((websocket, message_type, byte_count)),
    )

    success_socket = SuccessfulSocket()
    await traffic_module.send_tracked_websocket_bytes(
        success_socket,
        b"protobuf",
        channel="player",
        protobuf_type="snapshot_full",
    )
    with pytest.raises(RuntimeError, match="closed"):
        await traffic_module.send_tracked_websocket_bytes(
            FailingSocket(),
            b"protobuf",
            channel="player",
            protobuf_type="snapshot_full",
        )

    assert sent == [b"protobuf"]
    assert recorded == [(success_socket, "snapshot_full", len(b"protobuf"))]

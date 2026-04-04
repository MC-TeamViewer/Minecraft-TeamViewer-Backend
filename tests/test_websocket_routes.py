"""
WebSocket 路由与协议收口测试。

覆盖：
1. /web-map/ws 识别 legacy MessagePack 握手，仅提示升级并关闭；
2. /adminws 作为开发期 alias 仍可承载网页地图握手；
3. /admin/ws 仅预留未来后台管理入口；
4. route / wire channel 不一致会在握手阶段被拒绝。
"""

import asyncio
import logging
import socket
import sys
import threading
import time
import uuid
from pathlib import Path

import msgpack
import pytest
import uvicorn
import websockets

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

import main
from main import app, truncate_websocket_close_reason
from server.codec import ProtobufMessageCodec


CODEC = ProtobufMessageCodec()


def _find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


@pytest.fixture(scope="module")
def live_server() -> str:
    port = _find_free_port()
    config = uvicorn.Config(
        app,
        host="127.0.0.1",
        port=port,
        log_level="warning",
        ws_per_message_deflate=True,
    )
    server = uvicorn.Server(config)
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()

    deadline = time.time() + 10.0
    while not server.started:
        if not thread.is_alive():
            raise RuntimeError("test uvicorn server exited before startup")
        if time.time() >= deadline:
            raise RuntimeError("timed out waiting for test uvicorn server to start")
        time.sleep(0.05)

    yield f"ws://127.0.0.1:{port}"

    server.should_exit = True
    thread.join(timeout=10.0)


def build_handshake(
    *,
    channel: str,
    protocol_version: str = "0.6.0",
    room_code: str = "test-room",
    submit_player_id: str | None = None,
) -> bytes:
    payload = {
        "type": "handshake",
        "channel": channel,
        "networkProtocolVersion": protocol_version,
        "minimumCompatibleNetworkProtocolVersion": protocol_version,
        "localProgramVersion": "test-client",
        "roomCode": room_code,
    }
    if submit_player_id:
        payload["submitPlayerId"] = submit_player_id
    return CODEC.encode(payload)


def decode_packet(payload: bytes) -> dict:
    return CODEC.decode(payload)


def decode_legacy_msgpack_packet(payload: bytes) -> dict:
    return msgpack.unpackb(payload, raw=False)


def test_truncate_websocket_close_reason_limits_utf8_bytes() -> None:
    reason = (
        "unsupported_protocol_version: 当前服务器仅支持 Protobuf 协议（0.6.0 及以上）。"
        "MessagePack 协议（0.5.x 及更早版本）已不再支持。"
        "请升级到最新版本的客户端后重试。"
    )
    truncated = truncate_websocket_close_reason(reason)

    assert len(truncated.encode("utf-8")) <= 123
    assert truncated


@pytest.mark.asyncio
async def test_web_map_route_rejects_legacy_msgpack_handshake_with_upgrade_hint(
    live_server: str,
    caplog: pytest.LogCaptureFixture,
) -> None:
    legacy_handshake = {
        "type": "handshake",
        "networkProtocolVersion": "0.5.0",
        "localProgramVersion": "teamviewer-client-0.5.2",
        "roomCode": "test-room",
    }

    with caplog.at_level(logging.WARNING, logger="teamviewrelay.main"):
        async with websockets.connect(f"{live_server}/web-map/ws") as websocket:
            await websocket.send(msgpack.packb(legacy_handshake, use_bin_type=True))

            handshake_ack = decode_legacy_msgpack_packet(await asyncio.wait_for(websocket.recv(), timeout=5.0))
            assert handshake_ack["type"] == "handshake_ack"
            assert handshake_ack.get("ready") is False
            assert "unsupported_protocol_version" in str(handshake_ack.get("rejectReason") or "")

            with pytest.raises(websockets.exceptions.ConnectionClosedError) as exc_info:
                await asyncio.wait_for(websocket.recv(), timeout=5.0)

    assert exc_info.value.code == 1008
    assert "unsupported_protocol_version" in (exc_info.value.reason or "")
    assert "legacy MessagePack handshake" in caplog.text


@pytest.mark.asyncio
async def test_player_route_rejects_legacy_msgpack_handshake_with_upgrade_hint(
    live_server: str,
    caplog: pytest.LogCaptureFixture,
) -> None:
    legacy_handshake = {
        "type": "handshake",
        "networkProtocolVersion": "0.5.0",
        "minimumCompatibleNetworkProtocolVersion": "0.5.0",
        "localProgramVersion": "teamviewer-client-0.5.2",
        "roomCode": "test-room",
        "submitPlayerId": uuid.UUID("00000000-0000-0000-0000-000000000001").bytes,
    }

    with caplog.at_level(logging.WARNING, logger="teamviewrelay.main"):
        async with websockets.connect(f"{live_server}/mc-client") as websocket:
            await websocket.send(msgpack.packb(legacy_handshake, use_bin_type=True))

            handshake_ack = decode_legacy_msgpack_packet(await asyncio.wait_for(websocket.recv(), timeout=5.0))
            assert handshake_ack["type"] == "handshake_ack"
            assert handshake_ack.get("ready") is False
            assert "unsupported_protocol_version" in str(handshake_ack.get("rejectReason") or "")

            with pytest.raises(websockets.exceptions.ConnectionClosedError) as exc_info:
                await asyncio.wait_for(websocket.recv(), timeout=5.0)

    assert exc_info.value.code == 1008
    assert "unsupported_protocol_version" in (exc_info.value.reason or "")
    assert "legacy MessagePack handshake" in caplog.text
    assert "Rejecting legacy MessagePack client" in caplog.text


@pytest.mark.asyncio
async def test_adminws_alias_accepts_web_map_handshake_and_logs_deprecation(
    live_server: str,
    caplog: pytest.LogCaptureFixture,
) -> None:
    with caplog.at_level(logging.WARNING, logger="teamviewrelay.main"):
        async with websockets.connect(f"{live_server}/adminws") as websocket:
            await websocket.send(build_handshake(channel="web_map"))

            handshake_ack = decode_packet(await asyncio.wait_for(websocket.recv(), timeout=5.0))
            snapshot_full = decode_packet(await asyncio.wait_for(websocket.recv(), timeout=5.0))

    assert handshake_ack["type"] == "handshake_ack"
    assert handshake_ack.get("ready") is True
    assert snapshot_full["type"] == "snapshot_full"
    assert "Deprecated websocket route /adminws used" in caplog.text


@pytest.mark.asyncio
async def test_reserved_admin_route_returns_placeholder_rejection(live_server: str) -> None:
    async with websockets.connect(f"{live_server}/admin/ws") as websocket:
        await websocket.send(build_handshake(channel="admin"))

        handshake_ack = decode_packet(await asyncio.wait_for(websocket.recv(), timeout=5.0))
        assert handshake_ack["type"] == "handshake_ack"
        assert handshake_ack.get("ready") is not True
        assert "admin_interface_reserved" in str(handshake_ack.get("rejectReason") or "")

        with pytest.raises(websockets.exceptions.ConnectionClosedError) as exc_info:
            await asyncio.wait_for(websocket.recv(), timeout=5.0)

    assert exc_info.value.code == 1008


@pytest.mark.asyncio
async def test_web_map_route_rejects_player_wire_channel_handshake(live_server: str) -> None:
    async with websockets.connect(f"{live_server}/web-map/ws") as websocket:
        await websocket.send(build_handshake(channel="player"))

        handshake_ack = decode_packet(await asyncio.wait_for(websocket.recv(), timeout=5.0))
        assert handshake_ack["type"] == "handshake_ack"
        assert handshake_ack.get("ready") is not True
        assert "channel_mismatch" in str(handshake_ack.get("rejectReason") or "")

        with pytest.raises(websockets.exceptions.ConnectionClosedError) as exc_info:
            await asyncio.wait_for(websocket.recv(), timeout=5.0)

    assert exc_info.value.code == 1008


@pytest.mark.asyncio
async def test_player_route_does_not_register_connection_before_handshake_ack_send(
    live_server: str,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    submit_player_id = "00000000-0000-0000-0000-000000000123"
    original_send_packet = main.send_packet

    async def failing_send_packet(websocket, packet, *, channel=None):
        if getattr(packet, "type", None) == "handshake_ack":
            raise RuntimeError("simulated player handshake ack failure")
        return await original_send_packet(websocket, packet, channel=channel)

    monkeypatch.setattr(main, "send_packet", failing_send_packet)

    async with websockets.connect(f"{live_server}/mc-client") as websocket:
        await websocket.send(build_handshake(channel="player", submit_player_id=submit_player_id))

        with pytest.raises(websockets.exceptions.ConnectionClosed):
            await asyncio.wait_for(websocket.recv(), timeout=5.0)

    await asyncio.sleep(0.1)
    assert submit_player_id not in main.state.connections
    assert submit_player_id not in main.state.connection_caps
    assert submit_player_id not in main.state.connection_rooms


@pytest.mark.asyncio
async def test_web_map_route_does_not_register_connection_before_handshake_ack_send(
    live_server: str,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_send_packet = main.send_packet

    async def failing_send_packet(websocket, packet, *, channel=None):
        if getattr(packet, "type", None) == "handshake_ack" and channel == "web_map":
            raise RuntimeError("simulated web-map handshake ack failure")
        return await original_send_packet(websocket, packet, channel=channel)

    monkeypatch.setattr(main, "send_packet", failing_send_packet)

    async with websockets.connect(f"{live_server}/web-map/ws") as websocket:
        await websocket.send(build_handshake(channel="web_map"))

        with pytest.raises(websockets.exceptions.ConnectionClosed):
            await asyncio.wait_for(websocket.recv(), timeout=5.0)

    await asyncio.sleep(0.1)
    assert not main.state.web_map_connections
    assert not main.state.web_map_connection_rooms

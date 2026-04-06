import sys

import msgpack
from fastapi import WebSocket, WebSocketDisconnect

from ..app import runtime
from ..core.protocol import (
    BattleMapObservationPacket,
    EntitiesPatchPacket,
    EntitiesUpdatePacket,
    HandshakeAckPacket,
    HandshakeHelpers,
    HandshakePacket,
    PacketDecodeError,
    PlayerReportBundlePacket,
    PlayersPatchPacket,
    PlayersUpdatePacket,
    SourceStateClearPacket,
    StateKeepalivePacket,
    TabPlayersPatchPacket,
    TabPlayersUpdatePacket,
    WaypointsDeletePacket,
    WaypointsEntityDeathCancelPacket,
    WaypointsPatchPacket,
    WaypointsUpdatePacket,
)
from ..core.uuid_codec import normalize_inbound_uuid_fields
from ..state import ServerState


async def _raw_send_packet(websocket: WebSocket, packet, *, channel: str | None = None) -> None:
    if channel:
        if isinstance(packet, dict):
            body = dict(packet)
        else:
            body = packet.model_dump(exclude_none=True)
        body["channel"] = channel
        await websocket.send_bytes(runtime.message_codec.encode(body))
        return
    await websocket.send_bytes(runtime.message_codec.encode(packet))


def _resolve_send_packet():
    main_module = sys.modules.get("main")
    current = getattr(main_module, "send_packet", None) if main_module is not None else None
    if callable(current) and current is not send_packet:
        return current
    return _raw_send_packet


async def send_packet(websocket: WebSocket, packet, *, channel: str | None = None) -> None:
    sender = _resolve_send_packet()
    if sender is not _raw_send_packet:
        await sender(websocket, packet, channel=channel)
        return
    await _raw_send_packet(websocket, packet, channel=channel)


async def send_legacy_messagepack_packet(websocket: WebSocket, packet: dict) -> None:
    await websocket.send_bytes(msgpack.packb(packet, use_bin_type=True))


def describe_websocket(websocket: WebSocket) -> str:
    state_text = ServerState.websocket_state_label(websocket)
    close_code = getattr(websocket, "close_code", None)
    close_reason = getattr(websocket, "close_reason", None)
    return (
        f"state=({state_text}), "
        f"closeCode={close_code if close_code is not None else 'unknown'}, "
        f"closeReason={close_reason!r}"
    )


def _decode_legacy_messagepack_handshake(payload: bytes | bytearray | memoryview | str) -> dict | None:
    if isinstance(payload, str):
        return None

    raw = payload.tobytes() if isinstance(payload, memoryview) else bytes(payload)
    if not raw:
        return None

    try:
        data = msgpack.unpackb(raw, raw=False)
    except Exception:
        return None

    if not isinstance(data, dict):
        return None
    if data.get("type") != "handshake":
        return None

    data = normalize_inbound_uuid_fields(data)
    data["_legacy_msgpack"] = True
    return data


def truncate_websocket_close_reason(reason: str, max_bytes: int = 123) -> str:
    text = str(reason or "")
    encoded = text.encode("utf-8")
    if len(encoded) <= max_bytes:
        return text

    truncated = encoded[:max_bytes]
    while truncated:
        try:
            return truncated.decode("utf-8")
        except UnicodeDecodeError:
            truncated = truncated[:-1]
    return ""


async def receive_payload(websocket: WebSocket, *, allow_legacy_handshake: bool = False) -> dict:
    message = await websocket.receive()
    if message.get("type") == "websocket.disconnect":
        raise WebSocketDisconnect(code=message.get("code", 1000))

    payload = message.get("bytes")
    if payload is None:
        payload = message.get("text")
    if payload is None:
        raise PacketDecodeError("invalid_payload", "payload must be bytes")

    try:
        return runtime.message_codec.decode(payload)
    except PacketDecodeError as proto_error:
        if not allow_legacy_handshake:
            raise proto_error

        legacy_handshake = _decode_legacy_messagepack_handshake(payload)
        if legacy_handshake is None:
            raise proto_error

        runtime.logger.warning(
            "Detected legacy MessagePack handshake (clientProtocol=%s, programVersion=%s). "
            "This server version only supports Protobuf protocol (0.6.1+). "
            "Please upgrade your client.",
            legacy_handshake.get("networkProtocolVersion", "unknown"),
            legacy_handshake.get("localProgramVersion", "unknown"),
        )
        return legacy_handshake


def require_wire_channel(payload: dict, expected_channel: str, route_path: str) -> None:
    if payload.get("_legacy_msgpack"):
        return

    actual_channel = payload.get("_wire_channel")
    if actual_channel is None:
        return

    actual_text = str(actual_channel).strip().lower()
    expected_text = str(expected_channel).strip().lower()
    if actual_text == expected_text:
        return

    raise PacketDecodeError(
        "channel_mismatch",
        f"channel_mismatch: route={route_path}, expected={expected_text}, actual={actual_text}",
    )


def resolve_handshake_rejection_reason(packet: HandshakePacket) -> str | None:
    client_protocol = HandshakeHelpers.protocol_version(packet)
    client_min_compatible = HandshakeHelpers.minimum_compatible_protocol_version(packet, client_protocol)

    if not HandshakeHelpers.protocol_at_least(client_protocol, runtime.SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION):
        return (
            "client_protocol_too_old: "
            f"client={client_protocol}, required>={runtime.SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION}"
        )

    if not HandshakeHelpers.protocol_at_least(runtime.NETWORK_PROTOCOL_VERSION, client_min_compatible):
        return (
            "server_protocol_too_old: "
            f"server={runtime.NETWORK_PROTOCOL_VERSION}, clientRequires>={client_min_compatible}"
        )

    return None


async def reject_handshake(
    websocket: WebSocket,
    reason: str,
    room_code: str,
    *,
    channel: str = "player",
    send_ack: bool = True,
    legacy_messagepack_ack: bool = False,
) -> None:
    ack_packet = HandshakeAckPacket(
        ready=False,
        networkProtocolVersion=runtime.NETWORK_PROTOCOL_VERSION,
        minimumCompatibleNetworkProtocolVersion=runtime.SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION,
        localProgramVersion=runtime.SERVER_PROGRAM_VERSION,
        roomCode=room_code,
        deltaEnabled=True,
        error="version_incompatible",
        rejectReason=reason,
        broadcastHz=runtime.state.broadcast_hz,
        playerTimeoutSec=runtime.state.PLAYER_TIMEOUT,
        entityTimeoutSec=runtime.state.ENTITY_TIMEOUT,
        battleChunkTimeoutSec=runtime.state.BATTLE_CHUNK_TIMEOUT,
    )

    if legacy_messagepack_ack:
        await send_legacy_messagepack_packet(
            websocket,
            {
                "type": "handshake_ack",
                "ready": False,
                "networkProtocolVersion": runtime.NETWORK_PROTOCOL_VERSION,
                "minimumCompatibleNetworkProtocolVersion": runtime.SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION,
                "localProgramVersion": runtime.SERVER_PROGRAM_VERSION,
                "roomCode": room_code,
                "deltaEnabled": True,
                "error": "version_incompatible",
                "rejectReason": reason,
                "broadcastHz": runtime.state.broadcast_hz,
                "playerTimeoutSec": runtime.state.PLAYER_TIMEOUT,
                "entityTimeoutSec": runtime.state.ENTITY_TIMEOUT,
            },
        )
    elif send_ack:
        await send_packet(websocket, ack_packet, channel=channel)

    close_reason = truncate_websocket_close_reason(reason)
    await websocket.close(code=1008, reason=close_reason)


def normalize_waypoint_color_to_int(color_value, fallback: int = 0xEF4444) -> int:
    if isinstance(color_value, (int, float)):
        value = int(color_value)
        return max(0, min(value, 0xFFFFFF))

    text = str(color_value or "").strip()
    if not text:
        return fallback

    if text.startswith("#"):
        text = text[1:]
    if text.lower().startswith("0x"):
        text = text[2:]

    if len(text) != 6:
        return fallback

    try:
        return int(text, 16)
    except ValueError:
        return fallback


def expand_player_packets(packet) -> list:
    if not isinstance(packet, PlayerReportBundlePacket):
        return [packet]

    submit_player_id = packet.submitPlayerId
    expanded: list = []

    if packet.playersReplace:
        expanded.append(
            PlayersUpdatePacket(
                type="players_update",
                submitPlayerId=submit_player_id,
                players=packet.playersReplace,
            )
        )
    if packet.playersPatch is not None and (packet.playersPatch.upsert or packet.playersPatch.delete):
        expanded.append(
            PlayersPatchPacket(
                type="players_patch",
                submitPlayerId=submit_player_id,
                upsert=packet.playersPatch.upsert,
                delete=packet.playersPatch.delete,
            )
        )
    if packet.entitiesReplace:
        expanded.append(
            EntitiesUpdatePacket(
                type="entities_update",
                submitPlayerId=submit_player_id,
                entities=packet.entitiesReplace,
            )
        )
    if packet.entitiesPatch is not None and (packet.entitiesPatch.upsert or packet.entitiesPatch.delete):
        expanded.append(
            EntitiesPatchPacket(
                type="entities_patch",
                submitPlayerId=submit_player_id,
                upsert=packet.entitiesPatch.upsert,
                delete=packet.entitiesPatch.delete,
            )
        )
    if packet.waypointsReplace:
        expanded.append(
            WaypointsUpdatePacket(
                type="waypoints_update",
                submitPlayerId=submit_player_id,
                waypoints=packet.waypointsReplace,
            )
        )
    if packet.waypointsPatch is not None and (packet.waypointsPatch.upsert or packet.waypointsPatch.delete):
        expanded.append(
            WaypointsPatchPacket(
                type="waypoints_patch",
                submitPlayerId=submit_player_id,
                upsert=packet.waypointsPatch.upsert,
                delete=packet.waypointsPatch.delete,
            )
        )
    if packet.tabPlayersReplace:
        expanded.append(
            TabPlayersUpdatePacket(
                type="tab_players_update",
                submitPlayerId=submit_player_id,
                tabPlayers=packet.tabPlayersReplace,
            )
        )
    if packet.tabPlayersPatch is not None and (packet.tabPlayersPatch.upsert or packet.tabPlayersPatch.delete):
        expanded.append(
            TabPlayersPatchPacket(
                type="tab_players_patch",
                submitPlayerId=submit_player_id,
                upsert=packet.tabPlayersPatch.upsert,
                delete=packet.tabPlayersPatch.delete,
            )
        )
    if packet.battleMapObservation is not None:
        expanded.append(
            BattleMapObservationPacket(
                type="battle_map_observation",
                submitPlayerId=submit_player_id,
                **packet.battleMapObservation.model_dump(exclude={"type", "submitPlayerId"}, exclude_none=True),
            )
        )
    if packet.stateKeepalive is not None:
        expanded.append(
            StateKeepalivePacket(
                type="state_keepalive",
                submitPlayerId=submit_player_id,
                **packet.stateKeepalive.model_dump(exclude={"type", "submitPlayerId"}, exclude_none=True),
            )
        )
    if packet.sourceStateClear is not None:
        expanded.append(
            SourceStateClearPacket(
                type="source_state_clear",
                submitPlayerId=submit_player_id,
                **packet.sourceStateClear.model_dump(exclude={"type", "submitPlayerId"}, exclude_none=True),
            )
        )
    if packet.waypointsDelete is not None:
        expanded.append(
            WaypointsDeletePacket(
                type="waypoints_delete",
                submitPlayerId=submit_player_id,
                **packet.waypointsDelete.model_dump(exclude={"type", "submitPlayerId"}, exclude_none=True),
            )
        )
    if packet.waypointsEntityDeathCancel is not None:
        expanded.append(
            WaypointsEntityDeathCancelPacket(
                type="waypoints_entity_death_cancel",
                submitPlayerId=submit_player_id,
                **packet.waypointsEntityDeathCancel.model_dump(exclude={"type", "submitPlayerId"}, exclude_none=True),
            )
        )

    return expanded

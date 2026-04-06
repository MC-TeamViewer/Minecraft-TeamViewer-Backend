import base64
import binascii
import json
import logging
import os
import secrets
import asyncio
import ipaddress
import time
import uuid
from contextlib import asynccontextmanager
from functools import lru_cache
from pathlib import Path

import msgpack
from fastapi import FastAPI, Query, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse, JSONResponse, PlainTextResponse, StreamingResponse
from pydantic import ValidationError

from server.admin_frontend import ADMIN_UI_INDEX_PATH, admin_ui_ready, resolve_admin_asset_path
from server.admin_sse import AdminSseHub
from server.admin_store import AdminStore, AdminStoreConfig
from server.broadcaster import Broadcaster
from server.codec import ProtobufMessageCodec
from server.models import BattleChunkData, EntityData, PlayerData, WaypointData
from server.protocol import (
    BattleChunkMetaRequestPacket,
    BattleChunkMetaSnapshotPacket,
    BattleMapObservationPacket,
    CommandPlayerMarkClearAllPacket,
    CommandPlayerMarkClearPacket,
    CommandPlayerMarkSetPacket,
    CommandSameServerFilterSetPacket,
    CommandTacticalWaypointSetPacket,
    EntitiesPatchPacket,
    EntitiesUpdatePacket,
    HandshakeAckPacket,
    HandshakeHelpers,
    HandshakePacket,
    PacketDecodeError,
    PacketParsers,
    PingPacket,
    PlayerReportBundlePacket,
    PongPacket,
    PlayersPatchPacket,
    PlayersUpdatePacket,
    ResyncRequestPacket,
    SourceStateClearPacket,
    StateKeepalivePacket,
    TabPlayersPatchPacket,
    TabPlayersUpdatePacket,
    WaypointsDeletePacket,
    WaypointsEntityDeathCancelPacket,
    WaypointsPatchPacket,
    WaypointsUpdatePacket,
    WebMapAckPacket,
)
from server.state import ServerState
from server.uuid_codec import normalize_inbound_uuid_fields



NETWORK_PROTOCOL_VERSION = "0.6.1" # 服务器使用的协议版本
SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION = "0.6.1" # 服务器兼容的最低协议版本
SERVER_PROGRAM_VERSION = "team-view-relay-server-dev"
LEGACY_PROTOCOL_REJECTION_REASON = (
    "unsupported_protocol_version: "
    "当前服务器仅支持 Protobuf 协议（0.6.1 及以上）。"
    "battleChunks 同步与一致性修正要求客户端升级到 0.6.1。"
    "MessagePack 协议（0.5.x 及更早版本）已不再支持。"
    "请升级到最新版本的客户端后重试。"
)


def configure_logging() -> None:
    if logging.getLogger().handlers:
        return

    level_name = os.getenv("TEAMVIEWER_LOG_LEVEL", "INFO").upper()
    level = getattr(logging, level_name, logging.INFO)
    logging.basicConfig(
        level=level,
        format="%(asctime)s %(levelname)s [%(name)s] %(message)s",
    )


configure_logging()
logger = logging.getLogger("teamviewrelay.main")

message_codec = ProtobufMessageCodec()
admin_store: AdminStore | None = None
admin_retention_task: asyncio.Task | None = None
admin_sse_hub = AdminSseHub()
web_map_connection_meta: dict[str, dict] = {}
DEFAULT_ADMIN_DB_PATH = "./data/teamviewer-admin.db"


def parse_positive_int_env(name: str, default: int, *, minimum: int, maximum: int) -> int:
    raw = os.getenv(name)
    if raw is None:
        return default
    try:
        value = int(raw)
    except ValueError:
        return default
    return max(minimum, min(value, maximum))


def parse_bool_env(name: str, default: bool = False) -> bool:
    raw = os.getenv(name)
    if raw is None:
        return default
    return str(raw).strip().lower() in {"1", "true", "yes", "on"}


def build_admin_store_config() -> AdminStoreConfig:
    return AdminStoreConfig(
        db_path=os.getenv("TEAMVIEWER_DB_PATH", DEFAULT_ADMIN_DB_PATH),
        audit_retention_days=parse_positive_int_env("TEAMVIEWER_AUDIT_RETENTION_DAYS", 90, minimum=1, maximum=3650),
        hourly_retention_days=parse_positive_int_env("TEAMVIEWER_HOURLY_RETENTION_DAYS", 90, minimum=1, maximum=3650),
        daily_retention_days=parse_positive_int_env("TEAMVIEWER_DAILY_RETENTION_DAYS", 400, minimum=1, maximum=3650),
    )


def get_remote_addr_from_client(client) -> str | None:
    host = getattr(client, "host", None)
    if isinstance(host, str) and host.strip():
        return host
    return None


def normalize_ip_text(value: str | None) -> str | None:
    text = str(value or "").strip().strip('"')
    if not text:
        return None

    if text.startswith("[") and "]" in text:
        text = text[1:text.index("]")]

    try:
        return str(ipaddress.ip_address(text))
    except ValueError:
        pass

    if text.count(":") == 1 and "." in text:
        host, _, _port = text.rpartition(":")
        try:
            return str(ipaddress.ip_address(host))
        except ValueError:
            return None

    return None


def extract_client_ip_from_headers(headers) -> str | None:
    if headers is None:
        return None

    x_forwarded_for = headers.get("x-forwarded-for")
    if isinstance(x_forwarded_for, str):
        for item in x_forwarded_for.split(","):
            normalized = normalize_ip_text(item)
            if normalized:
                return normalized

    x_real_ip = headers.get("x-real-ip")
    if isinstance(x_real_ip, str):
        normalized = normalize_ip_text(x_real_ip)
        if normalized:
            return normalized

    return None


@lru_cache(maxsize=1)
def get_trusted_proxy_networks() -> tuple[ipaddress._BaseNetwork, ...]:
    raw_value = os.getenv(
        "TEAMVIEWER_TRUSTED_PROXY_CIDRS",
        "127.0.0.1/32,::1/128,172.16.0.0/12",
    )
    networks: list[ipaddress._BaseNetwork] = []

    for raw_item in str(raw_value).split(","):
        item = raw_item.strip()
        if not item:
            continue
        try:
            networks.append(ipaddress.ip_network(item, strict=False))
        except ValueError:
            logger.warning("Ignore invalid TEAMVIEWER_TRUSTED_PROXY_CIDRS entry: %s", item)

    return tuple(networks)


def should_trust_proxy_headers(client_host: str | None) -> bool:
    if not parse_bool_env("TEAMVIEWER_TRUST_PROXY_HEADERS", False):
        return False

    normalized = normalize_ip_text(client_host)
    if normalized is None:
        return False

    ip_obj = ipaddress.ip_address(normalized)
    return any(ip_obj in network for network in get_trusted_proxy_networks())


def get_effective_remote_addr(client, headers=None) -> str | None:
    direct_addr = get_remote_addr_from_client(client)
    if not should_trust_proxy_headers(direct_addr):
        return direct_addr

    forwarded_addr = extract_client_ip_from_headers(headers)
    return forwarded_addr or direct_addr


def get_request_remote_addr(request: Request) -> str | None:
    return get_effective_remote_addr(request.client, request.headers)


def get_websocket_remote_addr(websocket: WebSocket) -> str | None:
    return get_effective_remote_addr(websocket.client, websocket.headers)


def admin_ui_unavailable_response() -> PlainTextResponse:
    return PlainTextResponse(
        "admin_ui_not_built: run `cd admin-ui && pnpm install && pnpm build` first",
        status_code=503,
    )


async def record_audit_event(
    *,
    event_type: str,
    actor_type: str,
    actor_id: str | None = None,
    room_code: str | None = None,
    success: bool = True,
    remote_addr: str | None = None,
    detail: dict | None = None,
    occurred_at: float | None = None,
) -> None:
    if admin_store is None:
        return
    try:
        await admin_store.record_audit_event(
            event_type=event_type,
            actor_type=actor_type,
            actor_id=actor_id,
            room_code=room_code,
            success=success,
            remote_addr=remote_addr,
            detail=detail,
            occurred_at=occurred_at,
        )
        trigger_admin_sse_audit()
    except Exception as exc:
        logger.warning("Failed to persist audit event type=%s: %s", event_type, exc)


async def record_player_activity(player_id: str | None, room_code: str | None, *, occurred_at: float | None = None) -> None:
    if admin_store is None or not isinstance(player_id, str) or not player_id:
        return
    try:
        await admin_store.record_player_activity(
            player_id,
            state.normalize_room_code(room_code),
            occurred_at=occurred_at,
        )
        trigger_admin_sse_metrics()
    except Exception as exc:
        logger.warning("Failed to persist player activity playerId=%s: %s", player_id, exc)


def parse_basic_auth_header(header_value: str | None) -> tuple[str | None, str | None]:
    if not isinstance(header_value, str):
        return None, None

    scheme, _, encoded = header_value.partition(" ")
    if scheme.lower() != "basic" or not encoded:
        return None, None

    try:
        decoded = base64.b64decode(encoded).decode("utf-8")
    except (binascii.Error, UnicodeDecodeError):
        return None, None

    username, separator, password = decoded.partition(":")
    if not separator:
        return None, None
    return username, password


def admin_unauthorized_response(detail: str) -> JSONResponse:
    return JSONResponse(
        {"detail": detail},
        status_code=401,
        headers={"WWW-Authenticate": 'Basic realm="TeamViewRelay Admin"'},
    )


async def authenticate_admin_request(request: Request) -> str | JSONResponse:
    configured_username = os.getenv("TEAMVIEWER_ADMIN_USERNAME", "admin")
    configured_password = os.getenv("TEAMVIEWER_ADMIN_PASSWORD", "admin")
    remote_addr = get_request_remote_addr(request)

    if not configured_username or not configured_password:
        await record_audit_event(
            event_type="admin_auth_failed",
            actor_type="admin",
            success=False,
            remote_addr=remote_addr,
            detail={"reason": "admin_not_configured", "path": request.url.path},
        )
        return JSONResponse({"detail": "admin_not_configured"}, status_code=503)

    username, password = parse_basic_auth_header(request.headers.get("authorization"))
    auth_ok = (
        isinstance(username, str)
        and isinstance(password, str)
        and secrets.compare_digest(username, configured_username)
        and secrets.compare_digest(password, configured_password)
    )
    if not auth_ok:
        await record_audit_event(
            event_type="admin_auth_failed",
            actor_type="admin",
            actor_id=username,
            success=False,
            remote_addr=remote_addr,
            detail={"reason": "invalid_credentials", "path": request.url.path},
        )
        return admin_unauthorized_response("invalid_admin_credentials")

    await record_audit_event(
        event_type="admin_auth_success",
        actor_type="admin",
        actor_id=username,
        success=True,
        remote_addr=remote_addr,
        detail={"path": request.url.path, "method": request.method},
    )
    return username


async def record_admin_access(request: Request, username: str, access_type: str) -> None:
    await record_audit_event(
        event_type=access_type,
        actor_type="admin",
        actor_id=username,
        success=True,
        remote_addr=get_request_remote_addr(request),
        detail={"path": request.url.path, "method": request.method, "query": str(request.url.query or "")},
    )


def normalize_optional_room_code(room_code: str | None) -> str | None:
    if room_code is None:
        return None
    text = str(room_code).strip()
    if not text:
        return None
    return state.normalize_room_code(text)


def build_room_overview() -> list[dict]:
    room_index: dict[str, dict] = {}

    def ensure_room(room_code: str) -> dict:
        room = room_index.get(room_code)
        if room is None:
            room = {
                "roomCode": room_code,
                "playerConnections": 0,
                "webMapConnections": 0,
                "playerIds": [],
                "webMapIds": [],
            }
            room_index[room_code] = room
        return room

    for player_id in sorted(state.connections.keys()):
        room_code = state.get_player_room(player_id)
        room = ensure_room(room_code)
        room["playerConnections"] += 1
        room["playerIds"].append(player_id)

    for web_map_id in sorted(state.web_map_connections.keys()):
        room_code = state.get_web_map_room(web_map_id)
        room = ensure_room(room_code)
        room["webMapConnections"] += 1
        room["webMapIds"].append(web_map_id)

    return [room_index[key] for key in sorted(room_index.keys())]


def build_connection_details() -> list[dict]:
    details: list[dict] = []

    for player_id in sorted(state.connections.keys()):
        room_code = state.get_player_room(player_id)
        caps = state.connection_caps.get(player_id, {})
        player_node = state.players.get(player_id, {})
        player_data = player_node.get("data", {}) if isinstance(player_node, dict) else {}
        if not isinstance(player_data, dict):
            player_data = {}
        player_name = str(player_data.get("playerName") or "").strip()
        display_name = player_name or player_id
        details.append(
            {
                "channel": "player",
                "actorId": player_id,
                "displayName": display_name,
                "roomCode": room_code,
                "protocolVersion": caps.get("protocol"),
                "programVersion": caps.get("programVersion"),
                "remoteAddr": caps.get("remoteAddr"),
            }
        )

    for web_map_id in sorted(state.web_map_connections.keys()):
        room_code = state.get_web_map_room(web_map_id)
        meta = web_map_connection_meta.get(web_map_id, {})
        program_version = meta.get("programVersion")
        display_name = str(meta.get("displayName") or program_version or "Web Map").strip() or "Web Map"
        details.append(
            {
                "channel": "web_map",
                "actorId": web_map_id,
                "displayName": display_name,
                "roomCode": room_code,
                "protocolVersion": meta.get("protocolVersion"),
                "programVersion": program_version,
                "remoteAddr": meta.get("remoteAddr"),
            }
        )

    return details


async def build_admin_overview_payload() -> dict:
    if admin_store is None:
        raise RuntimeError("admin_store_unavailable")

    rooms = build_room_overview()
    connection_details = build_connection_details()
    hourly_metrics = await admin_store.query_hourly_metrics(hours=24)
    hourly_peak = max((item["activePlayers"] for item in hourly_metrics["items"]), default=0)
    return {
        "playerConnections": len(state.connections),
        "webMapConnections": len(state.web_map_connections),
        "activeRooms": len(rooms),
        "rooms": rooms,
        "connectionDetails": connection_details,
        "timezone": admin_store.timezone_label,
        "dbPathMasked": admin_store.masked_db_path,
        "broadcastHz": state.broadcast_hz,
        "hourlyPeak24h": hourly_peak,
    }


async def build_admin_daily_metrics_payload(days: int = 30, room_code: str | None = None) -> dict:
    if admin_store is None:
        raise RuntimeError("admin_store_unavailable")
    return await admin_store.query_daily_metrics(days=days, room_code=room_code)


async def build_admin_hourly_metrics_payload(hours: int = 48, room_code: str | None = None) -> dict:
    if admin_store is None:
        raise RuntimeError("admin_store_unavailable")
    return await admin_store.query_hourly_metrics(hours=hours, room_code=room_code)


async def build_admin_audit_payload(
    *,
    limit: int = 100,
    before_id: int | None = None,
    event_type: str | None = None,
    actor_type: str | None = None,
    actor_types: list[str] | tuple[str, ...] | None = None,
    success: bool | None = None,
) -> dict:
    if admin_store is None:
        raise RuntimeError("admin_store_unavailable")
    return await admin_store.query_audit_events(
        limit=limit,
        before_id=before_id,
        event_type=event_type,
        actor_type=actor_type,
        actor_types=actor_types,
        success=success,
    )


async def build_admin_bootstrap_payload(
    *,
    audit_limit: int = 100,
    audit_event_type: str | None = None,
    audit_actor_types: tuple[str, ...] = (),
    audit_success: bool | None = None,
) -> dict:
    overview, daily_metrics, hourly_metrics, audit = await asyncio.gather(
        build_admin_overview_payload(),
        build_admin_daily_metrics_payload(days=30),
        build_admin_hourly_metrics_payload(hours=48),
        build_admin_audit_payload(
            limit=audit_limit,
            event_type=audit_event_type,
            actor_types=audit_actor_types,
            success=audit_success,
        ),
    )
    return {
        "serverTime": time.time(),
        "overview": overview,
        "dailyMetrics": daily_metrics,
        "hourlyMetrics": hourly_metrics,
        "audit": audit,
    }


def format_sse_event(event_name: str, payload: dict) -> str:
    return f"event: {event_name}\ndata: {json.dumps(payload, ensure_ascii=False)}\n\n"


def trigger_admin_sse_overview() -> None:
    asyncio.create_task(admin_sse_hub.broadcast("overview"))


def trigger_admin_sse_metrics() -> None:
    admin_sse_hub.schedule_broadcast("daily_metrics", delay_sec=1.0)
    admin_sse_hub.schedule_broadcast("hourly_metrics", delay_sec=1.0)


def trigger_admin_sse_audit() -> None:
    admin_sse_hub.schedule_broadcast("audit", delay_sec=1.0)

async def send_packet(websocket: WebSocket, packet, *, channel: str | None = None) -> None:
    if channel:
        if isinstance(packet, dict):
            body = dict(packet)
        else:
            body = packet.model_dump(exclude_none=True)
        body["channel"] = channel
        await websocket.send_bytes(message_codec.encode(body))
        return
    await websocket.send_bytes(message_codec.encode(packet))


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
        return message_codec.decode(payload)
    except PacketDecodeError as proto_error:
        if not allow_legacy_handshake:
            raise proto_error

        legacy_handshake = _decode_legacy_messagepack_handshake(payload)
        if legacy_handshake is None:
            raise proto_error

        logger.warning(
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

    if not HandshakeHelpers.protocol_at_least(client_protocol, SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION):
        return (
            "client_protocol_too_old: "
            f"client={client_protocol}, required>={SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION}"
        )

    if not HandshakeHelpers.protocol_at_least(NETWORK_PROTOCOL_VERSION, client_min_compatible):
        return (
            "server_protocol_too_old: "
            f"server={NETWORK_PROTOCOL_VERSION}, clientRequires>={client_min_compatible}"
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
        networkProtocolVersion=NETWORK_PROTOCOL_VERSION,
        minimumCompatibleNetworkProtocolVersion=SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION,
        localProgramVersion=SERVER_PROGRAM_VERSION,
        roomCode=room_code,
        deltaEnabled=True,
        error="version_incompatible",
        rejectReason=reason,
        broadcastHz=state.broadcast_hz,
        playerTimeoutSec=state.PLAYER_TIMEOUT,
        entityTimeoutSec=state.ENTITY_TIMEOUT,
        battleChunkTimeoutSec=state.BATTLE_CHUNK_TIMEOUT,
    )
    
    if legacy_messagepack_ack:
        await send_legacy_messagepack_packet(
            websocket,
            {
                "type": "handshake_ack",
                "ready": False,
                "networkProtocolVersion": NETWORK_PROTOCOL_VERSION,
                "minimumCompatibleNetworkProtocolVersion": SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION,
                "localProgramVersion": SERVER_PROGRAM_VERSION,
                "roomCode": room_code,
                "deltaEnabled": True,
                "error": "version_incompatible",
                "rejectReason": reason,
                "broadcastHz": state.broadcast_hz,
                "playerTimeoutSec": state.PLAYER_TIMEOUT,
                "entityTimeoutSec": state.ENTITY_TIMEOUT,
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

# 进程级单例：承载内存态与广播能力。
state = ServerState()
broadcaster = Broadcaster(state)
broadcast_task: asyncio.Task | None = None


async def run_broadcast_scheduler() -> None:
    previous_hz: float | None = None
    while True:
        tick_start = time.time()
        try:
            current_hz = state.update_broadcast_hz_for_congestion()
            if previous_hz is None or abs(current_hz - previous_hz) > 1e-6:
                await broadcaster.broadcast_report_rate_hints(
                    reason="startup" if previous_hz is None else "congestion"
                )
                previous_hz = current_hz

            await broadcaster.broadcast_updates()
        except asyncio.CancelledError:
            raise
        except Exception as e:
            logger.exception("Broadcast scheduler error: %s", e)
            await record_audit_event(
                event_type="backend_error",
                actor_type="system",
                success=False,
                detail={
                    "scope": "broadcast_scheduler",
                    "errorType": type(e).__name__,
                    "message": str(e),
                },
            )

        interval_sec = 1.0 / max(state.MIN_BROADCAST_HZ, state.broadcast_hz)
        elapsed = time.time() - tick_start
        await asyncio.sleep(max(0.0, interval_sec - elapsed))


async def run_admin_retention_scheduler() -> None:
    while True:
        try:
            if admin_store is not None:
                cleanup = await admin_store.cleanup_retention()
                logger.info("Admin retention cleanup completed: %s", cleanup)
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            logger.exception("Admin retention cleanup error: %s", exc)
            await record_audit_event(
                event_type="backend_error",
                actor_type="system",
                success=False,
                detail={
                    "scope": "admin_retention_scheduler",
                    "errorType": type(exc).__name__,
                    "message": str(exc),
                },
            )
        await asyncio.sleep(6 * 60 * 60)


# HTTP/WS 入口层：仅做协议收发与调度，不承载核心仲裁逻辑。
@asynccontextmanager
async def lifespan(_app: FastAPI):
    global admin_retention_task, admin_store, broadcast_task
    admin_store = AdminStore(build_admin_store_config())
    await admin_store.initialize()
    cleanup = await admin_store.cleanup_retention()
    logger.info(
        "Admin store initialized db=%s timezone=%s cleanup=%s",
        admin_store.masked_db_path,
        admin_store.timezone_label,
        cleanup,
    )
    if broadcast_task is None or broadcast_task.done():
        broadcast_task = asyncio.create_task(run_broadcast_scheduler())
    if admin_retention_task is None or admin_retention_task.done():
        admin_retention_task = asyncio.create_task(run_admin_retention_scheduler())
    try:
        yield
    finally:
        await admin_sse_hub.close()
        if admin_retention_task is not None:
            admin_retention_task.cancel()
            try:
                await admin_retention_task
            except asyncio.CancelledError:
                pass
            admin_retention_task = None
        if broadcast_task is not None:
            broadcast_task.cancel()
            try:
                await broadcast_task
            except asyncio.CancelledError:
                pass
            broadcast_task = None
        if admin_store is not None:
            await admin_store.close()
            admin_store = None


app = FastAPI(lifespan=lifespan)

@app.websocket("/web-map/ws")
@app.websocket("/adminws")
async def web_map_ws(websocket: WebSocket):
    """网页地图订阅通道：用于查看服务端实时快照与发送观察端指令。"""
    await websocket.accept()
    web_map_id = str(id(websocket))
    remote_addr = get_websocket_remote_addr(websocket)
    if str(websocket.url.path) == "/adminws":
        logger.warning("Deprecated websocket route /adminws used; migrate clients to /web-map/ws")
    handshake_completed = False
    web_map_room = state.DEFAULT_ROOM_CODE
    disconnect_reason = "connection_closed"
    disconnect_code = None
    disconnect_exception = None
    try:
        while True:
            try:
                payload = await receive_payload(websocket, allow_legacy_handshake=not handshake_completed)
                require_wire_channel(payload, "web_map", "/web-map/ws")
            except PacketDecodeError as exc:
                if not handshake_completed and exc.code == "channel_mismatch":
                    await record_audit_event(
                        event_type="web_map_handshake_failed",
                        actor_type="web_map",
                        actor_id=web_map_id,
                        room_code=state.DEFAULT_ROOM_CODE,
                        success=False,
                        remote_addr=remote_addr,
                        detail={"reason": exc.detail},
                    )
                    await reject_handshake(websocket, exc.detail, state.DEFAULT_ROOM_CODE, channel="web_map")
                    return
                await send_packet(websocket, WebMapAckPacket(ok=False, error=exc.code))
                continue

            try:
                packet = PacketParsers.parse_web_map(payload)
            except PacketDecodeError:
                msg_type = str(payload.get("type") or "").strip()
                await send_packet(
                    websocket,
                    WebMapAckPacket(ok=False, error="unsupported_command", command=msg_type or None),
                )
                continue

            if isinstance(packet, HandshakePacket):
                if payload.get("_legacy_msgpack"):
                    legacy_room = state.set_web_map_room(web_map_id, HandshakeHelpers.room_code(packet, state.DEFAULT_ROOM_CODE))
                    legacy_reason = LEGACY_PROTOCOL_REJECTION_REASON
                    logger.warning(
                        "Rejecting legacy MessagePack web-map client (clientProtocol=%s, roomCode=%s)",
                        HandshakeHelpers.protocol_version(packet),
                        legacy_room,
                    )
                    await record_audit_event(
                        event_type="web_map_handshake_failed",
                        actor_type="web_map",
                        actor_id=web_map_id,
                        room_code=legacy_room,
                        success=False,
                        remote_addr=remote_addr,
                        detail={
                            "reason": "legacy_messagepack",
                            "clientProtocol": HandshakeHelpers.protocol_version(packet),
                        },
                    )
                    await reject_handshake(
                        websocket,
                        legacy_reason,
                        legacy_room,
                        channel="web_map",
                        send_ack=False,
                        legacy_messagepack_ack=True,
                    )
                    return
                
                client_protocol = HandshakeHelpers.protocol_version(packet)
                client_program_version = HandshakeHelpers.program_version(packet)
                web_map_room = state.normalize_room_code(HandshakeHelpers.room_code(packet, state.DEFAULT_ROOM_CODE))
                rejection_reason = resolve_handshake_rejection_reason(packet)
                if rejection_reason:
                    logger.warning(
                        "Web-map handshake rejected (clientProtocol=%s, roomCode=%s, reason=%s)",
                        client_protocol,
                        web_map_room,
                        rejection_reason,
                    )
                    await record_audit_event(
                        event_type="web_map_handshake_failed",
                        actor_type="web_map",
                        actor_id=web_map_id,
                        room_code=web_map_room,
                        success=False,
                        remote_addr=remote_addr,
                        detail={
                            "reason": rejection_reason,
                            "clientProtocol": client_protocol,
                            "clientProgramVersion": client_program_version,
                        },
                    )
                    await reject_handshake(websocket, rejection_reason, web_map_room, channel="web_map")
                    return

                await send_packet(
                    websocket,
                    HandshakeAckPacket(
                        networkProtocolVersion=NETWORK_PROTOCOL_VERSION,
                        minimumCompatibleNetworkProtocolVersion=SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION,
                        localProgramVersion=SERVER_PROGRAM_VERSION,
                        roomCode=web_map_room,
                        deltaEnabled=True,
                        broadcastHz=state.broadcast_hz,
                        playerTimeoutSec=state.PLAYER_TIMEOUT,
                        entityTimeoutSec=state.ENTITY_TIMEOUT,
                        battleChunkTimeoutSec=state.BATTLE_CHUNK_TIMEOUT,
                    ),
                    channel="web_map",
                )
                state.web_map_connections[web_map_id] = websocket
                state.set_web_map_room(web_map_id, web_map_room)
                web_map_connection_meta[web_map_id] = {
                    "protocolVersion": client_protocol,
                    "programVersion": client_program_version,
                    "displayName": "Web Map",
                    "remoteAddr": remote_addr,
                }
                handshake_completed = True
                logger.info(
                    "Web-map connected (clientProtocol=%s, clientProgramVersion=%s, roomCode=%s)",
                    client_protocol,
                    client_program_version,
                    web_map_room,
                )
                trigger_admin_sse_overview()
                await record_audit_event(
                    event_type="web_map_handshake_success",
                    actor_type="web_map",
                    actor_id=web_map_id,
                    room_code=web_map_room,
                    success=True,
                    remote_addr=remote_addr,
                    detail={
                        "clientProtocol": client_protocol,
                        "clientProgramVersion": client_program_version,
                    },
                )
                try:
                    await broadcaster.send_web_map_snapshot_full(web_map_id)
                except Exception as exc:
                    logger.warning(
                        "Initial web-map snapshot send failed (webMapId=%s, roomCode=%s, %s): %s: %r",
                        web_map_id,
                        web_map_room,
                        describe_websocket(websocket),
                        type(exc).__name__,
                        exc,
                    )
                    raise
                continue

            if not handshake_completed:
                await send_packet(websocket, WebMapAckPacket(ok=False, error="handshake_required"))
                continue

            if isinstance(packet, PingPacket):
                await send_packet(websocket, PongPacket(serverTime=time.time()), channel="web_map")
                continue

            if isinstance(packet, ResyncRequestPacket):
                await broadcaster.send_web_map_snapshot_full(web_map_id)
                continue

            if isinstance(packet, BattleChunkMetaRequestPacket):
                requested_chunk_ids = [
                    chunk_id
                    for chunk_id in packet.battleChunks[:256]
                    if isinstance(chunk_id, str) and chunk_id
                ]
                await send_packet(
                    websocket,
                    BattleChunkMetaSnapshotPacket(
                        battleChunks=state.select_battle_chunk_meta_snapshot(web_map_room, requested_chunk_ids),
                    ),
                    channel="web_map",
                )
                continue

            if isinstance(packet, CommandPlayerMarkSetPacket):
                target_player_id = packet.playerId
                updated_mark = state.set_player_mark(
                    target_player_id,
                    packet.team,
                    packet.color,
                    packet.label,
                    packet.source,
                )

                if updated_mark is None:
                    await send_packet(websocket, WebMapAckPacket(ok=False, error="invalid_player_id"))
                    continue

                await send_packet(
                    websocket,
                    WebMapAckPacket(
                        ok=True,
                        action="command_player_mark_set",
                        playerId=str(target_player_id).strip() if isinstance(target_player_id, str) else None,
                        mark=updated_mark,
                    ),
                )
                continue

            if isinstance(packet, CommandPlayerMarkClearPacket):
                target_player_id = packet.playerId
                removed = state.clear_player_mark(target_player_id)
                await send_packet(
                    websocket,
                    WebMapAckPacket(
                        ok=bool(removed),
                        action="command_player_mark_clear",
                        playerId=target_player_id,
                        error=None if removed else "mark_not_found",
                    ),
                )
                if removed:
                    await broadcaster.broadcast_web_map_updates()
                continue

            if isinstance(packet, CommandPlayerMarkClearAllPacket):
                removed_count = state.clear_all_player_marks()
                await send_packet(
                    websocket,
                    WebMapAckPacket(
                        ok=True,
                        action="command_player_mark_clear_all",
                        removedCount=removed_count,
                    ),
                )
                continue

            if isinstance(packet, CommandSameServerFilterSetPacket):
                state.same_server_filter_enabled = bool(packet.enabled)
                await send_packet(
                    websocket,
                    WebMapAckPacket(
                        ok=True,
                        action="command_same_server_filter_set",
                        enabled=state.same_server_filter_enabled,
                    ),
                )
                continue

            if isinstance(packet, CommandTacticalWaypointSetPacket):
                room_code = state.normalize_room_code(packet.roomCode or state.get_web_map_room(web_map_id))
                waypoint_id_raw = packet.waypointId
                waypoint_id = str(waypoint_id_raw).strip() if isinstance(waypoint_id_raw, str) and waypoint_id_raw.strip() else ""
                if not waypoint_id:
                    waypoint_id = f"web_map_tactical:{int(time.time() * 1000)}:{uuid.uuid4().hex[:8]}"

                x = packet.x
                z = packet.z
                label = str(packet.label or "战术标记").strip()
                if not label:
                    label = "战术标记"
                if len(label) > 64:
                    label = label[:64]

                dimension = str(packet.dimension or "minecraft:overworld").strip() or "minecraft:overworld"
                tactical_type = str(packet.tacticalType or "attack").strip() or "attack"
                permanent = bool(packet.permanent)
                ttl_seconds_raw = packet.ttlSeconds
                ttl_seconds = None
                if isinstance(ttl_seconds_raw, (int, float)):
                    ttl_seconds = max(10, min(int(ttl_seconds_raw), 86400))
                if permanent:
                    ttl_seconds = None

                waypoint_payload = {
                    "x": x,
                    "y": 64,
                    "z": z,
                    "dimension": dimension,
                    "name": label,
                    "symbol": "T",
                    "color": normalize_waypoint_color_to_int(packet.color, 0xEF4444),
                    "ownerId": None,
                    "ownerName": "Web Map Tactical",
                    "createdAt": int(time.time() * 1000),
                    "ttlSeconds": ttl_seconds,
                    "waypointKind": "web_map_tactical",
                    "replaceOldQuick": False,
                    "maxQuickMarks": None,
                    "targetType": "block",
                    "targetEntityId": None,
                    "targetEntityType": None,
                    "targetEntityName": None,
                    "roomCode": room_code,
                    "permanent": permanent,
                    "tacticalType": tactical_type,
                    "sourceType": "web_map_tactical",
                    "deletableBy": "owner",
                }

                try:
                    validated = WaypointData(**waypoint_payload)
                except ValidationError:
                    await send_packet(websocket, WebMapAckPacket(ok=False, error="invalid_tactical_waypoint_payload"))
                    continue

                web_map_source_id = state.build_web_map_tactical_source_id(room_code)
                node = state.build_state_node(web_map_source_id, time.time(), validated.model_dump())
                state.upsert_report(state.waypoint_reports, waypoint_id, web_map_source_id, node)

                await send_packet(
                    websocket,
                    WebMapAckPacket(
                        ok=True,
                        action="command_tactical_waypoint_set",
                        waypointId=waypoint_id,
                        waypoint=validated.model_dump(),
                    ),
                )
                continue

            if isinstance(packet, WaypointsDeletePacket):
                room_code = state.normalize_room_code(state.get_web_map_room(web_map_id))
                web_map_source_id = state.build_web_map_tactical_source_id(room_code)
                removed_ids: list[str] = []

                for waypoint_id in packet.waypointIds:
                    if not isinstance(waypoint_id, str):
                        continue
                    waypoint_id = waypoint_id.strip()
                    if not waypoint_id:
                        continue

                    source_bucket = state.waypoint_reports.get(waypoint_id)
                    if not isinstance(source_bucket, dict) or not source_bucket:
                        continue

                    removed_for_waypoint = False
                    for source_id, node in list(source_bucket.items()):
                        if not isinstance(node, dict):
                            continue

                        waypoint_data = node.get("data", {})
                        if not isinstance(waypoint_data, dict):
                            continue

                        deletable_by = str(waypoint_data.get("deletableBy", "everyone") or "everyone").strip().lower()
                        if deletable_by == "everyone":
                            removed_for_waypoint = state.delete_report(state.waypoint_reports, waypoint_id, source_id) or removed_for_waypoint
                            continue

                        if deletable_by == "owner" and source_id == web_map_source_id:
                            removed_for_waypoint = state.delete_report(state.waypoint_reports, waypoint_id, source_id) or removed_for_waypoint

                    if removed_for_waypoint:
                        removed_ids.append(waypoint_id)

                await send_packet(
                    websocket,
                    WebMapAckPacket(
                        ok=bool(removed_ids),
                        action="waypoints_delete",
                        waypointIds=removed_ids,
                        error=None if removed_ids else "waypoint_not_found",
                    ),
                )
                continue

            await send_packet(websocket, WebMapAckPacket(ok=False, error="unsupported_command"))
    except WebSocketDisconnect as exc:
        disconnect_reason = "client_disconnect"
        disconnect_code = getattr(exc, "code", None)
        disconnect_exception = exc
    except Exception as e:
        disconnect_reason = f"error:{type(e).__name__}"
        disconnect_exception = e
        logger.exception("Web-map websocket error: %s", e)
        await record_audit_event(
            event_type="backend_error",
            actor_type="web_map",
            actor_id=web_map_id,
            room_code=web_map_room,
            success=False,
            remote_addr=remote_addr,
            detail={
                "scope": "web_map_ws",
                "errorType": type(e).__name__,
                "message": str(e),
            },
        )
    finally:
        logger.info(
            "Web-map disconnected (webMapId=%s, roomCode=%s, handshakeCompleted=%s, reason=%s, code=%s, %s, error=%r)",
            web_map_id,
            web_map_room,
            handshake_completed,
            disconnect_reason,
            disconnect_code,
            describe_websocket(websocket),
            disconnect_exception,
        )
        if handshake_completed:
            trigger_admin_sse_overview()
            await record_audit_event(
                event_type="web_map_disconnected",
                actor_type="web_map",
                actor_id=web_map_id,
                room_code=web_map_room,
                success=True,
                remote_addr=remote_addr,
                detail={"reason": disconnect_reason, "code": disconnect_code},
            )
        if web_map_id in state.web_map_connections:
            del state.web_map_connections[web_map_id]
        if web_map_id in state.web_map_connection_rooms:
            del state.web_map_connection_rooms[web_map_id]
        web_map_connection_meta.pop(web_map_id, None)


@app.websocket("/admin/ws")
async def reserved_admin_ws(websocket: WebSocket):
    """预留给未来真正后台管理能力的接口。"""
    await websocket.accept()
    try:
        payload = await receive_payload(websocket, allow_legacy_handshake=False)
        require_wire_channel(payload, "admin", "/admin/ws")
        packet = PacketParsers.parse_admin(payload)
        if isinstance(packet, HandshakePacket):
            await reject_handshake(
                websocket,
                "admin_interface_reserved: /admin/ws is reserved for a future management interface",
                HandshakeHelpers.room_code(packet, state.DEFAULT_ROOM_CODE),
                channel="admin",
            )
            return
        await websocket.close(code=1008, reason="admin_interface_reserved")
    except PacketDecodeError:
        await websocket.close(code=1008, reason="admin_interface_reserved")


@app.get("/admin")
async def admin_page(request: Request):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result

    await record_admin_access(request, auth_result, "admin_page_view")
    if not admin_ui_ready():
        return admin_ui_unavailable_response()
    return FileResponse(ADMIN_UI_INDEX_PATH, media_type="text/html")


@app.get("/admin/assets/{asset_path:path}")
async def admin_assets(request: Request, asset_path: str):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result

    asset_file = resolve_admin_asset_path(asset_path)
    if asset_file is None:
        if not admin_ui_ready():
            return admin_ui_unavailable_response()
        return JSONResponse({"detail": "not_found"}, status_code=404)

    return FileResponse(Path(asset_file))


@app.get("/admin/api/overview")
async def admin_overview(request: Request):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    if admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    await record_admin_access(request, auth_result, "admin_api_access")
    return JSONResponse(await build_admin_overview_payload())


@app.get("/admin/api/events")
async def admin_events(
    request: Request,
    auditLimit: int = Query(default=100, ge=1, le=500),
    auditEventType: str | None = None,
    auditActorType: str | None = None,
    auditActorTypes: list[str] | None = Query(default=None),
    auditSuccess: bool | None = None,
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    if admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    await record_admin_access(request, auth_result, "admin_sse_connect")
    audit_event_type = auditEventType.strip() if isinstance(auditEventType, str) and auditEventType.strip() else None
    audit_actor_type = auditActorType.strip() if isinstance(auditActorType, str) and auditActorType.strip() else None
    audit_actor_types = tuple(
        item.strip()
        for item in (auditActorTypes or [])
        if isinstance(item, str) and item.strip()
    )
    if not audit_actor_types and audit_actor_type:
        audit_actor_types = (audit_actor_type,)
    subscriber = await admin_sse_hub.subscribe(
        audit_limit=auditLimit,
        audit_event_type=audit_event_type,
        audit_actor_types=audit_actor_types,
        audit_success=auditSuccess,
    )

    async def event_stream():
        try:
            bootstrap_payload = await build_admin_bootstrap_payload(
                audit_limit=subscriber.audit_limit,
                audit_event_type=subscriber.audit_event_type,
                audit_actor_types=subscriber.audit_actor_types,
                audit_success=subscriber.audit_success,
            )
            yield format_sse_event("bootstrap", bootstrap_payload)

            while True:
                if await request.is_disconnected():
                    break

                try:
                    event_name = await asyncio.wait_for(subscriber.queue.get(), timeout=15.0)
                except asyncio.TimeoutError:
                    yield format_sse_event("heartbeat", {"serverTime": time.time()})
                    continue

                payload: dict
                if event_name == "overview":
                    payload = {"serverTime": time.time(), **(await build_admin_overview_payload())}
                elif event_name == "daily_metrics":
                    payload = {"serverTime": time.time(), **(await build_admin_daily_metrics_payload(days=30))}
                elif event_name == "hourly_metrics":
                    payload = {"serverTime": time.time(), **(await build_admin_hourly_metrics_payload(hours=48))}
                elif event_name == "audit":
                    payload = {
                        "serverTime": time.time(),
                        **(
                            await build_admin_audit_payload(
                                limit=subscriber.audit_limit,
                                event_type=subscriber.audit_event_type,
                                actor_types=subscriber.audit_actor_types,
                                success=subscriber.audit_success,
                            )
                        ),
                    }
                else:
                    continue

                yield format_sse_event(event_name, payload)
        finally:
            await admin_sse_hub.unsubscribe(subscriber)

    return StreamingResponse(
        event_stream(),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )


@app.get("/admin/api/metrics/daily")
async def admin_daily_metrics(
    request: Request,
    days: int = Query(default=30, ge=1, le=400),
    roomCode: str | None = None,
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    if admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    await record_admin_access(request, auth_result, "admin_api_access")
    payload = await build_admin_daily_metrics_payload(days=days, room_code=normalize_optional_room_code(roomCode))
    return JSONResponse(payload)


@app.get("/admin/api/metrics/hourly")
async def admin_hourly_metrics(
    request: Request,
    hours: int = Query(default=48, ge=1, le=240),
    roomCode: str | None = None,
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    if admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    await record_admin_access(request, auth_result, "admin_api_access")
    payload = await build_admin_hourly_metrics_payload(hours=hours, room_code=normalize_optional_room_code(roomCode))
    return JSONResponse(payload)


@app.get("/admin/api/audit")
async def admin_audit_log(
    request: Request,
    limit: int = Query(default=100, ge=1, le=500),
    beforeId: int | None = Query(default=None, ge=1),
    eventType: str | None = None,
    actorType: str | None = None,
    actorTypes: list[str] | None = Query(default=None),
    success: bool | None = None,
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    if admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    await record_admin_access(request, auth_result, "admin_api_access")
    normalized_actor_types = [
        item.strip()
        for item in (actorTypes or [])
        if isinstance(item, str) and item.strip()
    ]
    if not normalized_actor_types and isinstance(actorType, str) and actorType.strip():
        normalized_actor_types = [actorType.strip()]
    payload = await build_admin_audit_payload(
        limit=limit,
        before_id=beforeId,
        event_type=eventType.strip() if isinstance(eventType, str) and eventType.strip() else None,
        actor_type=actorType.strip() if isinstance(actorType, str) and actorType.strip() else None,
        actor_types=normalized_actor_types,
        success=success,
    )
    return JSONResponse(payload)


@app.websocket("/playeresp")
@app.websocket("/mc-client")
async def websocket_endpoint(websocket: WebSocket):
    """
    玩家数据主通道。

    职责：
    1) 接收客户端上报（全量/增量）；
    2) 做入参校验并写入来源报告池；
    3) 触发统一广播（由 Broadcaster 完成聚合后下发）。
    """
    await websocket.accept()
    submit_player_id = None
    remote_addr = get_websocket_remote_addr(websocket)
    disconnect_reason = "connection_closed"
    disconnect_code = None
    disconnect_exception = None

    try:
        while True:
            payload = None
            try:
                payload = await receive_payload(websocket, allow_legacy_handshake=submit_player_id is None)
                require_wire_channel(payload, "player", "/mc-client")
                packet = PacketParsers.parse_player(payload)
            except PacketDecodeError as e:
                if submit_player_id is None and e.code == "channel_mismatch":
                    await record_audit_event(
                        event_type="player_handshake_failed",
                        actor_type="player",
                        room_code=state.DEFAULT_ROOM_CODE,
                        success=False,
                        remote_addr=remote_addr,
                        detail={"reason": e.detail},
                    )
                    await reject_handshake(
                        websocket,
                        e.detail,
                        state.DEFAULT_ROOM_CODE,
                        channel="player",
                    )
                    return
                payload_type = None
                if isinstance(payload, dict):
                    raw_type = payload.get("type")
                    if isinstance(raw_type, str) and raw_type.strip():
                        payload_type = raw_type.strip()

                logger.warning(
                    "Error decoding player packet type=%s detail=%s",
                    payload_type or "unknown",
                    e.detail,
                )
                continue

            packet_submit_id = getattr(packet, "submitPlayerId", None)
            if isinstance(packet_submit_id, str) and packet_submit_id:
                submit_player_id = packet_submit_id

            # 检查是否是旧版本 MessagePack 客户端
            if payload.get("_legacy_msgpack"):
                # 对于旧版本客户端，直接拒绝握手并提示升级
                legacy_room = HandshakeHelpers.room_code(packet, state.DEFAULT_ROOM_CODE)
                legacy_reason = LEGACY_PROTOCOL_REJECTION_REASON
                logger.warning(
                    "Rejecting legacy MessagePack client (submitPlayerId=%s, roomCode=%s)",
                    submit_player_id,
                    legacy_room,
                )
                await record_audit_event(
                    event_type="player_handshake_failed",
                    actor_type="player",
                    actor_id=submit_player_id,
                    room_code=legacy_room,
                    success=False,
                    remote_addr=remote_addr,
                    detail={"reason": "legacy_messagepack"},
                )
                await reject_handshake(
                    websocket,
                    legacy_reason,
                    legacy_room,
                    channel="player",
                    send_ack=False,
                    legacy_messagepack_ack=True,
                )
                return
            
            # 握手：建立能力协商（协议版本、是否支持 delta）。
            if isinstance(packet, HandshakePacket):
                rejection_reason = resolve_handshake_rejection_reason(packet)
                if rejection_reason:
                    logger.warning(
                        "Player handshake rejected (submitPlayerId=%s, reason=%s)",
                        submit_player_id,
                        rejection_reason,
                    )
                    await record_audit_event(
                        event_type="player_handshake_failed",
                        actor_type="player",
                        actor_id=submit_player_id,
                        room_code=HandshakeHelpers.room_code(packet, state.DEFAULT_ROOM_CODE),
                        success=False,
                        remote_addr=remote_addr,
                        detail={"reason": rejection_reason},
                    )
                    await reject_handshake(
                        websocket,
                        rejection_reason,
                        HandshakeHelpers.room_code(packet, state.DEFAULT_ROOM_CODE),
                        channel="player",
                    )
                    return

                if submit_player_id:
                    client_protocol = HandshakeHelpers.protocol_version(packet)
                    client_program_version = HandshakeHelpers.program_version(packet)
                    client_room = state.normalize_room_code(HandshakeHelpers.room_code(packet, state.DEFAULT_ROOM_CODE))
                    state.mark_player_capability(
                        submit_player_id,
                        client_protocol,
                        packet.preferredReportIntervalTicks,
                        packet.minReportIntervalTicks,
                        packet.maxReportIntervalTicks,
                    )
                    negotiated_ticks = state.negotiate_report_interval_ticks(
                        submit_player_id,
                        packet.preferredReportIntervalTicks,
                        packet.minReportIntervalTicks,
                        packet.maxReportIntervalTicks,
                    )
                    caps = state.connection_caps.get(submit_player_id)
                    if isinstance(caps, dict):
                        caps["negotiatedReportIntervalTicks"] = negotiated_ticks
                        caps["programVersion"] = client_program_version
                        caps["remoteAddr"] = remote_addr

                    ack = {
                        "networkProtocolVersion": NETWORK_PROTOCOL_VERSION,
                        "minimumCompatibleNetworkProtocolVersion": SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION,
                        "localProgramVersion": SERVER_PROGRAM_VERSION,
                        "roomCode": client_room,
                        "deltaEnabled": True,
                        "digestIntervalSec": state.DIGEST_INTERVAL_SEC,
                        "broadcastHz": state.broadcast_hz,
                        "reportIntervalTicks": negotiated_ticks,
                        "playerTimeoutSec": state.PLAYER_TIMEOUT,
                        "entityTimeoutSec": state.ENTITY_TIMEOUT,
                        "battleChunkTimeoutSec": state.BATTLE_CHUNK_TIMEOUT,
                    }
                    await send_packet(websocket, HandshakeAckPacket(**ack))
                    state.connections[submit_player_id] = websocket
                    state.set_player_room(submit_player_id, client_room)
                    logger.info(
                        "Client %s connected (protocol=%s, programVersion=%s, roomCode=%s)",
                        submit_player_id,
                        client_protocol,
                        client_program_version,
                        client_room,
                    )
                    trigger_admin_sse_overview()
                    await record_player_activity(submit_player_id, client_room)
                    await record_audit_event(
                        event_type="player_handshake_success",
                        actor_type="player",
                        actor_id=submit_player_id,
                        room_code=client_room,
                        success=True,
                        remote_addr=remote_addr,
                        detail={
                            "clientProtocol": client_protocol,
                            "clientProgramVersion": client_program_version,
                        },
                    )
                    await broadcaster.send_snapshot_full_to_player(submit_player_id)
                continue

            if not submit_player_id or submit_player_id not in state.connections:
                logger.debug("Ignore player packet before handshake registration submitPlayerId=%s", submit_player_id)
                continue

            await record_player_activity(
                submit_player_id,
                state.get_player_room(submit_player_id),
            )

            for packet in expand_player_packets(packet):
                if (
                    packet.type not in {"tab_players_update", "tab_players_patch"}
                    and not isinstance(packet, SourceStateClearPacket)
                ):
                    state.touch_tab_player_report(submit_player_id, time.time())

                if packet.type == "state_keepalive":
                    current_time = time.time()
                    touched_players = state.touch_reports(
                        state.player_reports,
                        packet.players,
                        submit_player_id,
                        current_time,
                    )
                    touched_entities = state.touch_reports(
                        state.entity_reports,
                        packet.entities,
                        submit_player_id,
                        current_time,
                    )
                    touched_battle_chunks = state.touch_reports(
                        state.battle_chunk_reports,
                        packet.battleChunks,
                        submit_player_id,
                        current_time,
                    )
                    if touched_players or touched_entities or touched_battle_chunks:
                        logger.debug(
                            "Applied state_keepalive "
                            f"submitPlayerId={submit_player_id} players={touched_players}/{len(packet.players)} "
                            f"entities={touched_entities}/{len(packet.entities)} "
                            f"battleChunks={touched_battle_chunks}/{len(packet.battleChunks)}"
                        )
                    continue

                if isinstance(packet, SourceStateClearPacket):
                    state.clear_source_state(submit_player_id, packet.scopes)
                    await broadcaster.broadcast_web_map_updates()
                    logger.info(
                        "Cleared source state for submitPlayerId=%s scopes=%s",
                        submit_player_id,
                        packet.scopes or ["players", "entities", "tab_players", "waypoints"],
                    )
                    continue

                if packet.type == "players_update":
                    # 玩家全量：语义为“该来源本轮玩家状态完整快照”。
                    current_time = time.time()
                    for pid, player_data in packet.players.items():
                        try:
                            normalized = player_data.model_dump()
                            node = state.build_state_node(submit_player_id, current_time, normalized)
                            state.upsert_report(state.player_reports, pid, submit_player_id, node)
                        except Exception as e:
                            logger.warning("Error validating player data for %s: %s", pid, e)

                    continue

                if packet.type == "tab_players_update":
                    if isinstance(submit_player_id, str) and submit_player_id:
                        current_time = time.time()
                        tab_players = packet.tabPlayers
                        state.upsert_tab_player_report(submit_player_id, tab_players, current_time)
                        await broadcaster.broadcast_web_map_updates()
                    continue

                if packet.type == "tab_players_patch":
                    if isinstance(submit_player_id, str) and submit_player_id:
                        current_time = time.time()
                        state.patch_tab_player_report(
                            submit_player_id,
                            packet.upsert,
                            packet.delete,
                            current_time,
                        )
                        await broadcaster.broadcast_web_map_updates()
                    continue

                if packet.type == "players_patch":
                    # 玩家增量：基于该来源已有快照做 merge 后再校验。
                    current_time = time.time()
                    upsert = packet.upsert
                    delete = packet.delete
                    missing_baseline_players = []

                    for pid, player_data in upsert.items():
                        source_key = submit_player_id if isinstance(submit_player_id, str) else ""
                        existing_node = state.player_reports.get(pid, {}).get(source_key)
                        try:
                            normalized = state.merge_patch_and_validate(PlayerData, existing_node, player_data)
                            node = state.build_state_node(submit_player_id, current_time, normalized)
                            state.upsert_report(state.player_reports, pid, submit_player_id, node)
                        except ValidationError as e:
                            missing_fields = state.missing_fields_from_validation_error(e)
                            existing_data = existing_node.get("data") if isinstance(existing_node, dict) else None
                            existing_keys = sorted(existing_data.keys()) if isinstance(existing_data, dict) else []
                            if not isinstance(existing_data, dict):
                                missing_baseline_players.append(pid)
                            logger.warning(
                                "Player patch validation failed "
                                f"pid={pid} submitPlayerId={submit_player_id} sourceKey={source_key!r} "
                                f"hasExistingSnapshot={bool(isinstance(existing_data, dict))} "
                                f"missingFields={missing_fields or '[]'} "
                                f"existingKeys={existing_keys} payload={state.payload_preview(player_data)} "
                                f"errors={state.payload_preview(e.errors(), 480)}"
                            )
                        except Exception as e:
                            logger.exception(
                                "Unexpected error validating player patch "
                                f"pid={pid} submitPlayerId={submit_player_id} payload={state.payload_preview(player_data)}: {e}"
                            )

                    if isinstance(delete, list):
                        for pid in delete:
                            if not isinstance(pid, str):
                                continue
                            state.delete_report(state.player_reports, pid, submit_player_id)

                    if missing_baseline_players and isinstance(submit_player_id, str) and submit_player_id:
                        await broadcaster.send_refresh_request_to_source(
                            submit_player_id,
                            players=missing_baseline_players,
                            entities=[],
                            battle_chunks=[],
                            reason="missing_baseline_patch",
                            bypass_cooldown=False,
                        )

                    continue

                if packet.type == "entities_update":
                    # 实体全量：先清理该来源旧实体，再写入本轮实体列表。
                    current_time = time.time()
                    player_entities = packet.entities
                    source_key = submit_player_id if isinstance(submit_player_id, str) else ""
                    for entity_id in list(state.entity_reports.keys()):
                        source_bucket = state.entity_reports.get(entity_id, {})
                        if source_key in source_bucket:
                            state.delete_report(state.entity_reports, entity_id, submit_player_id)

                    for entity_id, entity_data in player_entities.items():
                        try:
                            normalized = entity_data.model_dump()
                            node = state.build_state_node(submit_player_id, current_time, normalized)
                            state.upsert_report(state.entity_reports, entity_id, submit_player_id, node)
                        except Exception as e:
                            logger.warning("Error validating entity data for %s: %s", entity_id, e)

                    continue

                if packet.type == "entities_patch":
                    # 实体增量：仅修改当前来源 bucket，不影响其他来源。
                    current_time = time.time()
                    upsert = packet.upsert
                    delete = packet.delete
                    missing_baseline_entities = []

                    for entity_id, entity_data in upsert.items():
                        source_key = submit_player_id if isinstance(submit_player_id, str) else ""
                        existing_node = state.entity_reports.get(entity_id, {}).get(source_key)
                        try:
                            normalized = state.merge_patch_and_validate(EntityData, existing_node, entity_data)
                            node = state.build_state_node(submit_player_id, current_time, normalized)
                            state.upsert_report(state.entity_reports, entity_id, submit_player_id, node)
                        except ValidationError as e:
                            missing_fields = state.missing_fields_from_validation_error(e)
                            existing_data = existing_node.get("data") if isinstance(existing_node, dict) else None
                            existing_keys = sorted(existing_data.keys()) if isinstance(existing_data, dict) else []
                            if not isinstance(existing_data, dict):
                                missing_baseline_entities.append(entity_id)
                            logger.warning(
                                "Entity patch validation failed "
                                f"entityId={entity_id} submitPlayerId={submit_player_id} sourceKey={source_key!r} "
                                f"hasExistingSnapshot={bool(isinstance(existing_data, dict))} "
                                f"missingFields={missing_fields or '[]'} "
                                f"existingKeys={existing_keys} payload={state.payload_preview(entity_data)} "
                                f"errors={state.payload_preview(e.errors(), 480)}"
                            )
                        except Exception as e:
                            logger.exception(
                                "Unexpected error validating entity patch "
                                f"entityId={entity_id} submitPlayerId={submit_player_id} "
                                f"payload={state.payload_preview(entity_data)}: {e}"
                            )

                    if isinstance(delete, list):
                        for entity_id in delete:
                            if not isinstance(entity_id, str):
                                continue
                            state.delete_report(state.entity_reports, entity_id, submit_player_id)

                    if missing_baseline_entities and isinstance(submit_player_id, str) and submit_player_id:
                        await broadcaster.send_refresh_request_to_source(
                            submit_player_id,
                            players=[],
                            entities=missing_baseline_entities,
                            battle_chunks=[],
                            reason="missing_baseline_patch",
                            bypass_cooldown=False,
                        )

                    continue

                if packet.type == "waypoints_patch":
                    current_time = time.time()
                    upsert = packet.upsert
                    delete = packet.delete

                    for waypoint_id, waypoint_data in upsert.items():
                        source_key = submit_player_id if isinstance(submit_player_id, str) else ""
                        existing_node = state.waypoint_reports.get(waypoint_id, {}).get(source_key)
                        try:
                            normalized = state.merge_patch_and_validate(WaypointData, existing_node, waypoint_data)
                            node = state.build_state_node(submit_player_id, current_time, normalized)
                            state.upsert_report(state.waypoint_reports, waypoint_id, submit_player_id, node)
                        except ValidationError as e:
                            existing_data = existing_node.get("data") if isinstance(existing_node, dict) else None
                            logger.warning(
                                "Waypoint patch validation failed "
                                f"waypointId={waypoint_id} submitPlayerId={submit_player_id} "
                                f"hasExistingSnapshot={bool(isinstance(existing_data, dict))} "
                                f"payload={state.payload_preview(waypoint_data)} "
                                f"errors={state.payload_preview(e.errors(), 480)}"
                            )
                        except Exception as e:
                            logger.exception(
                                "Unexpected error validating waypoint patch "
                                f"waypointId={waypoint_id} submitPlayerId={submit_player_id} "
                                f"payload={state.payload_preview(waypoint_data)}: {e}"
                            )

                    if isinstance(delete, list):
                        for waypoint_id in delete:
                            if not isinstance(waypoint_id, str):
                                continue
                            state.delete_report(state.waypoint_reports, waypoint_id, submit_player_id)

                    continue

                if packet.type == "waypoints_update":
                # 路标上报：支持 quick 类型数量约束。
                    current_time = time.time()
                    player_waypoints = packet.waypoints
                    for waypoint_id, waypoint_data in player_waypoints.items():
                        try:
                            normalized = waypoint_data.model_dump()

                            if normalized.get("waypointKind") == "quick":
                                max_quick_marks = normalized.get("maxQuickMarks")
                                if isinstance(max_quick_marks, (int, float)):
                                    max_quick_marks = max(1, min(int(max_quick_marks), 100))
                                elif bool(normalized.get("replaceOldQuick")):
                                    max_quick_marks = 1
                                else:
                                    max_quick_marks = None

                                if max_quick_marks is not None:
                                    # 限流策略：超出上限时按最旧 quick 路标淘汰。
                                    old_quick_waypoints = [
                                        (wid, source_bucket[submit_player_id])
                                        for wid, source_bucket in list(state.waypoint_reports.items())
                                        if wid != waypoint_id
                                        and isinstance(source_bucket, dict)
                                        and submit_player_id in source_bucket
                                        and isinstance(source_bucket[submit_player_id], dict)
                                        and isinstance(source_bucket[submit_player_id].get("data"), dict)
                                        and source_bucket[submit_player_id]["data"].get("waypointKind") == "quick"
                                    ]

                                    remove_count = len(old_quick_waypoints) - max_quick_marks + 1
                                    if remove_count > 0:
                                        old_quick_waypoints.sort(key=lambda item: state.node_timestamp(item[1]))
                                        for old_id, _ in old_quick_waypoints[:remove_count]:
                                            state.delete_report(state.waypoint_reports, old_id, submit_player_id)

                            node = state.build_state_node(submit_player_id, current_time, normalized)
                            state.upsert_report(state.waypoint_reports, waypoint_id, submit_player_id, node)
                        except Exception as e:
                            logger.warning("Error validating waypoint data for %s: %s", waypoint_id, e)

                    continue

                if packet.type == "battle_map_observation":
                    current_time = time.time()
                    result = state.apply_battle_map_observation(
                        submit_player_id=submit_player_id,
                        room_code=state.get_player_room(submit_player_id),
                        dimension=packet.dimension,
                        map_size=packet.mapSize,
                        anchor_row=packet.anchorRow,
                        anchor_col=packet.anchorCol,
                        snapshot_observed_at=packet.snapshotObservedAt,
                        parsed_at=packet.parsedAt,
                        candidates=[candidate.model_dump() for candidate in packet.candidates],
                        cells=[cell.model_dump() for cell in packet.cells],
                        current_time=current_time,
                    )
                    if not result.get("accepted"):
                        logger.warning(
                            "Ignored battle_map_observation submitPlayerId=%s reason=%s",
                            submit_player_id,
                            result.get("reason"),
                        )

                    else:
                        logger.debug(
                            "Accepted battle_map_observation submitPlayerId=%s reason=%s upserted=%s currentTime=%s",
                            submit_player_id,
                            result.get("reason"),
                            result.get("upserted"),
                            current_time,
                        )

                    continue

                if packet.type == "waypoints_delete":
                    # 路标删除：根据 deletableBy 权限控制删除逻辑。
                    waypoint_ids = packet.waypointIds

                    for waypoint_id in waypoint_ids:
                        if not isinstance(waypoint_id, str):
                            continue

                        # 检查 waypoint 是否存在
                        source_bucket = state.waypoint_reports.get(waypoint_id)
                        if not isinstance(source_bucket, dict) or not source_bucket:
                            continue

                        # 获取第一个来源的节点数据来检查权限（假设同一 waypoint 的不同来源有相同的权限设置）
                        first_node = next(iter(source_bucket.values()), None)
                        if not isinstance(first_node, dict):
                            continue

                        waypoint_data = first_node.get("data", {})
                        if not isinstance(waypoint_data, dict):
                            continue

                        # 检查删除权限：deletableBy="owner" 时仅创建者可删除
                        deletable_by = waypoint_data.get("deletableBy", "everyone")
                        if deletable_by == "owner":
                            # 仅当提交者是创建者时才允许删除
                            if submit_player_id not in source_bucket:
                                logger.debug(
                                    "Waypoint delete denied: playerId=%s is not owner of waypoint=%s",
                                    submit_player_id,
                                    waypoint_id,
                                )
                                continue

                        # 执行删除：删除当前提交者的报告
                        state.delete_report(state.waypoint_reports, waypoint_id, submit_player_id)

                    continue

                if packet.type == "waypoints_entity_death_cancel":
                    # 实体死亡撤销：清理 targetEntityId 命中的 entity 类型路标。
                    target_entity_ids = packet.targetEntityIds

                    target_entity_id_set = {
                        entity_id for entity_id in target_entity_ids
                        if isinstance(entity_id, str) and entity_id.strip()
                    }

                    if target_entity_id_set:
                        for waypoint_id in list(state.waypoint_reports.keys()):
                            source_bucket = state.waypoint_reports.get(waypoint_id)
                            if not isinstance(source_bucket, dict):
                                continue

                            for source_id in list(source_bucket.keys()):
                                node = source_bucket.get(source_id)
                                if not isinstance(node, dict):
                                    continue
                                payload = node.get("data")
                                if not isinstance(payload, dict):
                                    continue
                                if payload.get("targetType") != "entity":
                                    continue
                                if payload.get("targetEntityId") not in target_entity_id_set:
                                    continue
                                state.delete_report(state.waypoint_reports, waypoint_id, source_id)

                    continue

                if isinstance(packet, ResyncRequestPacket) and submit_player_id:
                    # 客户端主动请求全量重同步。
                    try:
                        await broadcaster.send_snapshot_full_to_player(submit_player_id)
                    except Exception as e:
                        logger.warning("Error sending snapshot_full to %s: %s", submit_player_id, e)
                    continue

    except WebSocketDisconnect as exc:
        disconnect_reason = "client_disconnect"
        disconnect_code = getattr(exc, "code", None)
        disconnect_exception = exc
    except Exception as e:
        disconnect_reason = f"error:{type(e).__name__}"
        disconnect_exception = e
        logger.exception("Error handling player message: %s", e)
        await record_audit_event(
            event_type="backend_error",
            actor_type="player",
            actor_id=submit_player_id,
            room_code=state.get_player_room(submit_player_id) if submit_player_id else None,
            success=False,
            remote_addr=remote_addr,
            detail={
                "scope": "player_ws",
                "errorType": type(e).__name__,
                "message": str(e),
            },
        )
    finally:
        if submit_player_id:
            trigger_admin_sse_overview()
            await record_audit_event(
                event_type="player_disconnected",
                actor_type="player",
                actor_id=submit_player_id,
                room_code=state.get_player_room(submit_player_id),
                success=True,
                remote_addr=remote_addr,
                detail={"reason": disconnect_reason, "code": disconnect_code},
            )
            state.remove_connection(submit_player_id)
            await broadcaster.broadcast_web_map_updates()
            logger.info("Client %s disconnected", submit_player_id)


@app.get("/health")
async def health_check():
    """健康检查：用于探活。"""
    return JSONResponse({"status": "ok"})


@app.get("/snapshot")
async def snapshot(roomCode: str | None = None):
    """调试快照：返回当前最终视图与连接状态。"""
    current_time = time.time()

    connections_by_room: dict[str, list[str]] = {}
    for player_id in state.connections.keys():
        if not isinstance(player_id, str) or not player_id:
            continue
        room = state.get_player_room(player_id)
        connections_by_room.setdefault(room, []).append(player_id)

    for room in list(connections_by_room.keys()):
        connections_by_room[room].sort()

    active_rooms = sorted(connections_by_room.keys())
    requested_room = state.normalize_room_code(roomCode) if roomCode is not None else None
    selected_room = requested_room if requested_room is not None else state.DEFAULT_ROOM_CODE
    selected_sources = state.get_active_sources_in_room(selected_room)

    selected_players = state.filter_state_map_by_sources(state.players, selected_sources)
    selected_entities = state.filter_state_map_by_sources(state.entities, selected_sources)
    selected_waypoints = state.filter_waypoint_state_by_sources_and_room(
        state.waypoints,
        selected_sources,
        selected_room,
    )
    selected_battle_chunks = state.filter_battle_chunk_state_by_sources_and_room(
        state.battle_chunks,
        selected_sources,
        selected_room,
    )

    room_digests = {
        "players": state.state_digest(selected_players),
        "entities": state.state_digest(selected_entities),
        "waypoints": state.state_digest(selected_waypoints),
        "battleChunks": state.state_digest(selected_battle_chunks),
    }

    return JSONResponse({
        "server_time": current_time,
        "players": dict(state.players),
        "entities": dict(state.entities),
        "waypoints": dict(state.waypoints),
        "battleChunks": dict(state.battle_chunks),
        "playerMarks": dict(state.player_marks),
        "tabState": state.build_web_map_tab_snapshot(selected_room),
        "connections": list(state.connections.keys()),
        "connections_count": len(state.connections),
        "activeRooms": active_rooms,
        "connectionsByRoom": connections_by_room,
        "requestedRoomCode": requested_room,
        "selectedRoomCode": selected_room,
        "roomView": {
            "roomCode": selected_room,
            "connections": sorted(selected_sources),
            "connections_count": len(selected_sources),
            "players": dict(selected_players),
            "entities": dict(selected_entities),
            "waypoints": dict(selected_waypoints),
            "battleChunks": dict(selected_battle_chunks),
            "tabState": state.build_web_map_tab_snapshot(selected_room),
            "digests": room_digests,
        },
        "broadcastHz": state.broadcast_hz,
        "digests": state.build_digests(),
    })


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host="0.0.0.0", port=8765, ws_per_message_deflate=True)

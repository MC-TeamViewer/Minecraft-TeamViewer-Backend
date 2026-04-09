import time
import uuid

from fastapi import WebSocket, WebSocketDisconnect
from pydantic import ValidationError

from ..admin.auth import (
    record_audit_event,
    record_player_identity,
    record_player_activity,
    trigger_admin_sse_overview,
)
from ..admin.proxy_ip import get_websocket_remote_addr
from ..app import runtime
from ..core.models import EntityData, PlayerData, WaypointData
from ..core.protocol import (
    BattleChunkMetaRequestPacket,
    BattleChunkMetaSnapshotPacket,
    BattleMapObservationPacket,
    CommandPlayerMarkClearAllPacket,
    CommandPlayerMarkClearPacket,
    CommandPlayerMarkSetPacket,
    CommandSameServerFilterSetPacket,
    CommandTacticalWaypointSetPacket,
    HandshakeAckPacket,
    HandshakeHelpers,
    HandshakePacket,
    PacketDecodeError,
    PacketParsers,
    PingPacket,
    PongPacket,
    ResyncRequestPacket,
    SourceStateClearPacket,
    WaypointsDeletePacket,
    WebMapAckPacket,
)
from .io import (
    describe_websocket,
    expand_player_packets,
    normalize_waypoint_color_to_int,
    receive_payload,
    reject_handshake,
    require_wire_channel,
    resolve_handshake_rejection_reason,
    send_packet,
)


def _resolve_self_reported_username(
    submit_player_id: str | None,
    player_id: str,
    player_data: dict,
) -> str | None:
    normalized_submit_player_id = str(submit_player_id or "").strip()
    normalized_player_id = str(player_id or "").strip()
    if not normalized_submit_player_id:
        return None

    player_name = str(player_data.get("playerName") or "").strip()
    if not player_name:
        return None

    if normalized_player_id == normalized_submit_player_id:
        return player_name

    player_uuid = str(player_data.get("playerUUID") or "").strip()
    if player_uuid and player_uuid == normalized_submit_player_id:
        return player_name

    return None


async def web_map_ws(websocket: WebSocket):
    await websocket.accept()
    web_map_id = str(id(websocket))
    remote_addr = get_websocket_remote_addr(websocket)
    if str(websocket.url.path) == "/adminws":
        runtime.logger.warning("Deprecated websocket route /adminws used; migrate clients to /web-map/ws")
    handshake_completed = False
    web_map_room = runtime.state.DEFAULT_ROOM_CODE
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
                        room_code=runtime.state.DEFAULT_ROOM_CODE,
                        success=False,
                        remote_addr=remote_addr,
                        detail={"reason": exc.detail},
                    )
                    await reject_handshake(websocket, exc.detail, runtime.state.DEFAULT_ROOM_CODE, channel="web_map")
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
                    legacy_room = runtime.state.set_web_map_room(
                        web_map_id,
                        HandshakeHelpers.room_code(packet, runtime.state.DEFAULT_ROOM_CODE),
                    )
                    legacy_reason = runtime.LEGACY_PROTOCOL_REJECTION_REASON
                    runtime.logger.warning(
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
                web_map_room = runtime.state.normalize_room_code(
                    HandshakeHelpers.room_code(packet, runtime.state.DEFAULT_ROOM_CODE)
                )
                rejection_reason = resolve_handshake_rejection_reason(packet)
                if rejection_reason:
                    runtime.logger.warning(
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
                        networkProtocolVersion=runtime.NETWORK_PROTOCOL_VERSION,
                        minimumCompatibleNetworkProtocolVersion=runtime.SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION,
                        localProgramVersion=runtime.SERVER_PROGRAM_VERSION,
                        roomCode=web_map_room,
                        deltaEnabled=True,
                        broadcastHz=runtime.state.broadcast_hz,
                        playerTimeoutSec=runtime.state.PLAYER_TIMEOUT,
                        entityTimeoutSec=runtime.state.ENTITY_TIMEOUT,
                        battleChunkTimeoutSec=runtime.state.BATTLE_CHUNK_TIMEOUT,
                    ),
                    channel="web_map",
                )
                runtime.state.web_map_connections[web_map_id] = websocket
                runtime.state.set_web_map_room(web_map_id, web_map_room)
                runtime.web_map_connection_meta[web_map_id] = {
                    "protocolVersion": client_protocol,
                    "programVersion": client_program_version,
                    "displayName": "Web Map",
                    "remoteAddr": remote_addr,
                }
                handshake_completed = True
                runtime.logger.info(
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
                    await runtime.broadcaster.send_web_map_snapshot_full(web_map_id)
                except Exception as exc:
                    runtime.logger.warning(
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
                await runtime.broadcaster.send_web_map_snapshot_full(web_map_id)
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
                        battleChunks=runtime.state.select_battle_chunk_meta_snapshot(web_map_room, requested_chunk_ids),
                    ),
                    channel="web_map",
                )
                continue

            if isinstance(packet, CommandPlayerMarkSetPacket):
                target_player_id = packet.playerId
                updated_mark = runtime.state.set_player_mark(
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
                removed = runtime.state.clear_player_mark(target_player_id)
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
                    await runtime.broadcaster.broadcast_web_map_updates()
                continue

            if isinstance(packet, CommandPlayerMarkClearAllPacket):
                removed_count = runtime.state.clear_all_player_marks()
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
                runtime.state.same_server_filter_enabled = bool(packet.enabled)
                await send_packet(
                    websocket,
                    WebMapAckPacket(
                        ok=True,
                        action="command_same_server_filter_set",
                        enabled=runtime.state.same_server_filter_enabled,
                    ),
                )
                continue

            if isinstance(packet, CommandTacticalWaypointSetPacket):
                room_code = runtime.state.normalize_room_code(packet.roomCode or runtime.state.get_web_map_room(web_map_id))
                waypoint_id_raw = packet.waypointId
                waypoint_id = (
                    str(waypoint_id_raw).strip()
                    if isinstance(waypoint_id_raw, str) and waypoint_id_raw.strip()
                    else ""
                )
                if not waypoint_id:
                    waypoint_id = f"web_map_tactical:{int(time.time() * 1000)}:{uuid.uuid4().hex[:8]}"

                label = str(packet.label or "战术标记").strip() or "战术标记"
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
                    "x": packet.x,
                    "y": 64,
                    "z": packet.z,
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

                web_map_source_id = runtime.state.build_web_map_tactical_source_id(room_code)
                node = runtime.state.build_state_node(web_map_source_id, time.time(), validated.model_dump())
                runtime.state.upsert_report(runtime.state.waypoint_reports, waypoint_id, web_map_source_id, node)

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
                room_code = runtime.state.normalize_room_code(runtime.state.get_web_map_room(web_map_id))
                web_map_source_id = runtime.state.build_web_map_tactical_source_id(room_code)
                removed_ids: list[str] = []

                for waypoint_id in packet.waypointIds:
                    if not isinstance(waypoint_id, str):
                        continue
                    waypoint_id = waypoint_id.strip()
                    if not waypoint_id:
                        continue

                    source_bucket = runtime.state.waypoint_reports.get(waypoint_id)
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
                            removed_for_waypoint = (
                                runtime.state.delete_report(runtime.state.waypoint_reports, waypoint_id, source_id)
                                or removed_for_waypoint
                            )
                            continue

                        if deletable_by == "owner" and source_id == web_map_source_id:
                            removed_for_waypoint = (
                                runtime.state.delete_report(runtime.state.waypoint_reports, waypoint_id, source_id)
                                or removed_for_waypoint
                            )

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
    except Exception as exc:
        disconnect_reason = f"error:{type(exc).__name__}"
        disconnect_exception = exc
        runtime.logger.exception("Web-map websocket error: %s", exc)
        await record_audit_event(
            event_type="backend_error",
            actor_type="web_map",
            actor_id=web_map_id,
            room_code=web_map_room,
            success=False,
            remote_addr=remote_addr,
            detail={
                "scope": "web_map_ws",
                "errorType": type(exc).__name__,
                "message": str(exc),
            },
        )
    finally:
        runtime.logger.info(
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
        if web_map_id in runtime.state.web_map_connections:
            del runtime.state.web_map_connections[web_map_id]
        if web_map_id in runtime.state.web_map_connection_rooms:
            del runtime.state.web_map_connection_rooms[web_map_id]
        runtime.web_map_connection_meta.pop(web_map_id, None)


async def reserved_admin_ws(websocket: WebSocket):
    await websocket.accept()
    try:
        payload = await receive_payload(websocket, allow_legacy_handshake=False)
        require_wire_channel(payload, "admin", "/admin/ws")
        packet = PacketParsers.parse_admin(payload)
        if isinstance(packet, HandshakePacket):
            await reject_handshake(
                websocket,
                "admin_interface_reserved: /admin/ws is reserved for a future management interface",
                HandshakeHelpers.room_code(packet, runtime.state.DEFAULT_ROOM_CODE),
                channel="admin",
            )
            return
        await websocket.close(code=1008, reason="admin_interface_reserved")
    except PacketDecodeError:
        await websocket.close(code=1008, reason="admin_interface_reserved")


async def websocket_endpoint(websocket: WebSocket):
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
            except PacketDecodeError as exc:
                if submit_player_id is None and exc.code == "channel_mismatch":
                    await record_audit_event(
                        event_type="player_handshake_failed",
                        actor_type="player",
                        room_code=runtime.state.DEFAULT_ROOM_CODE,
                        success=False,
                        remote_addr=remote_addr,
                        detail={"reason": exc.detail},
                    )
                    await reject_handshake(
                        websocket,
                        exc.detail,
                        runtime.state.DEFAULT_ROOM_CODE,
                        channel="player",
                    )
                    return

                payload_type = None
                if isinstance(payload, dict):
                    raw_type = payload.get("type")
                    if isinstance(raw_type, str) and raw_type.strip():
                        payload_type = raw_type.strip()

                runtime.logger.warning(
                    "Error decoding player packet type=%s detail=%s",
                    payload_type or "unknown",
                    exc.detail,
                )
                continue

            packet_submit_id = getattr(packet, "submitPlayerId", None)
            if isinstance(packet_submit_id, str) and packet_submit_id:
                submit_player_id = packet_submit_id

            if payload.get("_legacy_msgpack"):
                legacy_room = HandshakeHelpers.room_code(packet, runtime.state.DEFAULT_ROOM_CODE)
                legacy_reason = runtime.LEGACY_PROTOCOL_REJECTION_REASON
                runtime.logger.warning(
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

            if isinstance(packet, HandshakePacket):
                rejection_reason = resolve_handshake_rejection_reason(packet)
                if rejection_reason:
                    runtime.logger.warning(
                        "Player handshake rejected (submitPlayerId=%s, reason=%s)",
                        submit_player_id,
                        rejection_reason,
                    )
                    await record_audit_event(
                        event_type="player_handshake_failed",
                        actor_type="player",
                        actor_id=submit_player_id,
                        room_code=HandshakeHelpers.room_code(packet, runtime.state.DEFAULT_ROOM_CODE),
                        success=False,
                        remote_addr=remote_addr,
                        detail={"reason": rejection_reason},
                    )
                    await reject_handshake(
                        websocket,
                        rejection_reason,
                        HandshakeHelpers.room_code(packet, runtime.state.DEFAULT_ROOM_CODE),
                        channel="player",
                    )
                    return

                if submit_player_id:
                    client_protocol = HandshakeHelpers.protocol_version(packet)
                    client_program_version = HandshakeHelpers.program_version(packet)
                    client_room = runtime.state.normalize_room_code(
                        HandshakeHelpers.room_code(packet, runtime.state.DEFAULT_ROOM_CODE)
                    )
                    runtime.state.mark_player_capability(
                        submit_player_id,
                        client_protocol,
                        packet.preferredReportIntervalTicks,
                        packet.minReportIntervalTicks,
                        packet.maxReportIntervalTicks,
                    )
                    negotiated_ticks = runtime.state.negotiate_report_interval_ticks(
                        submit_player_id,
                        packet.preferredReportIntervalTicks,
                        packet.minReportIntervalTicks,
                        packet.maxReportIntervalTicks,
                    )
                    caps = runtime.state.connection_caps.get(submit_player_id)
                    if isinstance(caps, dict):
                        caps["negotiatedReportIntervalTicks"] = negotiated_ticks
                        caps["programVersion"] = client_program_version
                        caps["remoteAddr"] = remote_addr

                    ack = {
                        "networkProtocolVersion": runtime.NETWORK_PROTOCOL_VERSION,
                        "minimumCompatibleNetworkProtocolVersion": runtime.SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION,
                        "localProgramVersion": runtime.SERVER_PROGRAM_VERSION,
                        "roomCode": client_room,
                        "deltaEnabled": True,
                        "digestIntervalSec": runtime.state.DIGEST_INTERVAL_SEC,
                        "broadcastHz": runtime.state.broadcast_hz,
                        "reportIntervalTicks": negotiated_ticks,
                        "playerTimeoutSec": runtime.state.PLAYER_TIMEOUT,
                        "entityTimeoutSec": runtime.state.ENTITY_TIMEOUT,
                        "battleChunkTimeoutSec": runtime.state.BATTLE_CHUNK_TIMEOUT,
                    }
                    await send_packet(websocket, HandshakeAckPacket(**ack))
                    runtime.state.connections[submit_player_id] = websocket
                    runtime.state.set_player_room(submit_player_id, client_room)
                    runtime.logger.info(
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
                    await runtime.broadcaster.send_snapshot_full_to_player(submit_player_id)
                continue

            if not submit_player_id or submit_player_id not in runtime.state.connections:
                runtime.logger.debug(
                    "Ignore player packet before handshake registration submitPlayerId=%s",
                    submit_player_id,
                )
                continue

            await record_player_activity(
                submit_player_id,
                runtime.state.get_player_room(submit_player_id),
            )

            for expanded_packet in expand_player_packets(packet):
                if (
                    expanded_packet.type not in {"tab_players_update", "tab_players_patch"}
                    and not isinstance(expanded_packet, SourceStateClearPacket)
                ):
                    runtime.state.touch_tab_player_report(submit_player_id, time.time())

                if expanded_packet.type == "state_keepalive":
                    current_time = time.time()
                    touched_players = runtime.state.touch_reports(
                        runtime.state.player_reports,
                        expanded_packet.players,
                        submit_player_id,
                        current_time,
                    )
                    touched_entities = runtime.state.touch_reports(
                        runtime.state.entity_reports,
                        expanded_packet.entities,
                        submit_player_id,
                        current_time,
                    )
                    touched_battle_chunks = runtime.state.touch_reports(
                        runtime.state.battle_chunk_reports,
                        expanded_packet.battleChunks,
                        submit_player_id,
                        current_time,
                    )
                    if touched_players or touched_entities or touched_battle_chunks:
                        runtime.logger.debug(
                            "Applied state_keepalive "
                            f"submitPlayerId={submit_player_id} players={touched_players}/{len(expanded_packet.players)} "
                            f"entities={touched_entities}/{len(expanded_packet.entities)} "
                            f"battleChunks={touched_battle_chunks}/{len(expanded_packet.battleChunks)}"
                        )
                    continue

                if isinstance(expanded_packet, SourceStateClearPacket):
                    runtime.state.clear_source_state(submit_player_id, expanded_packet.scopes)
                    await runtime.broadcaster.broadcast_web_map_updates()
                    runtime.logger.info(
                        "Cleared source state for submitPlayerId=%s scopes=%s",
                        submit_player_id,
                        expanded_packet.scopes or ["players", "entities", "tab_players", "waypoints"],
                    )
                    continue

                if expanded_packet.type == "players_update":
                    current_time = time.time()
                    for pid, player_data in expanded_packet.players.items():
                        try:
                            normalized = player_data.model_dump()
                            node = runtime.state.build_state_node(submit_player_id, current_time, normalized)
                            runtime.state.upsert_report(runtime.state.player_reports, pid, submit_player_id, node)
                            username = _resolve_self_reported_username(submit_player_id, pid, normalized)
                            if username is not None:
                                await record_player_identity(submit_player_id, username)
                        except Exception as exc:
                            runtime.logger.warning("Error validating player data for %s: %s", pid, exc)
                    continue

                if expanded_packet.type == "tab_players_update":
                    if isinstance(submit_player_id, str) and submit_player_id:
                        current_time = time.time()
                        runtime.state.upsert_tab_player_report(submit_player_id, expanded_packet.tabPlayers, current_time)
                        await runtime.broadcaster.broadcast_web_map_updates()
                    continue

                if expanded_packet.type == "tab_players_patch":
                    if isinstance(submit_player_id, str) and submit_player_id:
                        current_time = time.time()
                        runtime.state.patch_tab_player_report(
                            submit_player_id,
                            expanded_packet.upsert,
                            expanded_packet.delete,
                            current_time,
                        )
                        await runtime.broadcaster.broadcast_web_map_updates()
                    continue

                if expanded_packet.type == "players_patch":
                    current_time = time.time()
                    missing_baseline_players = []
                    for pid, player_data in expanded_packet.upsert.items():
                        source_key = submit_player_id if isinstance(submit_player_id, str) else ""
                        existing_node = runtime.state.player_reports.get(pid, {}).get(source_key)
                        try:
                            normalized = runtime.state.merge_patch_and_validate(PlayerData, existing_node, player_data)
                            node = runtime.state.build_state_node(submit_player_id, current_time, normalized)
                            runtime.state.upsert_report(runtime.state.player_reports, pid, submit_player_id, node)
                            username = _resolve_self_reported_username(submit_player_id, pid, normalized)
                            if username is not None:
                                await record_player_identity(submit_player_id, username)
                        except ValidationError as exc:
                            missing_fields = runtime.state.missing_fields_from_validation_error(exc)
                            existing_data = existing_node.get("data") if isinstance(existing_node, dict) else None
                            existing_keys = sorted(existing_data.keys()) if isinstance(existing_data, dict) else []
                            if not isinstance(existing_data, dict):
                                missing_baseline_players.append(pid)
                            runtime.logger.warning(
                                "Player patch validation failed "
                                f"pid={pid} submitPlayerId={submit_player_id} sourceKey={source_key!r} "
                                f"hasExistingSnapshot={bool(isinstance(existing_data, dict))} "
                                f"missingFields={missing_fields or '[]'} "
                                f"existingKeys={existing_keys} payload={runtime.state.payload_preview(player_data)} "
                                f"errors={runtime.state.payload_preview(exc.errors(), 480)}"
                            )
                        except Exception as exc:
                            runtime.logger.exception(
                                "Unexpected error validating player patch "
                                f"pid={pid} submitPlayerId={submit_player_id} "
                                f"payload={runtime.state.payload_preview(player_data)}: {exc}"
                            )

                    if isinstance(expanded_packet.delete, list):
                        for pid in expanded_packet.delete:
                            if not isinstance(pid, str):
                                continue
                            runtime.state.delete_report(runtime.state.player_reports, pid, submit_player_id)

                    if missing_baseline_players and isinstance(submit_player_id, str) and submit_player_id:
                        await runtime.broadcaster.send_refresh_request_to_source(
                            submit_player_id,
                            players=missing_baseline_players,
                            entities=[],
                            battle_chunks=[],
                            reason="missing_baseline_patch",
                            bypass_cooldown=False,
                        )
                    continue

                if expanded_packet.type == "entities_update":
                    current_time = time.time()
                    source_key = submit_player_id if isinstance(submit_player_id, str) else ""
                    for entity_id in list(runtime.state.entity_reports.keys()):
                        source_bucket = runtime.state.entity_reports.get(entity_id, {})
                        if source_key in source_bucket:
                            runtime.state.delete_report(runtime.state.entity_reports, entity_id, submit_player_id)

                    for entity_id, entity_data in expanded_packet.entities.items():
                        try:
                            normalized = entity_data.model_dump()
                            node = runtime.state.build_state_node(submit_player_id, current_time, normalized)
                            runtime.state.upsert_report(runtime.state.entity_reports, entity_id, submit_player_id, node)
                        except Exception as exc:
                            runtime.logger.warning("Error validating entity data for %s: %s", entity_id, exc)
                    continue

                if expanded_packet.type == "entities_patch":
                    current_time = time.time()
                    missing_baseline_entities = []
                    for entity_id, entity_data in expanded_packet.upsert.items():
                        source_key = submit_player_id if isinstance(submit_player_id, str) else ""
                        existing_node = runtime.state.entity_reports.get(entity_id, {}).get(source_key)
                        try:
                            normalized = runtime.state.merge_patch_and_validate(EntityData, existing_node, entity_data)
                            node = runtime.state.build_state_node(submit_player_id, current_time, normalized)
                            runtime.state.upsert_report(runtime.state.entity_reports, entity_id, submit_player_id, node)
                        except ValidationError as exc:
                            missing_fields = runtime.state.missing_fields_from_validation_error(exc)
                            existing_data = existing_node.get("data") if isinstance(existing_node, dict) else None
                            existing_keys = sorted(existing_data.keys()) if isinstance(existing_data, dict) else []
                            if not isinstance(existing_data, dict):
                                missing_baseline_entities.append(entity_id)
                            runtime.logger.warning(
                                "Entity patch validation failed "
                                f"entityId={entity_id} submitPlayerId={submit_player_id} sourceKey={source_key!r} "
                                f"hasExistingSnapshot={bool(isinstance(existing_data, dict))} "
                                f"missingFields={missing_fields or '[]'} "
                                f"existingKeys={existing_keys} payload={runtime.state.payload_preview(entity_data)} "
                                f"errors={runtime.state.payload_preview(exc.errors(), 480)}"
                            )
                        except Exception as exc:
                            runtime.logger.exception(
                                "Unexpected error validating entity patch "
                                f"entityId={entity_id} submitPlayerId={submit_player_id} "
                                f"payload={runtime.state.payload_preview(entity_data)}: {exc}"
                            )

                    if isinstance(expanded_packet.delete, list):
                        for entity_id in expanded_packet.delete:
                            if not isinstance(entity_id, str):
                                continue
                            runtime.state.delete_report(runtime.state.entity_reports, entity_id, submit_player_id)

                    if missing_baseline_entities and isinstance(submit_player_id, str) and submit_player_id:
                        await runtime.broadcaster.send_refresh_request_to_source(
                            submit_player_id,
                            players=[],
                            entities=missing_baseline_entities,
                            battle_chunks=[],
                            reason="missing_baseline_patch",
                            bypass_cooldown=False,
                        )
                    continue

                if expanded_packet.type == "waypoints_patch":
                    current_time = time.time()
                    for waypoint_id, waypoint_data in expanded_packet.upsert.items():
                        source_key = submit_player_id if isinstance(submit_player_id, str) else ""
                        existing_node = runtime.state.waypoint_reports.get(waypoint_id, {}).get(source_key)
                        try:
                            normalized = runtime.state.merge_patch_and_validate(WaypointData, existing_node, waypoint_data)
                            node = runtime.state.build_state_node(submit_player_id, current_time, normalized)
                            runtime.state.upsert_report(runtime.state.waypoint_reports, waypoint_id, submit_player_id, node)
                        except ValidationError as exc:
                            existing_data = existing_node.get("data") if isinstance(existing_node, dict) else None
                            runtime.logger.warning(
                                "Waypoint patch validation failed "
                                f"waypointId={waypoint_id} submitPlayerId={submit_player_id} "
                                f"hasExistingSnapshot={bool(isinstance(existing_data, dict))} "
                                f"payload={runtime.state.payload_preview(waypoint_data)} "
                                f"errors={runtime.state.payload_preview(exc.errors(), 480)}"
                            )
                        except Exception as exc:
                            runtime.logger.exception(
                                "Unexpected error validating waypoint patch "
                                f"waypointId={waypoint_id} submitPlayerId={submit_player_id} "
                                f"payload={runtime.state.payload_preview(waypoint_data)}: {exc}"
                            )

                    if isinstance(expanded_packet.delete, list):
                        for waypoint_id in expanded_packet.delete:
                            if not isinstance(waypoint_id, str):
                                continue
                            runtime.state.delete_report(runtime.state.waypoint_reports, waypoint_id, submit_player_id)
                    continue

                if expanded_packet.type == "waypoints_update":
                    current_time = time.time()
                    for waypoint_id, waypoint_data in expanded_packet.waypoints.items():
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
                                    old_quick_waypoints = [
                                        (wid, source_bucket[submit_player_id])
                                        for wid, source_bucket in list(runtime.state.waypoint_reports.items())
                                        if wid != waypoint_id
                                        and isinstance(source_bucket, dict)
                                        and submit_player_id in source_bucket
                                        and isinstance(source_bucket[submit_player_id], dict)
                                        and isinstance(source_bucket[submit_player_id].get("data"), dict)
                                        and source_bucket[submit_player_id]["data"].get("waypointKind") == "quick"
                                    ]
                                    remove_count = len(old_quick_waypoints) - max_quick_marks + 1
                                    if remove_count > 0:
                                        old_quick_waypoints.sort(key=lambda item: runtime.state.node_timestamp(item[1]))
                                        for old_id, _ in old_quick_waypoints[:remove_count]:
                                            runtime.state.delete_report(
                                                runtime.state.waypoint_reports,
                                                old_id,
                                                submit_player_id,
                                            )

                            node = runtime.state.build_state_node(submit_player_id, current_time, normalized)
                            runtime.state.upsert_report(runtime.state.waypoint_reports, waypoint_id, submit_player_id, node)
                        except Exception as exc:
                            runtime.logger.warning("Error validating waypoint data for %s: %s", waypoint_id, exc)
                    continue

                if expanded_packet.type == "battle_map_observation":
                    current_time = time.time()
                    result = runtime.state.apply_battle_map_observation(
                        submit_player_id=submit_player_id,
                        room_code=runtime.state.get_player_room(submit_player_id),
                        dimension=expanded_packet.dimension,
                        map_size=expanded_packet.mapSize,
                        anchor_row=expanded_packet.anchorRow,
                        anchor_col=expanded_packet.anchorCol,
                        snapshot_observed_at=expanded_packet.snapshotObservedAt,
                        parsed_at=expanded_packet.parsedAt,
                        candidates=[candidate.model_dump() for candidate in expanded_packet.candidates],
                        cells=[cell.model_dump() for cell in expanded_packet.cells],
                        current_time=current_time,
                    )
                    if not result.get("accepted"):
                        runtime.logger.warning(
                            "Ignored battle_map_observation submitPlayerId=%s reason=%s",
                            submit_player_id,
                            result.get("reason"),
                        )
                    else:
                        runtime.logger.debug(
                            "Accepted battle_map_observation submitPlayerId=%s reason=%s upserted=%s currentTime=%s",
                            submit_player_id,
                            result.get("reason"),
                            result.get("upserted"),
                            current_time,
                        )
                    continue

                if expanded_packet.type == "waypoints_delete":
                    for waypoint_id in expanded_packet.waypointIds:
                        if not isinstance(waypoint_id, str):
                            continue
                        source_bucket = runtime.state.waypoint_reports.get(waypoint_id)
                        if not isinstance(source_bucket, dict) or not source_bucket:
                            continue
                        first_node = next(iter(source_bucket.values()), None)
                        if not isinstance(first_node, dict):
                            continue
                        waypoint_data = first_node.get("data", {})
                        if not isinstance(waypoint_data, dict):
                            continue
                        deletable_by = waypoint_data.get("deletableBy", "everyone")
                        if deletable_by == "owner" and submit_player_id not in source_bucket:
                            runtime.logger.debug(
                                "Waypoint delete denied: playerId=%s is not owner of waypoint=%s",
                                submit_player_id,
                                waypoint_id,
                            )
                            continue
                        runtime.state.delete_report(runtime.state.waypoint_reports, waypoint_id, submit_player_id)
                    continue

                if expanded_packet.type == "waypoints_entity_death_cancel":
                    target_entity_id_set = {
                        entity_id
                        for entity_id in expanded_packet.targetEntityIds
                        if isinstance(entity_id, str) and entity_id.strip()
                    }
                    if target_entity_id_set:
                        for waypoint_id in list(runtime.state.waypoint_reports.keys()):
                            source_bucket = runtime.state.waypoint_reports.get(waypoint_id)
                            if not isinstance(source_bucket, dict):
                                continue

                            for source_id in list(source_bucket.keys()):
                                node = source_bucket.get(source_id)
                                if not isinstance(node, dict):
                                    continue
                                payload_data = node.get("data")
                                if not isinstance(payload_data, dict):
                                    continue
                                if payload_data.get("targetType") != "entity":
                                    continue
                                if payload_data.get("targetEntityId") not in target_entity_id_set:
                                    continue
                                runtime.state.delete_report(runtime.state.waypoint_reports, waypoint_id, source_id)
                    continue

                if isinstance(expanded_packet, ResyncRequestPacket) and submit_player_id:
                    try:
                        await runtime.broadcaster.send_snapshot_full_to_player(submit_player_id)
                    except Exception as exc:
                        runtime.logger.warning("Error sending snapshot_full to %s: %s", submit_player_id, exc)
                    continue

    except WebSocketDisconnect as exc:
        disconnect_reason = "client_disconnect"
        disconnect_code = getattr(exc, "code", None)
        disconnect_exception = exc
    except Exception as exc:
        disconnect_reason = f"error:{type(exc).__name__}"
        disconnect_exception = exc
        runtime.logger.exception("Error handling player message: %s", exc)
        await record_audit_event(
            event_type="backend_error",
            actor_type="player",
            actor_id=submit_player_id,
            room_code=runtime.state.get_player_room(submit_player_id) if submit_player_id else None,
            success=False,
            remote_addr=remote_addr,
            detail={
                "scope": "player_ws",
                "errorType": type(exc).__name__,
                "message": str(exc),
            },
        )
    finally:
        if submit_player_id:
            trigger_admin_sse_overview()
            await record_audit_event(
                event_type="player_disconnected",
                actor_type="player",
                actor_id=submit_player_id,
                room_code=runtime.state.get_player_room(submit_player_id),
                success=True,
                remote_addr=remote_addr,
                detail={"reason": disconnect_reason, "code": disconnect_code},
            )
            runtime.state.remove_connection(submit_player_id)
            await runtime.broadcaster.broadcast_web_map_updates()
            runtime.logger.info("Client %s disconnected", submit_player_id)


def register_websocket_routes(app) -> None:
    app.add_api_websocket_route("/web-map/ws", web_map_ws)
    app.add_api_websocket_route("/adminws", web_map_ws)
    app.add_api_websocket_route("/admin/ws", reserved_admin_ws)
    app.add_api_websocket_route("/playeresp", websocket_endpoint)
    app.add_api_websocket_route("/mc-client", websocket_endpoint)

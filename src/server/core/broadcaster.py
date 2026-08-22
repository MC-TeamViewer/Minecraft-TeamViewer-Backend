import asyncio
import logging
import time

from fastapi import WebSocketDisconnect

from ..admin.traffic import send_tracked_websocket_bytes
from ..ws.sender import websocket_send_hub
from .codec import ProtobufMessageCodec
from .protocol import DigestPacket, PatchPacket, RefreshRequestOutboundPacket, ReportRateHintPacket, SnapshotFullPacket
from ..state import ServerState


logger = logging.getLogger("teamviewrelay.broadcaster")


class Broadcaster:
    """
    广播编排层。

    业务职责：
    - 根据客户端能力分流全量/增量消息；
    - 统一执行“清理 -> 仲裁 -> 广播”的周期流程；
    - 为管理端推送实时快照。
    """

    def __init__(self, state: ServerState) -> None:
        self.state = state
        self._codec = ProtobufMessageCodec()
        self._web_map_last_states: dict[str, dict] = {}
        self._player_last_states: dict[str, dict] = {}
        self._resolved_source_views: dict[tuple[str, tuple[str, ...]], dict] = {}
        self._last_player_report_hints: dict[str, int] = {}
        self._last_housekeeping_at = time.monotonic()
        self._last_delivery_signature: tuple | None = None
        self._player_sync_scopes = (
            "players",
            "entities",
            "waypoints",
            "battleChunks",
            "lastSeenPlayers",
        )
        self._web_map_sync_scopes = (
            "players",
            "entities",
            "waypoints",
            "battleChunks",
            "playerMarks",
            "lastSeenPlayers",
        )
        self._player_delivery_scopes = self._player_sync_scopes + ("playerMarks",)

    def _encode_message(self, packet) -> bytes:
        return self._codec.encode(packet)

    async def _encode_message_async(self, packet) -> bytes:
        # Large protobuf population still contains Python work.  Keep HTTP and WS
        # handshakes responsive while the shared payload for a projection is built.
        return await asyncio.to_thread(self._encode_message, packet)

    def _encode_message_once(self, packet, cache: dict[str, bytes], cache_key: str) -> bytes:
        encoded = cache.get(cache_key)
        if encoded is None:
            encoded = self._encode_message(packet)
            cache[cache_key] = encoded
        return encoded

    async def _send_encoded(
        self,
        ws,
        payload: bytes,
        *,
        channel: str,
        protobuf_type: str,
    ) -> bool:
        return await send_tracked_websocket_bytes(
            ws,
            payload,
            channel=channel,
            protobuf_type=protobuf_type,
            coalesce_state=protobuf_type in {"snapshot_full", "patch"},
            wait=False,
        )

    def _build_full_message(
        self,
        scope_state: dict,
        *,
        channel: str | None = None,
        extra: dict | None = None,
    ) -> SnapshotFullPacket:
        message = {**scope_state}
        if channel:
            message["channel"] = channel
        if isinstance(extra, dict) and extra:
            message.update(extra)
        return SnapshotFullPacket(**message)

    def _build_patch_message(
        self,
        scope_patch: dict,
        *,
        channel: str | None = None,
        extra: dict | None = None,
    ) -> PatchPacket:
        message = {**scope_patch}
        if channel:
            message["channel"] = channel
        if isinstance(extra, dict) and extra:
            message.update(extra)
        return PatchPacket(**message)

    @staticmethod
    def _snapshot_scope_from_state_map(state_map: dict) -> dict:
        if not isinstance(state_map, dict):
            return {}
        return {object_id: node.get("data", {}) for object_id, node in state_map.items() if isinstance(node, dict)}

    def _client_visible_scope_from_state_map(self, scope_name: str, state_map: dict) -> dict:
        return self.state.build_player_outbound_digest_scope(
            scope_name,
            self._snapshot_scope_from_state_map(state_map),
        )

    def _build_web_map_view_state(self, web_map_room: str | None = None) -> dict:
        normalized_room = self.state.normalize_room_code(web_map_room)
        allowed_sources = self.state.get_active_sources_in_room(normalized_room)
        player_sources = allowed_sources.intersection(self.state.get_player_connection_ids())
        resolved = self._resolve_source_view(allowed_sources, normalized_room)
        return {
            "players": self._client_visible_scope_from_state_map("players", resolved["players"]),
            "entities": self._client_visible_scope_from_state_map("entities", resolved["entities"]),
            "waypoints": self._client_visible_scope_from_state_map("waypoints", resolved["waypoints"]),
            "battleChunks": self._client_visible_scope_from_state_map("battleChunks", resolved["battleChunks"]),
            "lastSeenPlayers": self._client_visible_scope_from_state_map(
                "lastSeenPlayers", resolved["lastSeenPlayers"]
            ),
            "playerMarks": self.state.prune_none_fields(dict(self.state.player_marks)),
            "tabState": self.state.build_web_map_tab_snapshot(normalized_room),
            "roomCode": normalized_room,
            "connections": sorted(player_sources),
            "connections_count": len(player_sources),
        }

    def _resolve_source_view(self, allowed_sources: set[str], room_code: str) -> dict:
        normalized_room = self.state.normalize_room_code(room_code)
        cache_key = (normalized_room, tuple(sorted(allowed_sources)))
        resolved = self._resolved_source_views.get(cache_key)
        if resolved is None:
            resolved = self.state.resolve_states_for_sources(allowed_sources, normalized_room)
            self._resolved_source_views[cache_key] = resolved
        return resolved

    @staticmethod
    def _wrap_plain_scope(scope_map: dict) -> dict:
        if not isinstance(scope_map, dict):
            return {}
        return {
            object_id: {"data": value}
            for object_id, value in scope_map.items()
        }

    def _compute_scope_patch_for_scopes(self, old_state: dict, new_state: dict, scopes: tuple[str, ...]) -> dict:
        patch = {}
        for scope in scopes:
            scope_patch = self.state.compute_scope_patch(
                self._wrap_plain_scope(old_state.get(scope, {})),
                self._wrap_plain_scope(new_state.get(scope, {})),
                full_replace=(scope in {"battleChunks", "playerMarks", "lastSeenPlayers"}),
            )
            if scope_patch.get("upsert") or scope_patch.get("delete"):
                patch[scope] = scope_patch
        return patch

    @staticmethod
    def _has_scope_patch_changes(patch: dict, scopes: tuple[str, ...]) -> bool:
        for scope in scopes:
            if patch.get(scope, {}).get("upsert") or patch.get(scope, {}).get("delete"):
                return True
        return False

    @staticmethod
    def _patch_requires_full_snapshot(patch: dict, scopes: tuple[str, ...]) -> bool:
        """Return whether this patch contains explicit field removals."""
        for scope in scopes:
            upsert = patch.get(scope, {}).get("upsert")
            if not isinstance(upsert, dict):
                continue
            for value in upsert.values():
                if isinstance(value, dict) and any(item is None for item in value.values()):
                    return True
        return False

    @staticmethod
    def _protocol_supports_clear_fields(protocol_version: str | None) -> bool:
        return ServerState._protocol_at_least(protocol_version, "0.6.5")

    def _split_patch_for_legacy_client(
        self,
        patch: dict,
        current_state: dict,
        scopes: tuple[str, ...],
    ) -> list[dict]:
        """Replace clear_fields with delete + full upsert for pre-0.6.5 clients."""
        if not self._patch_requires_full_snapshot(patch, scopes):
            return [patch]

        delete_phase: dict = {}
        upsert_phase: dict = {}
        for scope in scopes:
            scope_patch = patch.get(scope)
            if not isinstance(scope_patch, dict):
                continue
            upsert = scope_patch.get("upsert") if isinstance(scope_patch.get("upsert"), dict) else {}
            delete_ids = [item for item in scope_patch.get("delete", []) if isinstance(item, str) and item]
            replacement_ids = [
                object_id for object_id, delta in upsert.items()
                if isinstance(delta, dict) and any(value is None for value in delta.values())
            ]
            phase_delete_ids = list(dict.fromkeys(delete_ids + replacement_ids))
            if phase_delete_ids:
                delete_phase[scope] = {"upsert": {}, "delete": phase_delete_ids}

            phase_upsert: dict = {}
            current_scope = current_state.get(scope) if isinstance(current_state.get(scope), dict) else {}
            for object_id, delta in upsert.items():
                if object_id in replacement_ids:
                    current_value = current_scope.get(object_id)
                    if isinstance(current_value, dict):
                        phase_upsert[object_id] = dict(current_value)
                elif isinstance(delta, dict):
                    phase_upsert[object_id] = dict(delta)
            if phase_upsert:
                upsert_phase[scope] = {"upsert": phase_upsert, "delete": []}

        if isinstance(patch.get("meta"), dict) and patch["meta"]:
            upsert_phase["meta"] = dict(patch["meta"])
        return [phase for phase in (delete_phase, upsert_phase) if phase]

    async def _send_compatible_patch(
        self,
        ws,
        patch: dict,
        current_state: dict,
        scopes: tuple[str, ...],
        protocol_version: str | None,
        *,
        channel: str,
        extra: dict | None = None,
    ) -> None:
        phases = (
            [patch]
            if self._protocol_supports_clear_fields(protocol_version)
            else self._split_patch_for_legacy_client(patch, current_state, scopes)
        )
        for phase in phases:
            message = self._build_patch_message(phase, channel=channel if channel == "web_map" else None, extra=extra)
            await self._send_encoded(
                ws,
                await self._encode_message_async(message),
                channel=channel,
                protobuf_type="patch",
            )

    def _compute_web_map_patch(self, old_state: dict, new_state: dict) -> dict:
        scope_patch = self._compute_scope_patch_for_scopes(old_state, new_state, self._web_map_sync_scopes)

        meta_patch = {}
        tab_state_patch = self._compute_tab_state_patch(old_state.get("tabState"), new_state.get("tabState"))
        if tab_state_patch:
            meta_patch["tabStatePatch"] = tab_state_patch
        if old_state.get("connections") != new_state.get("connections"):
            meta_patch["connections"] = new_state.get("connections", [])
            meta_patch["connections_count"] = new_state.get("connections_count", 0)

        if meta_patch:
            scope_patch["meta"] = meta_patch
        return scope_patch

    @staticmethod
    def _compute_tab_state_patch(old_tab_state: dict | None, new_tab_state: dict | None) -> dict:
        old_state = old_tab_state if isinstance(old_tab_state, dict) else {}
        new_state = new_tab_state if isinstance(new_tab_state, dict) else {}

        old_reports = old_state.get("reports") if isinstance(old_state.get("reports"), dict) else {}
        new_reports = new_state.get("reports") if isinstance(new_state.get("reports"), dict) else {}

        upsert_reports = {
            source_id: report
            for source_id, report in new_reports.items()
            if old_reports.get(source_id) != report
        }
        delete_reports = [
            source_id
            for source_id in old_reports.keys()
            if source_id not in new_reports
        ]

        patch: dict[str, object] = {}
        if old_state.get("enabled") != new_state.get("enabled"):
            patch["enabled"] = bool(new_state.get("enabled", False))
        if old_state.get("roomCode") != new_state.get("roomCode"):
            patch["roomCode"] = new_state.get("roomCode")
        if old_state.get("groups") != new_state.get("groups"):
            patch["groups"] = new_state.get("groups", [])
        if upsert_reports:
            patch["upsertReports"] = upsert_reports
        if delete_reports:
            patch["deleteReports"] = delete_reports

        return patch

    def _compact_scope_state(self, node_scope_state: dict, scopes: tuple[str, ...]) -> dict:
        return {
            scope: self.state.build_player_outbound_digest_scope(
                scope,
                ServerState.compact_state_map(node_scope_state.get(scope, {})),
            )
            for scope in scopes
        }

    def _build_global_player_sync_node_state(self) -> dict:
        return {
            "players": self.state.players,
            "entities": self.state.entities,
            "waypoints": self.state.waypoints,
            "battleChunks": self.state.battle_chunks,
            "lastSeenPlayers": {},
        }

    def _build_player_sync_view_state(
        self,
        node_scope_state: dict,
        *,
        include_last_seen: bool = True,
    ) -> dict:
        state = self._compact_scope_state(node_scope_state, self._player_sync_scopes)
        if not include_last_seen:
            state.pop("lastSeenPlayers", None)
        return state

    def _player_supports_last_seen(self, player_id: str) -> bool:
        caps = self.state.connection_caps.get(player_id, {})
        return self.state._protocol_at_least(caps.get("protocol"), "0.6.4")

    def _web_map_supports_last_seen(self, web_map_id: str) -> bool:
        return self.state._protocol_at_least(
            self.state.web_map_connection_protocols.get(web_map_id),
            "0.6.4",
        )

    def _web_map_state_for_client(self, web_map_id: str, state: dict) -> dict:
        if self._web_map_supports_last_seen(web_map_id):
            return state
        return {key: value for key, value in state.items() if key != "lastSeenPlayers"}

    def _build_player_outbound_digest_view(
        self,
        sync_view_state: dict,
        *,
        include_player_source_metadata: bool = True,
    ) -> dict[str, dict]:
        return {
            scope: self.state.build_player_outbound_digest_scope(
                scope,
                sync_view_state.get(scope, {}),
                include_player_source_metadata=include_player_source_metadata,
            )
            for scope in self._player_sync_scopes
        }

    def _build_player_sync_digests(
        self,
        sync_view_state: dict,
        *,
        include_player_source_metadata: bool = True,
    ) -> dict[str, str]:
        digest_view = self._build_player_outbound_digest_view(
            sync_view_state,
            include_player_source_metadata=include_player_source_metadata,
        )
        return {
            scope: self.state.state_digest_plain(digest_view.get(scope, {}))
            for scope in self._player_sync_scopes
            if scope in sync_view_state
        }

    def _has_web_map_patch_changes(self, patch: dict) -> bool:
        return self._has_scope_patch_changes(patch, self._web_map_sync_scopes) or bool(patch.get("meta"))

    def _describe_web_map_socket(self, ws) -> str:
        state_text = self.state.websocket_state_label(ws)
        close_code = getattr(ws, "close_code", None)
        close_reason = getattr(ws, "close_reason", None)
        return (
            f"state=({state_text}), "
            f"closeCode={close_code if close_code is not None else 'unknown'}, "
            f"closeReason={close_reason!r}"
        )

    def _drop_web_map_connection(self, web_map_id: str) -> None:
        if web_map_id in self.state.web_map_connections:
            del self.state.web_map_connections[web_map_id]
        if web_map_id in self.state.web_map_connection_rooms:
            del self.state.web_map_connection_rooms[web_map_id]
        self.state.web_map_connection_protocols.pop(web_map_id, None)
        if web_map_id in self._web_map_last_states:
            del self._web_map_last_states[web_map_id]

    async def send_web_map_snapshot_full(self, web_map_id: str) -> None:
        ws = self.state.web_map_connections.get(web_map_id)
        if ws is None:
            return
        if not self.state.websocket_is_connected(ws):
            logger.info(
                "Skip full web-map snapshot for disconnected client %s (roomCode=%s, %s)",
                web_map_id,
                self.state.get_web_map_room(web_map_id),
                self._describe_web_map_socket(ws),
            )
            self._drop_web_map_connection(web_map_id)
            return

        web_map_room = self.state.get_web_map_room(web_map_id)
        view_state = self._web_map_state_for_client(
            web_map_id,
            self._build_web_map_view_state(web_map_room),
        )
        message = self._build_full_message(
            view_state,
            channel="web_map",
            extra={"server_time": time.time()},
        )

        await self._send_encoded(
            ws,
            await self._encode_message_async(message),
            channel="web_map",
            protobuf_type="snapshot_full",
        )
        self._web_map_last_states[web_map_id] = view_state

    def _build_visible_state_for_player(self, player_id: str) -> dict:
        allowed_sources = self.state.get_allowed_sources_for_player(player_id)
        player_room = self.state.get_player_room(player_id)
        return self._resolve_source_view(allowed_sources, player_room)

    async def send_snapshot_full_to_player(self, player_id: str) -> None:
        """向指定玩家推送完整快照（重同步场景）。"""
        if self.state.is_external_source(player_id):
            return
        self._resolved_source_views.clear()
        ws = self.state.connections.get(player_id)
        if ws is None:
            return
        visible = self._build_visible_state_for_player(player_id)
        supports_last_seen = self._player_supports_last_seen(player_id)
        sync_view_state = self._build_player_sync_view_state(
            visible,
            include_last_seen=supports_last_seen,
        )
        delivery_state = {
            **sync_view_state,
            "playerMarks": self.state.prune_none_fields(dict(self.state.player_marks)),
        }
        message = self._build_full_message(delivery_state)
        await self._send_encoded(
            ws,
            await self._encode_message_async(message),
            channel="player",
            protobuf_type="snapshot_full",
        )
        self._player_last_states[player_id] = delivery_state

    async def maybe_send_digest(
        self,
        player_id: str,
        visible_state: dict | None = None,
        sync_view_state: dict | None = None,
    ) -> None:
        """按节流周期发送摘要，帮助客户端做状态一致性检测。"""
        if self.state.is_external_source(player_id):
            return
        ws = self.state.connections.get(player_id)
        caps = self.state.connection_caps.get(player_id)
        if ws is None or caps is None:
            return

        now = time.monotonic()
        if now - float(caps.get("lastDigestSent", 0.0)) < self.state.DIGEST_INTERVAL_SEC:
            return

        caps["lastDigestSent"] = now
        if sync_view_state is None:
            if visible_state is None:
                visible_state = (
                    self._build_visible_state_for_player(player_id)
                    if self.state.requires_scoped_delivery(player_id)
                    or self._player_supports_last_seen(player_id)
                    else self._build_global_player_sync_node_state()
                )
            sync_view_state = self._build_player_sync_view_state(
                visible_state,
                include_last_seen=self._player_supports_last_seen(player_id),
            )
        hashes = self._build_player_sync_digests(
            sync_view_state,
            include_player_source_metadata=self.state._protocol_at_least(caps.get("protocol"), "0.6.5"),
        )
        logger.debug("Sending player digest player=%s source=outbound_projected hashes=%s", player_id, hashes)
        message = DigestPacket(
            hashes=hashes,
        )
        await self._send_encoded(
            ws,
            await self._encode_message_async(message),
            channel="player",
            protobuf_type="digest",
        )

    async def broadcast_web_map_updates(self, force_full: bool = False) -> None:
        """向网页地图观察端广播增量（必要时全量）。"""
        self._resolved_source_views.clear()
        if not self.state.web_map_connections:
            self._web_map_last_states = {}
            return

        disconnected = []
        room_states: dict[str, dict] = {}
        client_states: dict[tuple[str, bool], dict] = {}
        encoded_full_by_room: dict[tuple[str, bool], bytes] = {}
        encoded_patch_cache: dict[tuple[str, bool, int, bool], tuple[dict, bytes | None]] = {}
        server_time = time.time()
        for web_map_id, ws in list(self.state.web_map_connections.items()):
            web_map_room = self.state.get_web_map_room(web_map_id)
            if not self.state.websocket_is_connected(ws):
                logger.info(
                    "Skip web-map broadcast to disconnected client %s (roomCode=%s, forceFull=%s, %s)",
                    web_map_id,
                    web_map_room,
                    force_full,
                    self._describe_web_map_socket(ws),
                )
                disconnected.append(web_map_id)
                continue
            try:
                room_key = self.state.normalize_room_code(web_map_room)
                room_state = room_states.get(room_key)
                if room_state is None:
                    room_state = self._build_web_map_view_state(web_map_room)
                    room_states[room_key] = room_state
                supports_last_seen = self._web_map_supports_last_seen(web_map_id)
                state_key = (room_key, supports_last_seen)
                current_state = client_states.get(state_key)
                if current_state is None:
                    current_state = self._web_map_state_for_client(web_map_id, room_state)
                    client_states[state_key] = current_state
                previous_state = self._web_map_last_states.get(web_map_id)
                message_kind = "idle"
                protocol_version = self.state.web_map_connection_protocols.get(web_map_id)
                supports_clear = self._protocol_supports_clear_fields(protocol_version)
                must_send_full = force_full or previous_state is None or websocket_send_hub.state_pending(ws)

                patch_state: dict | None = None
                encoded_patch: bytes | None = None
                if not must_send_full:
                    patch_cache_key = (room_key, supports_last_seen, id(previous_state), supports_clear)
                    cached_patch = encoded_patch_cache.get(patch_cache_key)
                    if cached_patch is None:
                        patch_state = self._compute_web_map_patch(previous_state, current_state)
                        if self._has_web_map_patch_changes(patch_state):
                            if not supports_clear and self._patch_requires_full_snapshot(
                                patch_state, self._web_map_sync_scopes
                            ):
                                must_send_full = True
                            else:
                                message = self._build_patch_message(
                                    patch_state,
                                    channel="web_map",
                                    extra={"server_time": server_time},
                                )
                                encoded_patch = await self._encode_message_async(message)
                        encoded_patch_cache[patch_cache_key] = (patch_state, encoded_patch)
                    else:
                        patch_state, encoded_patch = cached_patch

                if must_send_full:
                    message_kind = "snapshot_full"
                    full_cache_key = state_key
                    encoded = encoded_full_by_room.get(full_cache_key)
                    if encoded is None:
                        message = self._build_full_message(
                            current_state,
                            channel="web_map",
                            extra={"server_time": server_time},
                        )
                        encoded = await self._encode_message_async(message)
                        encoded_full_by_room[full_cache_key] = encoded
                    await self._send_encoded(
                        ws,
                        encoded,
                        channel="web_map",
                        protobuf_type="snapshot_full",
                    )
                elif encoded_patch is not None:
                    message_kind = "patch"
                    await self._send_encoded(
                        ws,
                        encoded_patch,
                        channel="web_map",
                        protobuf_type="patch",
                    )

                self._web_map_last_states[web_map_id] = current_state
            except WebSocketDisconnect as e:
                logger.info(
                    "Web-map client disconnected during send %s (webMapId=%s, roomCode=%s, code=%s, %s)",
                    message_kind,
                    web_map_id,
                    web_map_room,
                    getattr(e, "code", None),
                    self._describe_web_map_socket(ws),
                )
                disconnected.append(web_map_id)
            except RuntimeError as e:
                logger.warning(
                    "RuntimeError sending web-map %s to %s (roomCode=%s, forceFull=%s, %s): %s: %r",
                    message_kind,
                    web_map_id,
                    web_map_room,
                    force_full,
                    self._describe_web_map_socket(ws),
                    type(e).__name__,
                    e,
                )
                disconnected.append(web_map_id)
            except Exception as e:
                logger.warning(
                    "Error sending web-map %s to %s (roomCode=%s, forceFull=%s, %s): %s: %r",
                    message_kind,
                    web_map_id,
                    web_map_room,
                    force_full,
                    self._describe_web_map_socket(ws),
                    type(e).__name__,
                    e,
                )
                disconnected.append(web_map_id)

        for web_map_id in disconnected:
            self._drop_web_map_connection(web_map_id)

    async def broadcast_updates(self, force_full_to_delta: bool = False) -> None:
        """统一广播入口：清理超时、计算 patch、按能力下发。"""
        now = time.monotonic()
        if now - self._last_housekeeping_at >= 0.5:
            await self.request_preexpiry_refreshes()
            self.state.cleanup_timeouts()
            self._last_housekeeping_at = now
        self.state.refresh_resolved_states()
        self._resolved_source_views.clear()

        player_topology = tuple(
            sorted(
                (
                    player_id,
                    self.state.get_player_room(player_id),
                    str(self.state.connection_caps.get(player_id, {}).get("protocol") or ""),
                    self.state.is_external_source(player_id),
                )
                for player_id in self.state.connections
            )
        )
        web_map_topology = tuple(
            sorted(
                (
                    web_map_id,
                    self.state.get_web_map_room(web_map_id),
                    str(self.state.web_map_connection_protocols.get(web_map_id) or ""),
                )
                for web_map_id in self.state.web_map_connections
            )
        )
        delivery_signature = (
            self.state.delivery_revision,
            self.state.last_seen_revision,
            self.state.player_marks_revision,
            self.state.tab_reports_revision,
            player_topology,
            web_map_topology,
        )
        digest_due = any(
            not self.state.is_external_source(player_id)
            and player_id in self.state.connection_caps
            and now - float(self.state.connection_caps.get(player_id, {}).get("lastDigestSent", 0.0))
            >= self.state.DIGEST_INTERVAL_SEC
            for player_id in self.state.connections
        )
        if (
            not force_full_to_delta
            and self._last_delivery_signature == delivery_signature
            and not digest_due
        ):
            return
        self._last_delivery_signature = delivery_signature

        disconnected = []
        projection_states: dict[tuple[str, tuple[str, ...], bool], tuple[dict, dict]] = {}
        encoded_full_cache: dict[tuple[str, tuple[str, ...], bool], bytes] = {}
        encoded_patch_cache: dict[tuple[tuple[str, tuple[str, ...], bool], int, bool], tuple[dict, bytes | None]] = {}
        digest_encoded_cache: dict[tuple[tuple[str, tuple[str, ...], bool], bool], bytes] = {}
        for player_id, ws in list(self.state.connections.items()):
            if self.state.is_external_source(player_id):
                continue
            if not self.state.websocket_is_connected(ws):
                logger.debug(
                    f"Skip delta broadcast to disconnected websocket player={player_id} "
                    f"state=({self.state.websocket_state_label(ws)})"
                )
                disconnected.append(player_id)
                continue

            try:
                supports_last_seen = self._player_supports_last_seen(player_id)
                room_code = self.state.get_player_room(player_id)
                allowed_sources = tuple(sorted(self.state.get_allowed_sources_for_player(player_id)))
                projection_key = (
                    self.state.normalize_room_code(room_code),
                    allowed_sources,
                    supports_last_seen,
                )
                projected = projection_states.get(projection_key)
                if projected is None:
                    visible = self._resolve_source_view(set(allowed_sources), room_code)
                    sync_view_state = self._build_player_sync_view_state(
                        visible,
                        include_last_seen=supports_last_seen,
                    )
                    delivery_state = {
                        **sync_view_state,
                        "playerMarks": self.state.prune_none_fields(dict(self.state.player_marks)),
                    }
                    projected = (sync_view_state, delivery_state)
                    projection_states[projection_key] = projected
                else:
                    sync_view_state, delivery_state = projected
                previous_state = self._player_last_states.get(player_id)
                caps = self.state.connection_caps.get(player_id, {})
                supports_clear = self._protocol_supports_clear_fields(caps.get("protocol"))
                must_send_full = (
                    force_full_to_delta
                    or previous_state is None
                    or websocket_send_hub.state_pending(ws)
                )
                encoded_patch: bytes | None = None
                if not must_send_full and previous_state != delivery_state:
                    patch_cache_key = (projection_key, id(previous_state), supports_clear)
                    cached_patch = encoded_patch_cache.get(patch_cache_key)
                    if cached_patch is None:
                        patch_state = self._compute_scope_patch_for_scopes(
                            previous_state,
                            delivery_state,
                            self._player_delivery_scopes,
                        )
                        if self._has_scope_patch_changes(patch_state, self._player_delivery_scopes):
                            if not supports_clear and self._patch_requires_full_snapshot(
                                patch_state, self._player_delivery_scopes
                            ):
                                must_send_full = True
                            else:
                                encoded_patch = await self._encode_message_async(
                                    self._build_patch_message(patch_state)
                                )
                        encoded_patch_cache[patch_cache_key] = (patch_state, encoded_patch)
                    else:
                        _, encoded_patch = cached_patch

                if must_send_full:
                    full_msg = self._build_full_message(delivery_state)
                    encoded_full = encoded_full_cache.get(projection_key)
                    if encoded_full is None:
                        encoded_full = await self._encode_message_async(full_msg)
                        encoded_full_cache[projection_key] = encoded_full
                    await self._send_encoded(
                        ws,
                        encoded_full,
                        channel="player",
                        protobuf_type="snapshot_full",
                    )
                elif encoded_patch is not None:
                    await self._send_encoded(
                        ws,
                        encoded_patch,
                        channel="player",
                        protobuf_type="patch",
                    )
                self._player_last_states[player_id] = delivery_state

                if caps and now - float(caps.get("lastDigestSent", 0.0)) >= self.state.DIGEST_INTERVAL_SEC:
                    caps["lastDigestSent"] = now
                    include_source_metadata = self.state._protocol_at_least(caps.get("protocol"), "0.6.5")
                    digest_key = (projection_key, include_source_metadata)
                    encoded_digest = digest_encoded_cache.get(digest_key)
                    if encoded_digest is None:
                        hashes = self._build_player_sync_digests(
                            sync_view_state,
                            include_player_source_metadata=include_source_metadata,
                        )
                        encoded_digest = await self._encode_message_async(DigestPacket(hashes=hashes))
                        digest_encoded_cache[digest_key] = encoded_digest
                    await self._send_encoded(
                        ws,
                        encoded_digest,
                        channel="player",
                        protobuf_type="digest",
                    )
            except RuntimeError as e:
                logger.warning(
                    f"RuntimeError sending delta update to player={player_id} "
                    f"state=({self.state.websocket_state_label(ws)}) "
                    f"force_full={force_full_to_delta}: {e}"
                )
                disconnected.append(player_id)
            except Exception as e:
                logger.warning(
                    f"Error sending delta update to player={player_id} "
                    f"state=({self.state.websocket_state_label(ws)}) "
                    f"force_full={force_full_to_delta}: {e}"
                )
                disconnected.append(player_id)

        for player_id in disconnected:
            self.state.remove_connection(player_id)
            self._player_last_states.pop(player_id, None)

        await self.broadcast_web_map_updates()

    async def request_preexpiry_refreshes(self) -> None:
        """在对象即将超时前，向对应来源客户端请求该范围内的全量确认。"""
        current_time = time.monotonic()
        refresh_targets = self.state.collect_preexpiry_refresh_requests(current_time)
        if not refresh_targets:
            return

        for source_id, payload in refresh_targets.items():
            await self.send_refresh_request_to_source(
                source_id,
                players=payload.get("players", []),
                entities=payload.get("entities", []),
                battle_chunks=payload.get("battleChunks", []),
                reason="expiry_soon",
                current_time=current_time,
                bypass_cooldown=False,
            )

    async def send_refresh_request_to_source(
        self,
        source_id: str,
        players: list,
        entities: list,
        battle_chunks: list | None,
        reason: str,
        current_time: float | None = None,
        bypass_cooldown: bool = False,
    ) -> None:
        if not isinstance(source_id, str) or not source_id:
            return
        if self.state.is_external_source(source_id):
            return

        players = [item for item in players if isinstance(item, str) and item]
        entities = [item for item in entities if isinstance(item, str) and item]
        battle_chunks = [item for item in (battle_chunks or []) if isinstance(item, str) and item]
        if not players and not entities and not battle_chunks:
            return

        now = time.monotonic() if current_time is None else current_time
        if not bypass_cooldown and not self.state.can_send_refresh_request(source_id, now):
            return

        ws = self.state.connections.get(source_id)
        if ws is None:
            return
        if not self.state.websocket_is_connected(ws):
            self.state.remove_connection(source_id)
            return

        message = RefreshRequestOutboundPacket(
            reason=reason,
            serverTime=time.time(),
            players=players,
            entities=entities,
            battleChunks=battle_chunks,
        )
        try:
            await self._send_encoded(
                ws,
                await self._encode_message_async(message),
                channel="player",
                protobuf_type="refresh_request",
            )
            self.state.mark_refresh_request_sent(source_id, now)
            logger.debug(
                "Sent refresh_req "
                f"source={source_id} players={len(players)} entities={len(entities)} "
                f"battleChunks={len(battle_chunks)} reason={reason}"
            )
        except Exception as e:
            logger.warning(
                f"Error sending refresh_req to source={source_id} "
                f"state=({self.state.websocket_state_label(ws)}): {e}"
            )
            self.state.remove_connection(source_id)

    async def broadcast_report_rate_hints(self, reason: str = "runtime") -> None:
        broadcast_hz = self.state.broadcast_hz
        encoded_cache: dict[tuple[int, float, str | None], bytes] = {}
        for player_id, ws in list(self.state.connections.items()):
            if self.state.is_external_source(player_id):
                continue
            if not self.state.websocket_is_connected(ws):
                continue

            caps = self.state.connection_caps.get(player_id, {})
            suggested_ticks = self.state.negotiate_report_interval_ticks(
                player_id,
                caps.get("preferredReportIntervalTicks"),
                caps.get("minReportIntervalTicks"),
                caps.get("maxReportIntervalTicks"),
            )
            previous_ticks = self._last_player_report_hints.get(player_id)
            if previous_ticks == suggested_ticks:
                continue

            self._last_player_report_hints[player_id] = suggested_ticks
            if isinstance(caps, dict):
                caps["negotiatedReportIntervalTicks"] = suggested_ticks

            packet = ReportRateHintPacket(
                reportIntervalTicks=suggested_ticks,
                broadcastHz=broadcast_hz,
                reason=reason,
            )
            try:
                cache_key = (suggested_ticks, broadcast_hz, reason)
                encoded = encoded_cache.get(cache_key)
                if encoded is None:
                    encoded = await self._encode_message_async(packet)
                    encoded_cache[cache_key] = encoded
                await self._send_encoded(
                    ws,
                    encoded,
                    channel="player",
                    protobuf_type="report_rate_hint",
                )
            except Exception as e:
                logger.warning(
                    "Error sending report_rate_hint to player=%s state=(%s): %s",
                    player_id,
                    self.state.websocket_state_label(ws),
                    e,
                )
                self.state.remove_connection(player_id)

    async def send_admin_snapshot_full(self, admin_id: str) -> None:
        await self.send_web_map_snapshot_full(admin_id)

    async def broadcast_admin_updates(self, force_full: bool = False) -> None:
        await self.broadcast_web_map_updates(force_full)

import logging
import time

from fastapi import WebSocketDisconnect

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
        self._last_player_report_hints: dict[str, int] = {}
        self._player_sync_scopes = ("players", "entities", "waypoints", "battleChunks")
        self._web_map_sync_scopes = ("players", "entities", "waypoints", "battleChunks", "playerMarks")

    def _encode_message(self, packet) -> bytes:
        return self._codec.encode(packet)

    def _encode_message_once(self, packet, cache: dict[str, bytes], cache_key: str) -> bytes:
        encoded = cache.get(cache_key)
        if encoded is None:
            encoded = self._encode_message(packet)
            cache[cache_key] = encoded
        return encoded

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

    def _build_web_map_view_state(self, web_map_room: str | None = None) -> dict:
        normalized_room = self.state.normalize_room_code(web_map_room)
        allowed_sources = self.state.get_active_sources_in_room(normalized_room)
        room_players = self.state.filter_state_map_by_sources(self.state.players, allowed_sources)
        room_entities = self.state.filter_state_map_by_sources(self.state.entities, allowed_sources)
        room_waypoints = self.state.filter_waypoint_state_by_sources_and_room(
            self.state.waypoints,
            allowed_sources,
            normalized_room,
        )
        room_battle_chunks = self.state.filter_battle_chunk_state_by_sources_and_room(
            self.state.battle_chunks,
            allowed_sources,
            normalized_room,
        )
        return {
            "players": self._snapshot_scope_from_state_map(room_players),
            "entities": self._snapshot_scope_from_state_map(room_entities),
            "waypoints": self._snapshot_scope_from_state_map(room_waypoints),
            "battleChunks": self._snapshot_scope_from_state_map(room_battle_chunks),
            "playerMarks": dict(self.state.player_marks),
            "tabState": self.state.build_web_map_tab_snapshot(normalized_room),
            "roomCode": normalized_room,
            "connections": sorted(allowed_sources),
            "connections_count": len(allowed_sources),
        }

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
                full_replace=(scope == "battleChunks"),
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

    @staticmethod
    def _compact_scope_state(node_scope_state: dict, scopes: tuple[str, ...]) -> dict:
        return {
            scope: ServerState.compact_state_map(node_scope_state.get(scope, {}))
            for scope in scopes
        }

    def _build_global_player_sync_node_state(self) -> dict:
        return {
            "players": self.state.players,
            "entities": self.state.entities,
            "waypoints": self.state.waypoints,
            "battleChunks": self.state.battle_chunks,
        }

    def _build_player_sync_view_state(self, node_scope_state: dict) -> dict:
        return self._compact_scope_state(node_scope_state, self._player_sync_scopes)

    def _build_player_outbound_digest_view(self, sync_view_state: dict) -> dict[str, dict]:
        return {
            scope: self.state.build_player_outbound_digest_scope(scope, sync_view_state.get(scope, {}))
            for scope in self._player_sync_scopes
        }

    def _build_player_sync_digests(self, sync_view_state: dict) -> dict[str, str]:
        digest_view = self._build_player_outbound_digest_view(sync_view_state)
        return {
            "players": self.state.state_digest_plain(digest_view.get("players", {})),
            "entities": self.state.state_digest_plain(digest_view.get("entities", {})),
            "waypoints": self.state.state_digest_plain(digest_view.get("waypoints", {})),
            "battleChunks": self.state.state_digest_plain(digest_view.get("battleChunks", {})),
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
        view_state = self._build_web_map_view_state(web_map_room)
        message = self._build_full_message(
            view_state,
            channel="web_map",
            extra={"server_time": time.time()},
        )

        await ws.send_bytes(self._encode_message(message))
        self._web_map_last_states[web_map_id] = view_state

    def _build_visible_state_for_player(self, player_id: str) -> dict:
        allowed_sources = self.state.get_allowed_sources_for_player(player_id)
        player_room = self.state.get_player_room(player_id)
        visible_players = self.state.filter_state_map_by_sources(self.state.players, allowed_sources)
        visible_entities = self.state.filter_state_map_by_sources(self.state.entities, allowed_sources)
        visible_waypoints = self.state.filter_waypoint_state_by_sources_and_room(
            self.state.waypoints,
            allowed_sources,
            player_room,
        )
        visible_battle_chunks = self.state.filter_battle_chunk_state_by_sources_and_room(
            self.state.battle_chunks,
            allowed_sources,
            player_room,
        )
        return {
            "players": visible_players,
            "entities": visible_entities,
            "waypoints": visible_waypoints,
            "battleChunks": visible_battle_chunks,
        }

    async def send_snapshot_full_to_player(self, player_id: str) -> None:
        """向指定玩家推送完整快照（重同步场景）。"""
        ws = self.state.connections.get(player_id)
        if ws is None:
            return
        visible = self._build_visible_state_for_player(player_id)
        sync_view_state = self._build_player_sync_view_state(visible)
        sync_view_state["playerMarks"] = dict(self.state.player_marks)
        message = self._build_full_message(sync_view_state)
        await ws.send_bytes(self._encode_message(message))

    async def maybe_send_digest(self, player_id: str, visible_state: dict | None = None) -> None:
        """按节流周期发送摘要，帮助客户端做状态一致性检测。"""
        ws = self.state.connections.get(player_id)
        caps = self.state.connection_caps.get(player_id)
        if ws is None or caps is None:
            return

        now = time.time()
        if now - float(caps.get("lastDigestSent", 0.0)) < self.state.DIGEST_INTERVAL_SEC:
            return

        caps["lastDigestSent"] = now
        if visible_state is None:
            visible_state = (
                self._build_visible_state_for_player(player_id)
                if self.state.requires_scoped_delivery(player_id)
                else self._build_global_player_sync_node_state()
            )
        sync_view_state = self._build_player_sync_view_state(visible_state)
        hashes = self._build_player_sync_digests(sync_view_state)
        logger.debug("Sending player digest player=%s source=outbound_projected hashes=%s", player_id, hashes)
        message = DigestPacket(
            hashes=hashes,
        )
        await ws.send_bytes(self._encode_message(message))

    async def broadcast_web_map_updates(self, force_full: bool = False) -> None:
        """向网页地图观察端广播增量（必要时全量）。"""
        if not self.state.web_map_connections:
            self._web_map_last_states = {}
            return

        disconnected = []
        room_states: dict[str, dict] = {}
        encoded_full_by_room: dict[str, bytes] = {}
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
                current_state = room_states.get(room_key)
                if current_state is None:
                    current_state = self._build_web_map_view_state(web_map_room)
                    room_states[room_key] = current_state
                previous_state = self._web_map_last_states.get(web_map_id)
                message_kind = "idle"

                if force_full or previous_state is None:
                    message_kind = "snapshot_full"
                    encoded = encoded_full_by_room.get(room_key)
                    if encoded is None:
                        message = self._build_full_message(
                            current_state,
                            channel="web_map",
                            extra={"server_time": time.time()},
                        )
                        encoded = self._encode_message(message)
                        encoded_full_by_room[room_key] = encoded
                    await ws.send_bytes(encoded)
                else:
                    patch_state = self._compute_web_map_patch(previous_state, current_state)
                    if self._has_web_map_patch_changes(patch_state):
                        message_kind = "patch"
                        message = self._build_patch_message(
                            patch_state,
                            channel="web_map",
                            extra={"server_time": time.time()},
                        )
                        await ws.send_bytes(self._encode_message(message))

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
        await self.request_preexpiry_refreshes()
        self.state.cleanup_timeouts()
        changes = self.state.refresh_resolved_states()

        changed = self.state.has_patch_changes(changes)
        encoded_cache: dict[str, bytes] = {}

        disconnected = []
        for player_id, ws in list(self.state.connections.items()):
            if not self.state.websocket_is_connected(ws):
                logger.debug(
                    f"Skip delta broadcast to disconnected websocket player={player_id} "
                    f"state=({self.state.websocket_state_label(ws)}) changed={changed}"
                )
                disconnected.append(player_id)
                continue

            try:
                requires_scoped = self.state.requires_scoped_delivery(player_id)
                if requires_scoped:
                    visible = self._build_visible_state_for_player(player_id)
                    if force_full_to_delta or changed:
                        sync_view_state = self._build_player_sync_view_state(visible)
                        sync_view_state["playerMarks"] = dict(self.state.player_marks)
                        full_msg = self._build_full_message(sync_view_state)
                        await ws.send_bytes(self._encode_message(full_msg))
                    await self.maybe_send_digest(player_id, visible)
                elif force_full_to_delta:
                    sync_view_state = self._build_player_sync_view_state(self._build_global_player_sync_node_state())
                    sync_view_state["playerMarks"] = dict(self.state.player_marks)
                    full_msg = self._build_full_message(sync_view_state)
                    encoded = self._encode_message_once(full_msg, encoded_cache, "global_player_full")
                    await ws.send_bytes(encoded)
                elif changed:
                    patch_state = {
                        "players": changes["players"],
                        "entities": changes["entities"],
                        "waypoints": changes["waypoints"],
                        "battleChunks": changes["battleChunks"],
                    }
                    patch_msg = self._build_patch_message(patch_state)
                    encoded = self._encode_message_once(patch_msg, encoded_cache, "global_player_patch")
                    await ws.send_bytes(encoded)

                if not requires_scoped:
                    await self.maybe_send_digest(player_id)
            except RuntimeError as e:
                logger.warning(
                    f"RuntimeError sending delta update to player={player_id} "
                    f"state=({self.state.websocket_state_label(ws)}) changed={changed} "
                    f"force_full={force_full_to_delta}: {e}"
                )
                disconnected.append(player_id)
            except Exception as e:
                logger.warning(
                    f"Error sending delta update to player={player_id} "
                    f"state=({self.state.websocket_state_label(ws)}) changed={changed} "
                    f"force_full={force_full_to_delta}: {e}"
                )
                disconnected.append(player_id)

        for player_id in disconnected:
            self.state.remove_connection(player_id)

        await self.broadcast_web_map_updates()

    async def request_preexpiry_refreshes(self) -> None:
        """在对象即将超时前，向对应来源客户端请求该范围内的全量确认。"""
        current_time = time.time()
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

        players = [item for item in players if isinstance(item, str) and item]
        entities = [item for item in entities if isinstance(item, str) and item]
        battle_chunks = [item for item in (battle_chunks or []) if isinstance(item, str) and item]
        if not players and not entities and not battle_chunks:
            return

        now = time.time() if current_time is None else current_time
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
            serverTime=now,
            players=players,
            entities=entities,
            battleChunks=battle_chunks,
        )
        try:
            await ws.send_bytes(self._encode_message(message))
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
                    encoded = self._encode_message(packet)
                    encoded_cache[cache_key] = encoded
                await ws.send_bytes(encoded)
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

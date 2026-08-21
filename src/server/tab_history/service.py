from __future__ import annotations

from typing import Any, Awaitable, Callable

from ..app import runtime
from ..core.protocol import (
    HandshakeHelpers,
    TabHistoryLookupRequestPacket,
    TabHistorySubscribePacket,
    TabHistorySyncRequestPacket,
)

SendPacket = Callable[..., Awaitable[None]]


def capabilities() -> dict[str, Any]:
    state = runtime.state
    return {
        "supported": bool(state.TAB_HISTORY_ENABLED),
        "syncModes": ["TAB_HISTORY_SYNC_MODE_FULL", "TAB_HISTORY_SYNC_MODE_DELTA"],
        "defaultChunkEntries": state.TAB_HISTORY_DEFAULT_CHUNK_ENTRIES,
        "maxChunkEntries": state.TAB_HISTORY_MAX_CHUNK_ENTRIES,
        "maxChunkBytes": state.TAB_HISTORY_MAX_CHUNK_BYTES,
        "maxLookupSelectors": state.TAB_HISTORY_MAX_LOOKUP_SELECTORS,
        "retentionDays": state.TAB_HISTORY_RETENTION_DAYS,
        "deltaRetentionDays": state.TAB_HISTORY_DELTA_RETENTION_DAYS,
        "maxFormattedTextSpans": 64,
        "maxFormattedTextUtf8Bytes": 4096,
    }


def supported_for_protocol(protocol_version: str | None) -> bool:
    return bool(runtime.state.TAB_HISTORY_ENABLED) and HandshakeHelpers.protocol_at_least(protocol_version, "0.7.0")


def _chunk_limit(requested: int | None) -> int:
    default = runtime.state.TAB_HISTORY_DEFAULT_CHUNK_ENTRIES
    maximum = runtime.state.TAB_HISTORY_MAX_CHUNK_ENTRIES
    if requested is None:
        return default
    return max(1, min(int(requested), maximum))


async def _send_sized_chunks(
    websocket,
    send_packet: SendPacket,
    *,
    channel: str,
    packet_type: str,
    base: dict[str, Any],
    items: list[Any],
    item_field: str,
    max_entries: int,
) -> None:
    groups: list[list[Any]] = []
    remaining = list(items)
    if not remaining:
        groups.append([])
    while remaining:
        candidate = remaining[:max_entries]
        while len(candidate) > 1:
            probe = {
                "type": packet_type,
                "channel": channel,
                **base,
                item_field: candidate,
                "chunkIndex": 0,
                "chunkCount": 1,
                "final": True,
            }
            if len(runtime.message_codec.encode(probe)) <= runtime.state.TAB_HISTORY_MAX_CHUNK_BYTES:
                break
            candidate = candidate[: max(1, len(candidate) // 2)]
        groups.append(candidate)
        remaining = remaining[len(candidate) :]

    chunk_count = len(groups)
    for index, group in enumerate(groups):
        packet = {
            "type": packet_type,
            **base,
            item_field: group,
            "chunkIndex": index,
            "chunkCount": chunk_count,
            "final": index + 1 == chunk_count,
        }
        await send_packet(websocket, packet, channel=channel)


async def send_digest(websocket, send_packet: SendPacket, *, room_code: str, channel: str) -> None:
    if runtime.tab_history_store is None:
        return
    head = await runtime.tab_history_store.head(room_code)
    await send_packet(websocket, {"type": "tab_history_digest", "head": head}, channel=channel)


async def broadcast_digest(room_code: str, send_packet: SendPacket) -> None:
    stale: list[tuple[str, str]] = []
    for key, subscription in list(runtime.tab_history_subscriptions.items()):
        if subscription.get("roomCode") != room_code:
            continue
        try:
            await send_digest(
                subscription["websocket"],
                send_packet,
                room_code=room_code,
                channel=str(subscription["channel"]),
            )
        except Exception:
            stale.append(key)
    for key in stale:
        runtime.tab_history_subscriptions.pop(key, None)


async def handle_packet(
    packet,
    websocket,
    send_packet: SendPacket,
    *,
    connection_key: tuple[str, str],
    room_code: str,
    channel: str,
) -> bool:
    if not isinstance(
        packet,
        (TabHistorySubscribePacket, TabHistorySyncRequestPacket, TabHistoryLookupRequestPacket),
    ):
        return False
    if runtime.tab_history_store is None or not runtime.state.TAB_HISTORY_ENABLED:
        await _send_error(packet, websocket, send_packet, channel, "TAB_HISTORY_ERROR_CODE_UNSUPPORTED")
        return True

    if isinstance(packet, TabHistorySubscribePacket):
        if packet.enabled:
            runtime.tab_history_subscriptions[connection_key] = {
                "websocket": websocket,
                "roomCode": room_code,
                "channel": channel,
            }
            await send_digest(websocket, send_packet, room_code=room_code, channel=channel)
        else:
            runtime.tab_history_subscriptions.pop(connection_key, None)
        return True

    request_id = str(packet.requestId or "").strip()[:128]
    if not request_id:
        await _send_error(packet, websocket, send_packet, channel, "TAB_HISTORY_ERROR_CODE_INVALID_REQUEST")
        return True

    if isinstance(packet, TabHistorySyncRequestPacket):
        result = await runtime.tab_history_store.sync(
            room_code,
            preferred_mode=str(packet.preferredMode),
            base_revision=packet.baseRevision,
            base_digest=packet.baseDigestSha256,
            allow_full_fallback=packet.allowFullFallback,
        )
        tagged_items = [
            {"entry": entry} for entry in result["upsert"]
        ] + [{"deleteUuid": player_uuid} for player_uuid in result["deleteUuids"]]
        # The temporary tagged representation keeps a single stable ordering;
        # convert each group into the two protobuf repeated fields below.
        await _send_sync_chunks(
            websocket,
            send_packet,
            channel=channel,
            request_id=request_id,
            result=result,
            tagged_items=tagged_items,
            max_entries=_chunk_limit(packet.maxChunkEntries),
        )
        return True

    selectors = list(packet.selectors)
    if not selectors or len(selectors) > runtime.state.TAB_HISTORY_MAX_LOOKUP_SELECTORS:
        code = (
            "TAB_HISTORY_ERROR_CODE_TOO_MANY_SELECTORS"
            if len(selectors) > runtime.state.TAB_HISTORY_MAX_LOOKUP_SELECTORS
            else "TAB_HISTORY_ERROR_CODE_INVALID_REQUEST"
        )
        await _send_error(packet, websocket, send_packet, channel, code)
        return True
    head, results = await runtime.tab_history_store.lookup(room_code, selectors)
    await _send_sized_chunks(
        websocket,
        send_packet,
        channel=channel,
        packet_type="tab_history_lookup_chunk",
        base={"requestId": request_id, "head": head},
        items=results,
        item_field="results",
        max_entries=_chunk_limit(packet.maxChunkEntries),
    )
    return True


async def _send_sync_chunks(
    websocket,
    send_packet: SendPacket,
    *,
    channel: str,
    request_id: str,
    result: dict[str, Any],
    tagged_items: list[dict[str, Any]],
    max_entries: int,
) -> None:
    groups = [tagged_items[index : index + max_entries] for index in range(0, len(tagged_items), max_entries)] or [[]]
    # Split again when rich formatted text makes the encoded protobuf exceed
    # the negotiated server byte ceiling.
    index = 0
    while index < len(groups):
        group = groups[index]
        upsert = [item["entry"] for item in group if "entry" in item]
        deletes = [item["deleteUuid"] for item in group if "deleteUuid" in item]
        probe = {
            "type": "tab_history_sync_chunk",
            "channel": channel,
            "requestId": request_id,
            "mode": result["mode"],
            "head": result["head"],
            "upsert": upsert,
            "deleteUuids": deletes,
            "chunkIndex": 0,
            "chunkCount": 1,
            "final": True,
            "resetReason": result.get("resetReason"),
        }
        if len(group) > 1 and len(runtime.message_codec.encode(probe)) > runtime.state.TAB_HISTORY_MAX_CHUNK_BYTES:
            split_at = max(1, len(group) // 2)
            groups[index : index + 1] = [group[:split_at], group[split_at:]]
            continue
        index += 1

    for index, group in enumerate(groups):
        await send_packet(
            websocket,
            {
                "type": "tab_history_sync_chunk",
                "requestId": request_id,
                "mode": result["mode"],
                "head": result["head"],
                "upsert": [item["entry"] for item in group if "entry" in item],
                "deleteUuids": [item["deleteUuid"] for item in group if "deleteUuid" in item],
                "chunkIndex": index,
                "chunkCount": len(groups),
                "final": index + 1 == len(groups),
                "resetReason": result.get("resetReason"),
            },
            channel=channel,
        )


async def _send_error(packet, websocket, send_packet: SendPacket, channel: str, code: str) -> None:
    request_id = str(getattr(packet, "requestId", "") or "")[:128]
    if isinstance(packet, TabHistoryLookupRequestPacket):
        await send_packet(
            websocket,
            {
                "type": "tab_history_lookup_chunk",
                "requestId": request_id,
                "chunkIndex": 0,
                "chunkCount": 1,
                "final": True,
                "errorCode": code,
                "errorDetail": code,
            },
            channel=channel,
        )
    elif isinstance(packet, TabHistorySyncRequestPacket):
        await send_packet(
            websocket,
            {
                "type": "tab_history_sync_chunk",
                "requestId": request_id,
                "chunkIndex": 0,
                "chunkCount": 1,
                "final": True,
                "errorCode": code,
                "errorDetail": code,
            },
            channel=channel,
        )

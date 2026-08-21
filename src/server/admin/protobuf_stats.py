from __future__ import annotations

import time
from collections import defaultdict
from dataclasses import dataclass
from typing import Any, Callable


SNAPSHOT_FULL_MESSAGE_TYPE = "snapshot_full"


@dataclass(slots=True)
class _PacketTotals:
    message_count: int = 0
    byte_count: int = 0
    max_packet_bytes: int = 0
    last_sent_at: float | None = None

    def add(self, byte_count: int, occurred_at: float) -> None:
        self.message_count += 1
        self.byte_count += byte_count
        self.max_packet_bytes = max(self.max_packet_bytes, byte_count)
        self.last_sent_at = occurred_at


@dataclass(slots=True)
class _LivePacketTotals:
    message_count: int = 0
    byte_count: int = 0

    def add(self, byte_count: int) -> None:
        self.message_count += 1
        self.byte_count += byte_count


class ProtobufStatsService:
    """Aggregate successful outgoing Protobuf sends for the admin dashboard.

    Counters are intentionally in-memory: session totals reset at process restart and
    per-connection data is discarded once that socket is no longer active.
    """

    def __init__(
        self,
        *,
        resolve_active_connections: Callable[[], list[dict[str, Any]]],
        live_window_sec: int = 10,
    ) -> None:
        self._resolve_active_connections = resolve_active_connections
        self._live_window_sec = max(1, int(live_window_sec))
        self._live_buckets: dict[int, dict[tuple[int, str], _LivePacketTotals]] = {}
        self._session_by_type: dict[str, _PacketTotals] = defaultdict(_PacketTotals)
        self._session_by_connection: dict[int, dict[str, _PacketTotals]] = {}

    @property
    def live_window_sec(self) -> int:
        return self._live_window_sec

    def record_nowait(
        self,
        *,
        websocket: object,
        message_type: str | None,
        byte_count: int,
        occurred_at: float | None = None,
    ) -> None:
        normalized_type = str(message_type or "").strip()
        amount = max(0, int(byte_count))
        if not normalized_type or amount <= 0:
            return

        stamp = time.time() if occurred_at is None else float(occurred_at)
        websocket_key = id(websocket)
        self._session_by_type[normalized_type].add(amount, stamp)
        connection_totals = self._session_by_connection.setdefault(websocket_key, {})
        connection_totals.setdefault(normalized_type, _PacketTotals()).add(amount, stamp)

        second_bucket = int(stamp)
        bucket = self._live_buckets.setdefault(second_bucket, {})
        bucket.setdefault((websocket_key, normalized_type), _LivePacketTotals()).add(amount)
        self._prune_live_buckets(second_bucket)

    async def build_live_payload(self) -> dict[str, object]:
        now_sec = int(time.time())
        self._prune_live_buckets(now_sec)
        active_connections = self._resolve_active_connections()
        active_websocket_keys = {
            id(item.get("websocket"))
            for item in active_connections
            if isinstance(item, dict) and item.get("websocket") is not None
        }
        self._prune_inactive_connections(active_websocket_keys)

        live_by_connection = self._build_live_totals(now_sec)
        live_by_type = self._build_live_type_totals(live_by_connection)
        session_total = self._combine_totals(self._session_by_type.values())
        live_total = self._combine_live_totals(live_by_type.values())

        connections: list[dict[str, object]] = []
        for target in active_connections:
            if not isinstance(target, dict):
                continue
            websocket = target.get("websocket")
            if websocket is None:
                continue
            websocket_key = id(websocket)
            session_by_type = self._session_by_connection.get(websocket_key, {})
            live_by_type_for_connection = live_by_connection.get(websocket_key, {})
            connections.append(
                {
                    "actorId": str(target.get("actorId") or ""),
                    "channel": str(target.get("channel") or ""),
                    "total": self._build_metric_payload(
                        self._combine_totals(session_by_type.values()),
                        self._combine_live_totals(live_by_type_for_connection.values()),
                    ),
                    "snapshotFull": self._build_metric_payload(
                        session_by_type.get(SNAPSHOT_FULL_MESSAGE_TYPE),
                        live_by_type_for_connection.get(SNAPSHOT_FULL_MESSAGE_TYPE),
                    ),
                    "messageTypes": self._build_message_type_payload(
                        session_by_type,
                        live_by_type_for_connection,
                    ),
                }
            )

        connections.sort(key=lambda item: (str(item["channel"]), str(item["actorId"])))
        return {
            "sampleWindowSec": self._live_window_sec,
            "total": self._build_metric_payload(session_total, live_total),
            "snapshotFull": self._build_metric_payload(
                self._session_by_type.get(SNAPSHOT_FULL_MESSAGE_TYPE),
                live_by_type.get(SNAPSHOT_FULL_MESSAGE_TYPE),
            ),
            "messageTypes": self._build_message_type_payload(self._session_by_type, live_by_type),
            "connections": connections,
        }

    def _prune_live_buckets(self, current_second: int) -> None:
        threshold = current_second - self._live_window_sec - 2
        stale = [bucket for bucket in self._live_buckets if bucket < threshold]
        for bucket in stale:
            self._live_buckets.pop(bucket, None)

    def _prune_inactive_connections(self, active_websocket_keys: set[int]) -> None:
        stale = [websocket_key for websocket_key in self._session_by_connection if websocket_key not in active_websocket_keys]
        for websocket_key in stale:
            self._session_by_connection.pop(websocket_key, None)
        for bucket in self._live_buckets.values():
            for key in [key for key in bucket if key[0] not in active_websocket_keys]:
                bucket.pop(key, None)

    def _build_live_totals(self, now_sec: int) -> dict[int, dict[str, _LivePacketTotals]]:
        threshold = now_sec - self._live_window_sec + 1
        result: dict[int, dict[str, _LivePacketTotals]] = {}
        for bucket_second, bucket in self._live_buckets.items():
            if bucket_second < threshold:
                continue
            for (websocket_key, message_type), value in bucket.items():
                by_type = result.setdefault(websocket_key, {})
                by_type.setdefault(message_type, _LivePacketTotals()).message_count += value.message_count
                by_type[message_type].byte_count += value.byte_count
        return result

    @staticmethod
    def _build_live_type_totals(
        live_by_connection: dict[int, dict[str, _LivePacketTotals]],
    ) -> dict[str, _LivePacketTotals]:
        result: dict[str, _LivePacketTotals] = {}
        for by_type in live_by_connection.values():
            for message_type, value in by_type.items():
                target = result.setdefault(message_type, _LivePacketTotals())
                target.message_count += value.message_count
                target.byte_count += value.byte_count
        return result

    @staticmethod
    def _combine_totals(values) -> _PacketTotals:
        combined = _PacketTotals()
        for value in values:
            combined.message_count += value.message_count
            combined.byte_count += value.byte_count
            combined.max_packet_bytes = max(combined.max_packet_bytes, value.max_packet_bytes)
            if value.last_sent_at is not None:
                combined.last_sent_at = max(combined.last_sent_at or value.last_sent_at, value.last_sent_at)
        return combined

    @staticmethod
    def _combine_live_totals(values) -> _LivePacketTotals:
        combined = _LivePacketTotals()
        for value in values:
            combined.message_count += value.message_count
            combined.byte_count += value.byte_count
        return combined

    def _build_metric_payload(
        self,
        session: _PacketTotals | None,
        live: _LivePacketTotals | None,
    ) -> dict[str, int | float | None]:
        session_value = session or _PacketTotals()
        live_value = live or _LivePacketTotals()
        window = float(self._live_window_sec)
        return {
            "messageCount": session_value.message_count,
            "messagesPerSecond": live_value.message_count / window,
            "byteCount": session_value.byte_count,
            "bytesPerSecond": live_value.byte_count / window,
            "maxPacketBytes": session_value.max_packet_bytes,
            "lastSentAt": session_value.last_sent_at,
        }

    def _build_message_type_payload(
        self,
        session_by_type: dict[str, _PacketTotals],
        live_by_type: dict[str, _LivePacketTotals],
    ) -> list[dict[str, int | float | str | None]]:
        message_types = set(session_by_type) | set(live_by_type)
        items = [
            {
                "messageType": message_type,
                **self._build_metric_payload(
                    session_by_type.get(message_type),
                    live_by_type.get(message_type),
                ),
            }
            for message_type in message_types
        ]
        return sorted(
            items,
            key=lambda item: (
                item["messageType"] != SNAPSHOT_FULL_MESSAGE_TYPE,
                -int(item["byteCount"]),
                str(item["messageType"]),
            ),
        )


def record_protobuf_packet_nowait(
    *,
    websocket: object,
    message_type: str | None,
    byte_count: int,
) -> None:
    """Record a successful protobuf send and debounce admin refresh notifications."""
    if not isinstance(message_type, str) or not message_type.strip():
        return

    from ..app import runtime

    service = runtime.admin_protobuf_stats_service
    if service is None:
        return
    service.record_nowait(
        websocket=websocket,
        message_type=message_type,
        byte_count=byte_count,
    )
    if runtime.admin_payload_service is not None:
        runtime.admin_payload_service.invalidate("protobuf_traffic")
    runtime.admin_sse_hub.schedule_broadcast("protobuf_traffic", delay_sec=1.0)

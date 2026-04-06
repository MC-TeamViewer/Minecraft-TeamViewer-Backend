from __future__ import annotations

import asyncio
import time
from collections import defaultdict
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .store import AdminStore


TRAFFIC_CHANNELS = ("player", "web_map")
TRAFFIC_DIRECTIONS = ("ingress", "egress")


def _normalize_payload_size(payload: bytes | bytearray | memoryview | str) -> int:
    if isinstance(payload, str):
        return len(payload.encode("utf-8"))
    if isinstance(payload, memoryview):
        return payload.nbytes
    return len(payload)


def infer_websocket_traffic_channel(websocket, explicit_channel: str | None = None) -> str | None:
    if explicit_channel in TRAFFIC_CHANNELS:
        return explicit_channel

    url = getattr(websocket, "url", None)
    path = str(getattr(url, "path", "") or "")
    if path in {"/mc-client", "/playeresp"}:
        return "player"
    if path in {"/web-map/ws", "/adminws"}:
        return "web_map"
    return None


class TrafficStatsService:
    def __init__(
        self,
        *,
        admin_store: AdminStore,
        live_window_sec: int = 10,
    ) -> None:
        self._store = admin_store
        self._live_window_sec = max(1, int(live_window_sec))
        self._lock = asyncio.Lock()
        self._live_buckets: dict[int, dict[tuple[str, str], int]] = {}
        self._pending_minute: dict[tuple[str, str, str], int] = defaultdict(int)
        self._pending_hourly: dict[tuple[str, str, str], int] = defaultdict(int)
        self._pending_daily: dict[tuple[str, str, str], int] = defaultdict(int)

    @property
    def live_window_sec(self) -> int:
        return self._live_window_sec

    async def record(
        self,
        *,
        channel: str,
        direction: str,
        byte_count: int,
        occurred_at: float | None = None,
    ) -> None:
        if channel not in TRAFFIC_CHANNELS or direction not in TRAFFIC_DIRECTIONS:
            return
        amount = max(0, int(byte_count))
        if amount <= 0:
            return

        stamp = time.time() if occurred_at is None else float(occurred_at)
        second_bucket = int(stamp)
        local_dt = self._store.local_datetime(stamp)
        minute_bucket = local_dt.strftime("%Y-%m-%dT%H:%M:00")
        hourly_bucket = local_dt.strftime("%Y-%m-%dT%H:00:00")
        daily_bucket = local_dt.strftime("%Y-%m-%d")

        async with self._lock:
            live_bucket = self._live_buckets.get(second_bucket)
            if live_bucket is None:
                live_bucket = defaultdict(int)
                self._live_buckets[second_bucket] = live_bucket
            live_bucket[(channel, direction)] += amount
            self._prune_live_buckets_locked(second_bucket)
            self._pending_minute[(minute_bucket, channel, direction)] += amount
            self._pending_hourly[(hourly_bucket, channel, direction)] += amount
            self._pending_daily[(daily_bucket, channel, direction)] += amount

    async def build_live_payload(self) -> dict[str, float | int]:
        now_sec = int(time.time())
        async with self._lock:
            self._prune_live_buckets_locked(now_sec)
            totals: dict[tuple[str, str], int] = defaultdict(int)
            threshold = now_sec - self._live_window_sec + 1
            for bucket, series in self._live_buckets.items():
                if bucket < threshold:
                    continue
                for key, value in series.items():
                    totals[key] += int(value)

        return self._build_live_payload_from_totals(totals)

    async def flush_pending(self) -> bool:
        async with self._lock:
            minute = dict(self._pending_minute)
            hourly = dict(self._pending_hourly)
            daily = dict(self._pending_daily)
            self._pending_minute.clear()
            self._pending_hourly.clear()
            self._pending_daily.clear()

        if not minute and not hourly and not daily:
            return False

        await self._store.apply_traffic_increments(
            minute_increments=minute,
            hourly_increments=hourly,
            daily_increments=daily,
        )
        return True

    def _prune_live_buckets_locked(self, current_second: int) -> None:
        threshold = current_second - self._live_window_sec - 2
        stale_keys = [bucket for bucket in self._live_buckets.keys() if bucket < threshold]
        for bucket in stale_keys:
            self._live_buckets.pop(bucket, None)

    def _build_live_payload_from_totals(self, totals: dict[tuple[str, str], int]) -> dict[str, float | int]:
        window = float(self._live_window_sec)

        def value(channel: str, direction: str) -> float:
            return float(totals.get((channel, direction), 0)) / window

        player_ingress = value("player", "ingress")
        player_egress = value("player", "egress")
        web_map_ingress = value("web_map", "ingress")
        web_map_egress = value("web_map", "egress")
        return {
            "sampleWindowSec": self._live_window_sec,
            "playerIngressBps": player_ingress,
            "playerEgressBps": player_egress,
            "webMapIngressBps": web_map_ingress,
            "webMapEgressBps": web_map_egress,
            "totalIngressBps": player_ingress + web_map_ingress,
            "totalEgressBps": player_egress + web_map_egress,
        }


async def record_websocket_traffic(
    *,
    channel: str,
    direction: str,
    byte_count: int,
    occurred_at: float | None = None,
) -> None:
    if channel not in TRAFFIC_CHANNELS or direction not in TRAFFIC_DIRECTIONS:
        return

    from ..app import runtime

    service = runtime.admin_traffic_service
    if service is None:
        return

    await service.record(
        channel=channel,
        direction=direction,
        byte_count=byte_count,
        occurred_at=occurred_at,
    )
    if runtime.admin_payload_service is not None:
        runtime.admin_payload_service.invalidate("live_traffic", "hourly_traffic", "daily_traffic", "traffic_history")
    runtime.admin_sse_hub.schedule_broadcast("traffic_live", delay_sec=1.0)
    runtime.admin_sse_hub.schedule_broadcast("traffic_history", delay_sec=5.0)


async def record_websocket_payload_traffic(
    *,
    websocket,
    direction: str,
    payload: bytes | bytearray | memoryview | str,
    channel: str | None = None,
    occurred_at: float | None = None,
) -> None:
    resolved_channel = infer_websocket_traffic_channel(websocket, channel)
    if resolved_channel is None:
        return
    await record_websocket_traffic(
        channel=resolved_channel,
        direction=direction,
        byte_count=_normalize_payload_size(payload),
        occurred_at=occurred_at,
    )


async def send_tracked_websocket_bytes(websocket, payload: bytes, *, channel: str | None = None) -> None:
    await websocket.send_bytes(payload)
    await record_websocket_payload_traffic(
        websocket=websocket,
        direction="egress",
        payload=payload,
        channel=channel,
    )

from __future__ import annotations

import asyncio
import time
from collections import defaultdict
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .store import AdminStore


TRAFFIC_CHANNELS = ("player", "web_map")
TRAFFIC_DIRECTIONS = ("ingress", "egress")
TRAFFIC_LAYERS = ("application", "wire")
DEFAULT_TRAFFIC_LAYER = "application"


def _normalize_payload_size(payload: bytes | bytearray | memoryview | str) -> int:
    if isinstance(payload, str):
        return len(payload.encode("utf-8"))
    if isinstance(payload, memoryview):
        return payload.nbytes
    return len(payload)


def infer_traffic_channel_from_path(path: str | None) -> str | None:
    normalized_path = str(path or "")
    if normalized_path in {"/mc-client", "/playeresp"}:
        return "player"
    if normalized_path in {"/web-map/ws", "/adminws"}:
        return "web_map"
    return None


def infer_websocket_traffic_channel(websocket, explicit_channel: str | None = None) -> str | None:
    if explicit_channel in TRAFFIC_CHANNELS:
        return explicit_channel

    url = getattr(websocket, "url", None)
    return infer_traffic_channel_from_path(str(getattr(url, "path", "") or ""))


def build_empty_live_layer_payload() -> dict[str, float]:
    return {
        "playerIngressBps": 0.0,
        "playerEgressBps": 0.0,
        "webMapIngressBps": 0.0,
        "webMapEgressBps": 0.0,
        "totalIngressBps": 0.0,
        "totalEgressBps": 0.0,
    }


def build_live_payload(
    *,
    sample_window_sec: int,
    application: dict[str, float],
    wire: dict[str, float],
    selected_layer: str = DEFAULT_TRAFFIC_LAYER,
) -> dict[str, object]:
    return {
        "sampleWindowSec": int(sample_window_sec),
        "selectedLayer": selected_layer if selected_layer in TRAFFIC_LAYERS else DEFAULT_TRAFFIC_LAYER,
        "application": application,
        "wire": wire,
    }


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
        self._live_buckets: dict[str, dict[int, dict[tuple[str, str], int]]] = {
            layer: {} for layer in TRAFFIC_LAYERS
        }
        self._pending_minute: dict[tuple[str, str, str, str], int] = defaultdict(int)
        self._pending_hourly: dict[tuple[str, str, str, str], int] = defaultdict(int)
        self._pending_daily: dict[tuple[str, str, str, str], int] = defaultdict(int)

    @property
    def live_window_sec(self) -> int:
        return self._live_window_sec

    async def record(
        self,
        *,
        layer: str = DEFAULT_TRAFFIC_LAYER,
        channel: str,
        direction: str,
        byte_count: int,
        occurred_at: float | None = None,
    ) -> None:
        self.record_nowait(
            layer=layer,
            channel=channel,
            direction=direction,
            byte_count=byte_count,
            occurred_at=occurred_at,
        )

    def record_nowait(
        self,
        *,
        layer: str = DEFAULT_TRAFFIC_LAYER,
        channel: str,
        direction: str,
        byte_count: int,
        occurred_at: float | None = None,
    ) -> None:
        if layer not in TRAFFIC_LAYERS or channel not in TRAFFIC_CHANNELS or direction not in TRAFFIC_DIRECTIONS:
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

        layer_live_buckets = self._live_buckets[layer]
        live_bucket = layer_live_buckets.get(second_bucket)
        if live_bucket is None:
            live_bucket = defaultdict(int)
            layer_live_buckets[second_bucket] = live_bucket
        live_bucket[(channel, direction)] += amount
        self._prune_live_buckets_locked(layer, second_bucket)
        self._pending_minute[(layer, minute_bucket, channel, direction)] += amount
        self._pending_hourly[(layer, hourly_bucket, channel, direction)] += amount
        self._pending_daily[(layer, daily_bucket, channel, direction)] += amount

    async def build_live_payload(self) -> dict[str, object]:
        now_sec = int(time.time())
        async with self._lock:
            threshold = now_sec - self._live_window_sec + 1
            layer_totals: dict[str, dict[tuple[str, str], int]] = {}
            for layer in TRAFFIC_LAYERS:
                self._prune_live_buckets_locked(layer, now_sec)
                totals: dict[tuple[str, str], int] = defaultdict(int)
                for bucket, series in self._live_buckets[layer].items():
                    if bucket < threshold:
                        continue
                    for key, value in series.items():
                        totals[key] += int(value)
                layer_totals[layer] = totals

        return build_live_payload(
            sample_window_sec=self._live_window_sec,
            application=self._build_live_layer_payload_from_totals(layer_totals["application"]),
            wire=self._build_live_layer_payload_from_totals(layer_totals["wire"]),
        )

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

    def _prune_live_buckets_locked(self, layer: str, current_second: int) -> None:
        threshold = current_second - self._live_window_sec - 2
        layer_buckets = self._live_buckets[layer]
        stale_keys = [bucket for bucket in layer_buckets.keys() if bucket < threshold]
        for bucket in stale_keys:
            layer_buckets.pop(bucket, None)

    def _build_live_layer_payload_from_totals(self, totals: dict[tuple[str, str], int]) -> dict[str, float]:
        window = float(self._live_window_sec)

        def value(channel: str, direction: str) -> float:
            return float(totals.get((channel, direction), 0)) / window

        player_ingress = value("player", "ingress")
        player_egress = value("player", "egress")
        web_map_ingress = value("web_map", "ingress")
        web_map_egress = value("web_map", "egress")
        return {
            "playerIngressBps": player_ingress,
            "playerEgressBps": player_egress,
            "webMapIngressBps": web_map_ingress,
            "webMapEgressBps": web_map_egress,
            "totalIngressBps": player_ingress + web_map_ingress,
            "totalEgressBps": player_egress + web_map_egress,
        }


def _schedule_traffic_updates() -> None:
    from ..app import runtime

    if runtime.admin_payload_service is not None:
        runtime.admin_payload_service.invalidate("live_traffic", "hourly_traffic", "daily_traffic", "traffic_history")
    runtime.admin_sse_hub.schedule_broadcast("traffic_live", delay_sec=1.0)
    runtime.admin_sse_hub.schedule_broadcast("traffic_history", delay_sec=5.0)


async def record_websocket_traffic(
    *,
    layer: str = DEFAULT_TRAFFIC_LAYER,
    channel: str,
    direction: str,
    byte_count: int,
    occurred_at: float | None = None,
) -> None:
    if layer not in TRAFFIC_LAYERS or channel not in TRAFFIC_CHANNELS or direction not in TRAFFIC_DIRECTIONS:
        return

    from ..app import runtime

    service = runtime.admin_traffic_service
    if service is None:
        return

    await service.record(
        layer=layer,
        channel=channel,
        direction=direction,
        byte_count=byte_count,
        occurred_at=occurred_at,
    )
    _schedule_traffic_updates()


def record_websocket_traffic_nowait(
    *,
    layer: str = DEFAULT_TRAFFIC_LAYER,
    channel: str,
    direction: str,
    byte_count: int,
    occurred_at: float | None = None,
) -> None:
    if layer not in TRAFFIC_LAYERS or channel not in TRAFFIC_CHANNELS or direction not in TRAFFIC_DIRECTIONS:
        return

    from ..app import runtime

    service = runtime.admin_traffic_service
    if service is None:
        return

    service.record_nowait(
        layer=layer,
        channel=channel,
        direction=direction,
        byte_count=byte_count,
        occurred_at=occurred_at,
    )
    _schedule_traffic_updates()


async def record_websocket_payload_traffic(
    *,
    websocket,
    layer: str = DEFAULT_TRAFFIC_LAYER,
    direction: str,
    payload: bytes | bytearray | memoryview | str,
    channel: str | None = None,
    occurred_at: float | None = None,
) -> None:
    resolved_channel = infer_websocket_traffic_channel(websocket, channel)
    if resolved_channel is None:
        return
    await record_websocket_traffic(
        layer=layer,
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

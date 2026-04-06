from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass
from typing import Any, Awaitable, Callable

from .models import (
    AdminObservabilityPayload,
    AuditPayload,
    BootstrapPayload,
    ConnectionDetailItem,
    LiveTrafficPayload,
    MetricsPayload,
    OverviewPayload,
    TrafficHistoryPayload,
    RoomOverviewItem,
)
from .store import AdminStore


@dataclass(slots=True)
class _CacheEntry:
    payload: Any
    expires_at: float


class AdminPayloadService:
    def __init__(
        self,
        *,
        admin_store: AdminStore,
        build_room_overview: Callable[[], list[RoomOverviewItem]],
        build_connection_details: Callable[[], list[ConnectionDetailItem]],
        build_live_traffic: Callable[[], Awaitable[LiveTrafficPayload]],
        get_broadcast_hz: Callable[[], float],
        get_sse_subscriber_count: Callable[[], int],
        get_observability_payload: Callable[[], AdminObservabilityPayload],
    ) -> None:
        self._store = admin_store
        self._build_room_overview = build_room_overview
        self._build_connection_details = build_connection_details
        self._build_live_traffic = build_live_traffic
        self._get_broadcast_hz = get_broadcast_hz
        self._get_sse_subscriber_count = get_sse_subscriber_count
        self._get_observability_payload = get_observability_payload
        self._cache: dict[tuple[str, tuple[tuple[str, Any], ...]], _CacheEntry] = {}
        self._inflight: dict[tuple[str, tuple[tuple[str, Any], ...]], asyncio.Task] = {}
        self._lock = asyncio.Lock()

    def invalidate(self, *groups: str) -> None:
        if not groups:
            self._cache.clear()
            return
        selected = set(groups)
        self._cache = {
            key: value
            for key, value in self._cache.items()
            if key[0] not in selected
        }

    async def build_overview_payload(self) -> OverviewPayload:
        async def builder() -> OverviewPayload:
            rooms = self._build_room_overview()
            connection_details = self._build_connection_details()
            hourly_metrics = await self._store.query_hourly_metrics(hours=24)
            hourly_peak = max((item["activePlayers"] for item in hourly_metrics["items"]), default=0)
            return {
                "playerConnections": sum(1 for item in connection_details if item["channel"] == "player"),
                "webMapConnections": sum(1 for item in connection_details if item["channel"] == "web_map"),
                "activeRooms": len(rooms),
                "rooms": rooms,
                "connectionDetails": connection_details,
                "timezone": self._store.timezone_label,
                "dbPathMasked": self._store.masked_db_path,
                "broadcastHz": self._get_broadcast_hz(),
                "hourlyPeak24h": hourly_peak,
                "observability": {
                    **self._get_observability_payload(),
                    "sseSubscribers": self._get_sse_subscriber_count(),
                },
            }

        return await self._get_cached_payload(("overview", ()), ttl_sec=1.0, builder=builder)

    async def build_daily_metrics_payload(self, *, days: int = 30, room_code: str | None = None) -> MetricsPayload:
        return await self._get_cached_payload(
            ("daily_metrics", (("days", days), ("room_code", room_code))),
            ttl_sec=1.0,
            builder=lambda: self._store.query_daily_metrics(days=days, room_code=room_code),
        )

    async def build_hourly_metrics_payload(self, *, hours: int = 48, room_code: str | None = None) -> MetricsPayload:
        return await self._get_cached_payload(
            ("hourly_metrics", (("hours", hours), ("room_code", room_code))),
            ttl_sec=1.0,
            builder=lambda: self._store.query_hourly_metrics(hours=hours, room_code=room_code),
        )

    async def build_live_traffic_payload(self) -> LiveTrafficPayload:
        return await self._get_cached_payload(
            ("live_traffic", ()),
            ttl_sec=1.0,
            builder=self._build_live_traffic,
        )

    async def build_hourly_traffic_payload(self, *, hours: int = 48) -> dict[str, Any]:
        return await self._get_cached_payload(
            ("hourly_traffic", (("hours", hours),)),
            ttl_sec=1.0,
            builder=lambda: self._store.query_hourly_traffic(hours=hours),
        )

    async def build_daily_traffic_payload(self, *, days: int = 30) -> dict[str, Any]:
        return await self._get_cached_payload(
            ("daily_traffic", (("days", days),)),
            ttl_sec=1.0,
            builder=lambda: self._store.query_daily_traffic(days=days),
        )

    async def build_traffic_history_payload(
        self,
        *,
        range_preset: str = "48h",
        granularity: str = "1h",
    ) -> TrafficHistoryPayload:
        return await self._get_cached_payload(
            ("traffic_history", (("range", range_preset), ("granularity", granularity))),
            ttl_sec=1.0,
            builder=lambda: self._store.query_traffic_history(range_preset=range_preset, granularity=granularity),
        )

    async def build_audit_payload(
        self,
        *,
        limit: int = 100,
        before_id: int | None = None,
        event_type: str | None = None,
        actor_type: str | None = None,
        actor_types: list[str] | tuple[str, ...] | None = None,
        success: bool | None = None,
    ) -> AuditPayload:
        normalized_actor_types = tuple(item for item in (actor_types or []) if isinstance(item, str) and item)

        async def builder() -> AuditPayload:
            return await self._store.query_audit_events(
                limit=limit,
                before_id=before_id,
                event_type=event_type,
                actor_type=actor_type,
                actor_types=normalized_actor_types,
                success=success,
            )

        return await self._get_cached_payload(
            (
                "audit",
                (
                    ("limit", limit),
                    ("before_id", before_id),
                    ("event_type", event_type),
                    ("actor_type", actor_type),
                    ("actor_types", normalized_actor_types),
                    ("success", success),
                ),
            ),
            ttl_sec=1.0,
            builder=builder,
        )

    async def build_bootstrap_payload(
        self,
        *,
        audit_limit: int = 100,
        audit_event_type: str | None = None,
        audit_actor_types: tuple[str, ...] = (),
        audit_success: bool | None = None,
        daily_days: int = 30,
        daily_room_code: str | None = None,
        hourly_hours: int = 48,
        hourly_room_code: str | None = None,
        traffic_range: str = "48h",
        traffic_granularity: str = "1h",
    ) -> BootstrapPayload:
        overview, daily_metrics, hourly_metrics, live_traffic, traffic_history, audit = await asyncio.gather(
            self.build_overview_payload(),
            self.build_daily_metrics_payload(days=daily_days, room_code=daily_room_code),
            self.build_hourly_metrics_payload(hours=hourly_hours, room_code=hourly_room_code),
            self.build_live_traffic_payload(),
            self.build_traffic_history_payload(range_preset=traffic_range, granularity=traffic_granularity),
            self.build_audit_payload(
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
            "liveTraffic": live_traffic,
            "trafficHistory": traffic_history,
            "audit": audit,
        }

    async def _get_cached_payload(
        self,
        key: tuple[str, tuple[tuple[str, Any], ...]],
        *,
        ttl_sec: float,
        builder: Callable[[], Awaitable[Any]],
    ) -> Any:
        now = time.monotonic()
        async with self._lock:
            cached = self._cache.get(key)
            if cached is not None and cached.expires_at > now:
                return cached.payload

            task = self._inflight.get(key)
            if task is None:
                task = asyncio.create_task(builder())
                self._inflight[key] = task

        try:
            payload = await task
        finally:
            async with self._lock:
                current = self._inflight.get(key)
                if current is task:
                    self._inflight.pop(key, None)

        async with self._lock:
            self._cache[key] = _CacheEntry(payload=payload, expires_at=time.monotonic() + max(ttl_sec, 0.1))
        return payload

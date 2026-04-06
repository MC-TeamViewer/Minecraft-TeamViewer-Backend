from __future__ import annotations

from typing import NotRequired, TypedDict


class RoomOverviewItem(TypedDict):
    roomCode: str
    playerConnections: int
    webMapConnections: int
    playerIds: list[str]
    webMapIds: list[str]


class ConnectionDetailItem(TypedDict):
    channel: str
    actorId: str
    displayName: str | None
    roomCode: str | None
    protocolVersion: str | None
    programVersion: str | None
    remoteAddr: str | None


class MetricItem(TypedDict):
    bucket: str
    label: str
    activePlayers: int


class MetricsPayload(TypedDict):
    timezone: str
    roomCode: str | None
    items: list[MetricItem]
    days: NotRequired[int]
    hours: NotRequired[int]
    serverTime: NotRequired[float]


class AuditEventItem(TypedDict):
    id: int
    occurredAt: int
    localDate: str
    localHour: str
    eventType: str
    actorType: str
    actorId: str | None
    roomCode: str | None
    success: bool
    remoteAddr: str | None
    detail: dict


class AuditPayload(TypedDict):
    items: list[AuditEventItem]
    nextBeforeId: int | None
    limit: int
    availableEventTypes: list[str]
    serverTime: NotRequired[float]


class AdminObservabilityPayload(TypedDict):
    sseSubscribers: int
    lastRetentionCleanup: str | None
    apiErrors: int
    sseErrors: int
    trustProxyHeaders: bool


class OverviewPayload(TypedDict):
    playerConnections: int
    webMapConnections: int
    activeRooms: int
    rooms: list[RoomOverviewItem]
    connectionDetails: list[ConnectionDetailItem]
    timezone: str
    dbPathMasked: str
    broadcastHz: float
    hourlyPeak24h: int
    observability: AdminObservabilityPayload
    serverTime: NotRequired[float]


class BootstrapPayload(TypedDict):
    serverTime: float
    overview: OverviewPayload
    dailyMetrics: MetricsPayload
    hourlyMetrics: MetricsPayload
    audit: AuditPayload


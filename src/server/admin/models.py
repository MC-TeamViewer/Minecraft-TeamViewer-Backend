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


class LiveTrafficPayload(TypedDict):
    sampleWindowSec: int
    playerIngressBps: float
    playerEgressBps: float
    webMapIngressBps: float
    webMapEgressBps: float
    totalIngressBps: float
    totalEgressBps: float
    serverTime: NotRequired[float]


class TrafficBucketItem(TypedDict):
    bucket: str
    label: str
    playerIngressBytes: int
    playerEgressBytes: int
    webMapIngressBytes: int
    webMapEgressBytes: int
    totalIngressBytes: int
    totalEgressBytes: int
    totalBytes: int


class TrafficMetricsPayload(TypedDict):
    timezone: str
    items: list[TrafficBucketItem]
    totalIngressBytes: int
    totalEgressBytes: int
    totalBytes: int
    days: NotRequired[int]
    hours: NotRequired[int]
    serverTime: NotRequired[float]


class TrafficHistoryPayload(TypedDict):
    timezone: str
    range: str
    granularity: str
    bucketSeconds: int
    items: list[TrafficBucketItem]
    totalIngressBytes: int
    totalEgressBytes: int
    totalBytes: int
    serverTime: NotRequired[float]


class AdminSessionPayload(TypedDict):
    sessionId: str
    actorId: str
    remoteAddr: str | None
    createdAt: int
    lastSeenAt: int
    expiresAt: int


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
    liveTraffic: LiveTrafficPayload
    trafficHistory: TrafficHistoryPayload
    audit: AuditPayload

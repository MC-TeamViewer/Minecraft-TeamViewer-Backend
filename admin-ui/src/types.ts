export interface RoomOverview {
  roomCode: string;
  playerConnections: number;
  webMapConnections: number;
  playerIds: string[];
  webMapIds: string[];
}

export interface ConnectionDetail {
  channel: "player" | "web_map" | string;
  actorId: string;
  displayName: string | null;
  roomCode: string | null;
  protocolVersion: string | null;
  programVersion: string | null;
  remoteAddr: string | null;
}

export interface OverviewPayload {
  playerConnections: number;
  webMapConnections: number;
  activeRooms: number;
  rooms: RoomOverview[];
  connectionDetails: ConnectionDetail[];
  timezone: string;
  dbPathMasked: string;
  broadcastHz: number;
  hourlyPeak24h: number;
  observability: {
    sseSubscribers: number;
    lastRetentionCleanup: string | null;
    apiErrors: number;
    sseErrors: number;
    trustProxyHeaders: boolean;
  };
  serverTime?: number;
}

export interface MetricItem {
  bucket: string;
  label: string;
  activePlayers: number;
}

export interface MetricsPayload {
  timezone: string;
  roomCode: string | null;
  items: MetricItem[];
  days?: number;
  hours?: number;
  startDate?: string | null;
  startAt?: string | null;
  serverTime?: number;
}

export interface AuditDetail {
  [key: string]: unknown;
}

export interface AuditItem {
  id: number;
  occurredAt: number;
  localDate: string;
  localHour: string;
  eventType: string;
  actorType: string;
  actorId: string | null;
  resolvedActorName: string | null;
  roomCode: string | null;
  success: boolean;
  remoteAddr: string | null;
  detail: AuditDetail;
}

export interface PlayerIdentityMapping {
  playerId: string;
  username: string;
  updatedAt: number;
}

export interface AuditPayload {
  items: AuditItem[];
  playerIdentityMappings: PlayerIdentityMapping[];
  nextBeforeId: number | null;
  limit: number;
  availableEventTypes: string[];
  serverTime?: number;
}

export interface AdminSessionPayload {
  sessionId: string;
  actorId: string;
  remoteAddr: string | null;
  createdAt: number;
  lastSeenAt: number;
  expiresAt: number;
}

export interface LiveTrafficPayload {
  sampleWindowSec: number;
  selectedLayer: TrafficLayer;
  application: TrafficLayerLivePayload;
  wire: TrafficLayerLivePayload;
  serverTime?: number;
}

export interface TrafficBucketItem {
  bucket: string;
  label: string;
  playerIngressBytes: number;
  playerEgressBytes: number;
  webMapIngressBytes: number;
  webMapEgressBytes: number;
  totalIngressBytes: number;
  totalEgressBytes: number;
  totalBytes: number;
}

export type TrafficRangePreset = "1h" | "6h" | "24h" | "48h" | "7d" | "30d";
export type TrafficGranularity = "1m" | "5m" | "15m" | "1h" | "1d";
export type TrafficLayer = "application" | "wire";
export type TrafficHistoryDisplayMode = TrafficLayer | "mixed";
export type TrafficMixedViewMode = "total" | "breakdown";

export interface TrafficLayerLivePayload {
  playerIngressBps: number;
  playerEgressBps: number;
  webMapIngressBps: number;
  webMapEgressBps: number;
  totalIngressBps: number;
  totalEgressBps: number;
}

export interface TrafficLayerHistoryPayload {
  items: TrafficBucketItem[];
  totalIngressBytes: number;
  totalEgressBytes: number;
  totalBytes: number;
}

export interface TrafficHistoryPayload {
  timezone: string;
  range: TrafficRangePreset;
  granularity: TrafficGranularity;
  bucketSeconds: number;
  startAt?: string | null;
  selectedLayer: TrafficLayer;
  application: TrafficLayerHistoryPayload;
  wire: TrafficLayerHistoryPayload;
  serverTime?: number;
}

export interface AuditFilters {
  eventType: string;
  actorTypes: string[];
  success: "" | "true" | "false";
}

export interface MetricsFilters {
  roomCode: string;
  dailyDays: number;
  hourlyHours: number;
  dailyStartDate: string;
  hourlyStartAt: string;
}

export interface TrafficFilters {
  range: TrafficRangePreset;
  granularity: TrafficGranularity;
  startAt: string;
}

export interface DashboardFilters {
  audit: AuditFilters;
  metrics: MetricsFilters;
  traffic: TrafficFilters;
}

export interface BootstrapPayload {
  serverTime: number;
  overview: OverviewPayload;
  dailyMetrics: MetricsPayload;
  hourlyMetrics: MetricsPayload;
  liveTraffic: LiveTrafficPayload;
  trafficHistory: TrafficHistoryPayload;
  audit: AuditPayload;
}

export type LiveStatus = "connecting" | "live" | "reconnecting";

export const DEFAULT_AUDIT_FILTERS: AuditFilters = {
  eventType: "",
  actorTypes: ["player", "web_map", "system", "admin"],
  success: "",
};

export const DEFAULT_METRICS_FILTERS: MetricsFilters = {
  roomCode: "",
  dailyDays: 30,
  hourlyHours: 48,
  dailyStartDate: "",
  hourlyStartAt: "",
};

export const DEFAULT_TRAFFIC_FILTERS: TrafficFilters = {
  range: "48h",
  granularity: "1h",
  startAt: "",
};

export const DEFAULT_DASHBOARD_FILTERS: DashboardFilters = {
  audit: DEFAULT_AUDIT_FILTERS,
  metrics: DEFAULT_METRICS_FILTERS,
  traffic: DEFAULT_TRAFFIC_FILTERS,
};

export const DAILY_RANGE_OPTIONS = [7, 14, 30, 60, 90];
export const HOURLY_RANGE_OPTIONS = [12, 24, 48, 72, 168];

export const TRAFFIC_RANGE_OPTIONS: Array<{ label: string; value: TrafficRangePreset }> = [
  { label: "最近 1 小时", value: "1h" },
  { label: "最近 6 小时", value: "6h" },
  { label: "最近 24 小时", value: "24h" },
  { label: "最近 48 小时", value: "48h" },
  { label: "最近 7 天", value: "7d" },
  { label: "最近 30 天", value: "30d" },
];

export const TRAFFIC_GRANULARITY_OPTIONS: Record<TrafficRangePreset, TrafficGranularity[]> = {
  "1h": ["1m", "5m"],
  "6h": ["1m", "5m", "15m"],
  "24h": ["5m", "15m", "1h"],
  "48h": ["15m", "1h"],
  "7d": ["1h", "1d"],
  "30d": ["1d"],
};

export const DEFAULT_TRAFFIC_GRANULARITY_BY_RANGE: Record<TrafficRangePreset, TrafficGranularity> = {
  "1h": "1m",
  "6h": "5m",
  "24h": "15m",
  "48h": "1h",
  "7d": "1h",
  "30d": "1d",
};

export const TRAFFIC_GRANULARITY_LABELS: Record<TrafficGranularity, string> = {
  "1m": "1 分钟",
  "5m": "5 分钟",
  "15m": "15 分钟",
  "1h": "1 小时",
  "1d": "1 天",
};

export const TRAFFIC_LAYER_LABELS: Record<TrafficLayer, string> = {
  application: "应用层",
  wire: "传输层",
};

export const TRAFFIC_LAYER_OPTIONS: Array<{ label: string; value: TrafficLayer }> = [
  { label: "应用层", value: "application" },
  { label: "传输层", value: "wire" },
];

export const TRAFFIC_HISTORY_DISPLAY_LABELS: Record<TrafficHistoryDisplayMode, string> = {
  application: "应用层",
  wire: "传输层",
  mixed: "混合显示",
};

export const TRAFFIC_HISTORY_DISPLAY_OPTIONS: Array<{ label: string; value: TrafficHistoryDisplayMode }> = [
  { label: "应用层", value: "application" },
  { label: "传输层", value: "wire" },
  { label: "混合显示", value: "mixed" },
];

export const TRAFFIC_MIXED_VIEW_OPTIONS: Array<{ label: string; value: TrafficMixedViewMode }> = [
  { label: "总量对比", value: "total" },
  { label: "分流量细则", value: "breakdown" },
];

export interface RoomOverview {
  roomCode: string;
  playerConnections: number;
  webMapConnections: number;
  externalSourceConnections?: number;
  playerIds: string[];
  webMapIds: string[];
  externalSourceIds?: string[];
}

export interface ConnectionDetail {
  channel: "player" | "web_map" | string;
  actorId: string;
  displayName: string | null;
  roomCode: string | null;
  protocolVersion: string | null;
  negotiatedProtocolVersion?: string | null;
  compatibilityEpoch?: string | null;
  compatibilityRuleCount?: number;
  activeCompatibilityRules?: string[];
  programVersion: string | null;
  remoteAddr: string | null;
  connected?: boolean;
  health?: string | null;
  failureCode?: string | null;
  statusReceivedAt?: number | null;
  lastHealthyAt?: number | null;
}

export interface ProtocolCompatibilityRule {
  id: string;
  introducedIn: string;
  summary: string;
  activeConnections: number;
}

export interface ProtocolCompatibilityOverview {
  currentVersion: string;
  minimumSupportedVersion: string;
  knownEpochCount: number;
  registeredRuleCount: number;
  compatibilityConnectionCount: number;
  activeRuleBindingCount: number;
  connectionsByEpoch: Array<{ epoch: string; connections: number }>;
  rules: ProtocolCompatibilityRule[];
}

export interface OverviewPayload {
  playerConnections: number;
  webMapConnections: number;
  externalSourceConnections?: number;
  activeRooms: number;
  rooms: RoomOverview[];
  connectionDetails: ConnectionDetail[];
  protocolCompatibility?: ProtocolCompatibilityOverview;
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
  availableRooms?: string[];
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

export interface AdminPagePayload<T> {
  items: T[];
  total: number;
  page: number;
  pageSize: number;
  availableRooms: string[];
  serverTime?: number;
}

export interface LastSeenHistoryRecord {
  roomCode: string;
  sourceId: string;
  playerUuid: string;
  playerName: string | null;
  x: number | null;
  y: number | null;
  z: number | null;
  dimension: string | null;
  lastSeenAtUtcMs: number | null;
  positionObservedAtUtcMs: number | null;
  offlineDetectedAtUtcMs: number | null;
}

export interface TabHistoryRecord {
  roomCode: string;
  playerUuid: string;
  player: Record<string, unknown>;
  labelFirstObservedAtUtcMs: number;
  lastObservedAtUtcMs: number;
  revision: number;
}

export interface HistoryQuery {
  roomCode: string;
  search: string;
  page: number;
  pageSize: number;
}

export interface DeleteHistoryResponse {
  requested: number;
  deleted: number;
  missing: number;
}

export type RuntimeStateKind =
  | "tab-reports"
  | "players"
  | "entities"
  | "waypoints"
  | "battle-chunks"
  | "player-marks";

export interface RuntimeStateItem {
  id?: string;
  ref?: {
    dimension: string;
    chunkX: number;
    chunkZ: number;
  };
  sourceId: string | null;
  reportedAtUtcMs: number | null;
  data: Record<string, unknown>;
}

export interface RuntimeStatePayload extends AdminPagePayload<RuntimeStateItem> {
  kind: RuntimeStateKind;
  roomCode: string;
}

export interface LiveTrafficPayload {
  sampleWindowSec: number;
  selectedLayer: TrafficLayer;
  application: TrafficLayerLivePayload;
  wire: TrafficLayerLivePayload;
  serverTime?: number;
}

export interface ProtobufMetric {
  messageCount: number;
  messagesPerSecond: number;
  byteCount: number;
  bytesPerSecond: number;
  maxPacketBytes: number;
  lastSentAt: number | null;
}

export interface ProtobufMessageMetric extends ProtobufMetric {
  messageType: string;
}

export interface ProtobufConnectionTraffic {
  actorId: string;
  channel: string;
  total: ProtobufMetric;
  snapshotFull: ProtobufMetric;
  messageTypes: ProtobufMessageMetric[];
}

export interface ProtobufTrafficPayload {
  sampleWindowSec: number;
  total: ProtobufMetric;
  snapshotFull: ProtobufMetric;
  messageTypes: ProtobufMessageMetric[];
  connections: ProtobufConnectionTraffic[];
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
  roomCode: string;
}

export interface RoomDataSummary {
  roomCode: string;
  activeConnections: number;
  auditEvents: number;
  dailyActivity: number;
  hourlyActivity: number;
  tabHistoryEntries: number;
  tabHistoryDeltas: number;
  tabHistoryRevisions: number;
  tabHistoryHeads: number;
  lastSeenRecords: number;
  identityMappings: number;
  totalRemovable: number;
}

export interface RoomDataMaintenancePayload {
  items: RoomDataSummary[];
  trafficDataRetained: boolean;
  warning: string;
  serverTime?: number;
}

export interface RoomDataPurgeResponse {
  ok: boolean;
  roomCode: string;
  deleted: Record<string, number>;
  trafficDataRetained: boolean;
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
  protobufTraffic: ProtobufTrafficPayload;
  trafficHistory: TrafficHistoryPayload;
  audit: AuditPayload;
}

export type LiveStatus = "connecting" | "live" | "reconnecting";

export const DEFAULT_AUDIT_FILTERS: AuditFilters = {
  eventType: "",
  actorTypes: ["player", "external_source", "web_map", "system", "admin"],
  success: "",
  roomCode: "",
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

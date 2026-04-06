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
  roomCode: string | null;
  success: boolean;
  remoteAddr: string | null;
  detail: AuditDetail;
}

export interface AuditPayload {
  items: AuditItem[];
  nextBeforeId: number | null;
  limit: number;
  serverTime?: number;
}

export interface AuditFilters {
  eventType: string;
  actorTypes: string[];
  success: "" | "true" | "false";
}

export interface BootstrapPayload {
  serverTime: number;
  overview: OverviewPayload;
  dailyMetrics: MetricsPayload;
  hourlyMetrics: MetricsPayload;
  audit: AuditPayload;
}

export type LiveStatus = "connecting" | "live" | "reconnecting";

export const DEFAULT_AUDIT_FILTERS: AuditFilters = {
  eventType: "",
  actorTypes: ["player", "web_map", "system"],
  success: "",
};

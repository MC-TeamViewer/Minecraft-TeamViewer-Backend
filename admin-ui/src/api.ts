import type {
  AdminSessionPayload,
  AdminPagePayload,
  AuditFilters,
  AuditPayload,
  DeleteHistoryResponse,
  HistoryQuery,
  LastSeenHistoryRecord,
  LiveTrafficPayload,
  MetricsFilters,
  MetricsPayload,
  OverviewPayload,
  TrafficFilters,
  TrafficHistoryPayload,
  RuntimeStateKind,
  RuntimeStatePayload,
  TabHistoryRecord,
} from "@/types";

export class ApiError extends Error {
  status: number;

  constructor(status: number, message?: string) {
    super(message ?? `request_failed:${status}`);
    this.status = status;
  }
}

function appendQuery(url: URL, params: Record<string, string | string[] | undefined>): URL {
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === "") {
      continue;
    }
    if (Array.isArray(value)) {
      for (const item of value) {
        if (item) {
          url.searchParams.append(key, item);
        }
      }
      continue;
    }
    url.searchParams.set(key, value);
  }
  return url;
}

async function requestJson<T>(
  path: string,
  {
    method = "GET",
    params = {},
    body,
  }: {
    method?: "GET" | "POST" | "DELETE";
    params?: Record<string, string | string[] | undefined>;
    body?: unknown;
  } = {},
): Promise<T> {
  const url = appendQuery(new URL(path, window.location.origin), params);
  const response = await fetch(url.toString(), {
    method,
    headers: {
      Accept: "application/json",
      ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
    },
    body: body !== undefined ? JSON.stringify(body) : undefined,
    credentials: "same-origin",
  });

  if (!response.ok) {
    throw new ApiError(response.status);
  }

  return response.json() as Promise<T>;
}

export function fetchSession(): Promise<AdminSessionPayload> {
  return requestJson<AdminSessionPayload>("/admin/api/session");
}

export function loginSession(username: string, password: string): Promise<AdminSessionPayload> {
  return requestJson<AdminSessionPayload>("/admin/api/session/login", {
    method: "POST",
    body: { username, password },
  });
}

export function logoutSession(): Promise<{ ok: boolean }> {
  return requestJson<{ ok: boolean }>("/admin/api/session/logout", {
    method: "POST",
  });
}

export function fetchOverview(): Promise<OverviewPayload> {
  return requestJson<OverviewPayload>("/admin/api/overview");
}

export function fetchDailyMetrics(filters: MetricsFilters): Promise<MetricsPayload> {
  return requestJson<MetricsPayload>("/admin/api/metrics/daily", {
    params: {
      days: String(filters.dailyDays),
      startDate: filters.dailyStartDate,
      roomCode: filters.roomCode,
    },
  });
}

export function fetchHourlyMetrics(filters: MetricsFilters): Promise<MetricsPayload> {
  return requestJson<MetricsPayload>("/admin/api/metrics/hourly", {
    params: {
      hours: String(filters.hourlyHours),
      startAt: filters.hourlyStartAt,
      roomCode: filters.roomCode,
    },
  });
}

export function fetchLiveTraffic(): Promise<LiveTrafficPayload> {
  return requestJson<LiveTrafficPayload>("/admin/api/traffic/live");
}

export function fetchTrafficHistory(filters: TrafficFilters): Promise<TrafficHistoryPayload> {
  return requestJson<TrafficHistoryPayload>("/admin/api/traffic/history", {
    params: {
      range: filters.range,
      granularity: filters.granularity,
      startAt: filters.startAt,
    },
  });
}

export function fetchAudit(filters: AuditFilters, limit = 100): Promise<AuditPayload> {
  return requestJson<AuditPayload>("/admin/api/audit", {
    params: {
      limit: String(limit),
      eventType: filters.eventType,
      actorTypes: filters.actorTypes,
      success: filters.success,
    },
  });
}

export function fetchLastSeenHistory(query: HistoryQuery): Promise<AdminPagePayload<LastSeenHistoryRecord>> {
  return requestJson<AdminPagePayload<LastSeenHistoryRecord>>("/admin/api/history/last-seen", {
    params: {
      roomCode: query.roomCode,
      search: query.search,
      page: String(query.page),
      pageSize: String(query.pageSize),
    },
  });
}

export function deleteLastSeenHistory(records: Pick<LastSeenHistoryRecord, "roomCode" | "sourceId" | "playerUuid">[]): Promise<DeleteHistoryResponse> {
  return requestJson<DeleteHistoryResponse>("/admin/api/history/last-seen", {
    method: "DELETE",
    body: { records },
  });
}

export function fetchTabHistory(query: HistoryQuery): Promise<AdminPagePayload<TabHistoryRecord>> {
  return requestJson<AdminPagePayload<TabHistoryRecord>>("/admin/api/history/tab", {
    params: {
      roomCode: query.roomCode,
      search: query.search,
      page: String(query.page),
      pageSize: String(query.pageSize),
    },
  });
}

export function deleteTabHistory(records: Pick<TabHistoryRecord, "roomCode" | "playerUuid">[]): Promise<DeleteHistoryResponse> {
  return requestJson<DeleteHistoryResponse>("/admin/api/history/tab", {
    method: "DELETE",
    body: { records },
  });
}

export function fetchRuntimeState(
  kind: RuntimeStateKind,
  query: Pick<HistoryQuery, "roomCode" | "page" | "pageSize">,
): Promise<RuntimeStatePayload> {
  return requestJson<RuntimeStatePayload>(`/admin/api/runtime/${kind}`, {
    params: {
      roomCode: query.roomCode,
      page: String(query.page),
      pageSize: String(query.pageSize),
    },
  });
}

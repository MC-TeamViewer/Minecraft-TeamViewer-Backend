import type {
  AdminSessionPayload,
  AuditFilters,
  AuditPayload,
  LiveTrafficPayload,
  MetricsFilters,
  MetricsPayload,
  OverviewPayload,
  TrafficFilters,
  TrafficHistoryPayload,
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
    method?: "GET" | "POST";
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
      roomCode: filters.roomCode,
    },
  });
}

export function fetchHourlyMetrics(filters: MetricsFilters): Promise<MetricsPayload> {
  return requestJson<MetricsPayload>("/admin/api/metrics/hourly", {
    params: {
      hours: String(filters.hourlyHours),
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

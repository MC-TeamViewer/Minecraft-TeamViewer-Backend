import type {
  AuditFilters,
  AuditPayload,
  MetricsPayload,
  MetricsFilters,
  OverviewPayload,
} from "@/types";

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

async function fetchJson<T>(path: string, params: Record<string, string | string[] | undefined> = {}): Promise<T> {
  const url = appendQuery(new URL(path, window.location.origin), params);
  const response = await fetch(url.toString(), {
    headers: {
      Accept: "application/json",
    },
    credentials: "same-origin",
  });

  if (!response.ok) {
    throw new Error(`request_failed:${response.status}`);
  }

  return response.json() as Promise<T>;
}

export function fetchOverview(): Promise<OverviewPayload> {
  return fetchJson<OverviewPayload>("/admin/api/overview");
}

export function fetchDailyMetrics(filters: MetricsFilters): Promise<MetricsPayload> {
  return fetchJson<MetricsPayload>("/admin/api/metrics/daily", {
    days: String(filters.dailyDays),
    roomCode: filters.roomCode,
  });
}

export function fetchHourlyMetrics(filters: MetricsFilters): Promise<MetricsPayload> {
  return fetchJson<MetricsPayload>("/admin/api/metrics/hourly", {
    hours: String(filters.hourlyHours),
    roomCode: filters.roomCode,
  });
}

export function fetchAudit(filters: AuditFilters, limit = 100): Promise<AuditPayload> {
  return fetchJson<AuditPayload>("/admin/api/audit", {
    limit: String(limit),
    eventType: filters.eventType,
    actorTypes: filters.actorTypes,
    success: filters.success,
  });
}

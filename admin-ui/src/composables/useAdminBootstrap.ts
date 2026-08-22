import {
  fetchAudit,
  fetchDailyMetrics,
  fetchHourlyMetrics,
  fetchLiveTraffic,
  fetchLiveProtobufTraffic,
  fetchOverview,
  fetchTrafficHistory,
} from "@/api";
import type { AuditPayload, BootstrapPayload, DashboardFilters } from "@/types";

function emptyAuditPayload(limit = 100): AuditPayload {
  return {
    items: [],
    playerIdentityMappings: [],
    nextBeforeId: null,
    limit,
    availableEventTypes: [],
  };
}

async function loadOptionalAudit(filters: DashboardFilters): Promise<AuditPayload> {
  try {
    return await fetchAudit(filters.audit);
  } catch (error) {
    console.warn("admin audit bootstrap failed; continuing with the remaining dashboard", error);
    return emptyAuditPayload();
  }
}

export async function loadAdminBootstrap(filters: DashboardFilters): Promise<BootstrapPayload> {
  const [overview, dailyMetrics, hourlyMetrics, liveTraffic, protobufTraffic, trafficHistory, audit] = await Promise.all([
    fetchOverview(),
    fetchDailyMetrics(filters.metrics),
    fetchHourlyMetrics(filters.metrics),
    fetchLiveTraffic(),
    fetchLiveProtobufTraffic(),
    fetchTrafficHistory(filters.traffic),
    loadOptionalAudit(filters),
  ]);

  return {
    serverTime: Date.now() / 1000,
    overview,
    dailyMetrics,
    hourlyMetrics,
    liveTraffic,
    protobufTraffic,
    trafficHistory,
    audit,
  };
}

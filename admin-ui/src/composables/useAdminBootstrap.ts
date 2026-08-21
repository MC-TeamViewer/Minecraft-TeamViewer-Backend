import {
  fetchAudit,
  fetchDailyMetrics,
  fetchHourlyMetrics,
  fetchLiveTraffic,
  fetchLiveProtobufTraffic,
  fetchOverview,
  fetchTrafficHistory,
} from "@/api";
import type { BootstrapPayload, DashboardFilters } from "@/types";

export async function loadAdminBootstrap(filters: DashboardFilters): Promise<BootstrapPayload> {
  const [overview, dailyMetrics, hourlyMetrics, liveTraffic, protobufTraffic, trafficHistory, audit] = await Promise.all([
    fetchOverview(),
    fetchDailyMetrics(filters.metrics),
    fetchHourlyMetrics(filters.metrics),
    fetchLiveTraffic(),
    fetchLiveProtobufTraffic(),
    fetchTrafficHistory(filters.traffic),
    fetchAudit(filters.audit),
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

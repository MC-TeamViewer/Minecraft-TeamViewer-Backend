import { fetchAudit, fetchDailyMetrics, fetchHourlyMetrics, fetchOverview } from "@/api";
import type { BootstrapPayload, DashboardFilters } from "@/types";

export async function loadAdminBootstrap(filters: DashboardFilters): Promise<BootstrapPayload> {
  const [overview, dailyMetrics, hourlyMetrics, audit] = await Promise.all([
    fetchOverview(),
    fetchDailyMetrics(filters.metrics),
    fetchHourlyMetrics(filters.metrics),
    fetchAudit(filters.audit),
  ]);

  return {
    serverTime: Date.now() / 1000,
    overview,
    dailyMetrics,
    hourlyMetrics,
    audit,
  };
}

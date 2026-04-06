import { fetchAudit, fetchDailyMetrics, fetchHourlyMetrics, fetchOverview } from "@/api";
import type { AuditFilters, BootstrapPayload } from "@/types";

export async function loadAdminBootstrap(filters: AuditFilters): Promise<BootstrapPayload> {
  const [overview, dailyMetrics, hourlyMetrics, audit] = await Promise.all([
    fetchOverview(),
    fetchDailyMetrics(),
    fetchHourlyMetrics(),
    fetchAudit(filters),
  ]);

  return {
    serverTime: Date.now() / 1000,
    overview,
    dailyMetrics,
    hourlyMetrics,
    audit,
  };
}

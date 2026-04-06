import { ref } from "vue";

import type { MetricsPayload } from "@/types";


export function useMetricsState() {
  const dailyMetrics = ref<MetricsPayload | null>(null);
  const hourlyMetrics = ref<MetricsPayload | null>(null);

  function applyDailyMetrics(payload: MetricsPayload) {
    dailyMetrics.value = payload;
  }

  function applyHourlyMetrics(payload: MetricsPayload) {
    hourlyMetrics.value = payload;
  }

  return {
    dailyMetrics,
    hourlyMetrics,
    applyDailyMetrics,
    applyHourlyMetrics,
  };
}

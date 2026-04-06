import { ref } from "vue";

import type { LiveTrafficPayload, MetricsPayload, TrafficHistoryPayload } from "@/types";


export function useMetricsState() {
  const dailyMetrics = ref<MetricsPayload | null>(null);
  const hourlyMetrics = ref<MetricsPayload | null>(null);
  const liveTraffic = ref<LiveTrafficPayload | null>(null);
  const trafficHistory = ref<TrafficHistoryPayload | null>(null);

  function applyDailyMetrics(payload: MetricsPayload) {
    dailyMetrics.value = payload;
  }

  function applyHourlyMetrics(payload: MetricsPayload) {
    hourlyMetrics.value = payload;
  }

  function applyLiveTraffic(payload: LiveTrafficPayload) {
    liveTraffic.value = payload;
  }

  function applyTrafficHistory(payload: TrafficHistoryPayload) {
    trafficHistory.value = payload;
  }

  function resetMetrics() {
    dailyMetrics.value = null;
    hourlyMetrics.value = null;
    liveTraffic.value = null;
    trafficHistory.value = null;
  }

  return {
    dailyMetrics,
    hourlyMetrics,
    liveTraffic,
    trafficHistory,
    applyDailyMetrics,
    applyHourlyMetrics,
    applyLiveTraffic,
    applyTrafficHistory,
    resetMetrics,
  };
}

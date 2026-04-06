<script setup lang="ts">
import ElAlert from "element-plus/es/components/alert/index";
import ElButton from "element-plus/es/components/button/index";
import ElCard from "element-plus/es/components/card/index";
import ElSkeleton from "element-plus/es/components/skeleton/index";
import ElTag from "element-plus/es/components/tag/index";
import { computed, defineAsyncComponent, onMounted, ref, watch } from "vue";

import {
  ApiError,
  fetchAudit,
  fetchDailyMetrics,
  fetchHourlyMetrics,
  fetchSession,
  fetchTrafficHistory,
  loginSession,
  logoutSession,
} from "@/api";
import LiveTrafficCards from "@/components/LiveTrafficCards.vue";
import LoginPanel from "@/components/LoginPanel.vue";
import MetricChartCard from "@/components/MetricChartCard.vue";
import MetricsToolbar from "@/components/MetricsToolbar.vue";
import OverviewCards from "@/components/OverviewCards.vue";
import TrafficChartCard from "@/components/TrafficChartCard.vue";
import TrafficToolbar from "@/components/TrafficToolbar.vue";
import { loadAdminBootstrap } from "@/composables/useAdminBootstrap";
import { useAdminSse } from "@/composables/useAdminSse";
import { useAuditState } from "@/composables/useAuditState";
import { useMetricsState } from "@/composables/useMetricsState";
import { useOverviewState } from "@/composables/useOverviewState";
import type {
  AdminSessionPayload,
  AuditFilters as AuditFiltersModel,
  BootstrapPayload,
  DashboardFilters,
  MetricsFilters,
  TrafficFilters,
  TrafficLayer,
} from "@/types";
import { DEFAULT_AUDIT_FILTERS, DEFAULT_METRICS_FILTERS, DEFAULT_TRAFFIC_FILTERS } from "@/types";

const RoomOverviewTable = defineAsyncComponent(() => import("@/components/RoomOverviewTable.vue"));
const ConnectionStatusTable = defineAsyncComponent(() => import("@/components/ConnectionStatusTable.vue"));
const AuditFilters = defineAsyncComponent(() => import("@/components/AuditFilters.vue"));
const AuditTable = defineAsyncComponent(() => import("@/components/AuditTable.vue"));

const auditFilters = ref<AuditFiltersModel>({ ...DEFAULT_AUDIT_FILTERS });
const metricsFilters = ref<MetricsFilters>({ ...DEFAULT_METRICS_FILTERS });
const trafficFilters = ref<TrafficFilters>({ ...DEFAULT_TRAFFIC_FILTERS });
const trafficLayer = ref<TrafficLayer>("application");
const session = ref<AdminSessionPayload | null>(null);
const sessionLoading = ref(true);
const loginLoading = ref(false);
const loginError = ref<string | null>(null);
const isLoading = ref(true);
const loadError = ref<string | null>(null);
const dailyMetricsLoading = ref(false);
const hourlyMetricsLoading = ref(false);
const trafficHistoryLoading = ref(false);
let metricsRefreshVersion = 0;
let trafficRefreshVersion = 0;

const { overview, roomOptions, applyOverview, resetOverview } = useOverviewState();
const {
  dailyMetrics,
  hourlyMetrics,
  liveTraffic,
  trafficHistory,
  applyDailyMetrics,
  applyHourlyMetrics,
  applyLiveTraffic,
  applyTrafficHistory,
  resetMetrics,
} = useMetricsState();
const { auditPayload, eventTypes, applyAudit, resetAudit } = useAuditState();

const dashboardFilters = computed<DashboardFilters>(() => ({
  audit: auditFilters.value,
  metrics: metricsFilters.value,
  traffic: trafficFilters.value,
}));

function markLoaded() {
  isLoading.value = false;
  loadError.value = null;
}

function resetDashboard() {
  resetOverview();
  resetMetrics();
  resetAudit();
  isLoading.value = true;
  loadError.value = null;
  dailyMetricsLoading.value = false;
  hourlyMetricsLoading.value = false;
  trafficHistoryLoading.value = false;
}

function matchesDailyFilters(payload: { days?: number; roomCode?: string | null }) {
  return (payload.days ?? DEFAULT_METRICS_FILTERS.dailyDays) === metricsFilters.value.dailyDays
    && (payload.roomCode ?? "") === metricsFilters.value.roomCode;
}

function matchesHourlyFilters(payload: { hours?: number; roomCode?: string | null }) {
  return (payload.hours ?? DEFAULT_METRICS_FILTERS.hourlyHours) === metricsFilters.value.hourlyHours
    && (payload.roomCode ?? "") === metricsFilters.value.roomCode;
}

function applyBootstrap(payload: BootstrapPayload) {
  applyOverview(payload.overview);
  applyDailyMetrics(payload.dailyMetrics);
  applyHourlyMetrics(payload.hourlyMetrics);
  applyLiveTraffic(payload.liveTraffic);
  applyTrafficHistory(payload.trafficHistory);
  trafficLayer.value = payload.trafficHistory.selectedLayer ?? payload.liveTraffic.selectedLayer ?? "application";
  applyAudit(payload.audit);
  dailyMetricsLoading.value = false;
  hourlyMetricsLoading.value = false;
  trafficHistoryLoading.value = false;
  markLoaded();
}

function resolveErrorMessage(error: unknown, fallback = "request_failed"): string {
  if (error instanceof ApiError) {
    return `${fallback}:${error.status}`;
  }
  return error instanceof Error ? error.message : fallback;
}

async function handleUnauthorizedState() {
  stop();
  session.value = null;
  loginError.value = "会话已失效，请重新登录。";
  resetDashboard();
  sessionLoading.value = false;
}

async function guardApiCall(action: () => Promise<void>, fallbackMessage: string) {
  try {
    await action();
    return true;
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) {
      await handleUnauthorizedState();
      return false;
    }
    loadError.value = resolveErrorMessage(error, fallbackMessage);
    return false;
  }
}

async function fallbackBootstrap() {
  const ok = await guardApiCall(async () => {
    applyBootstrap(await loadAdminBootstrap(dashboardFilters.value));
  }, "bootstrap_failed");
  if (!ok) {
    isLoading.value = false;
  }
}

async function refreshMetricsOnly() {
  const refreshVersion = ++metricsRefreshVersion;
  const nextFilters = { ...metricsFilters.value };
  dailyMetricsLoading.value = true;
  hourlyMetricsLoading.value = true;
  const ok = await guardApiCall(async () => {
    const [daily, hourly] = await Promise.all([
      fetchDailyMetrics(nextFilters),
      fetchHourlyMetrics(nextFilters),
    ]);
    if (refreshVersion !== metricsRefreshVersion) {
      return;
    }
    applyDailyMetrics(daily);
    applyHourlyMetrics(hourly);
  }, "metrics_refresh_failed");
  if (!ok && refreshVersion === metricsRefreshVersion) {
    dailyMetricsLoading.value = false;
    hourlyMetricsLoading.value = false;
    return;
  }
  if (refreshVersion === metricsRefreshVersion) {
    dailyMetricsLoading.value = false;
    hourlyMetricsLoading.value = false;
  }
}

async function refreshTrafficOnly() {
  const refreshVersion = ++trafficRefreshVersion;
  const nextFilters = { ...trafficFilters.value };
  trafficHistoryLoading.value = true;
  const ok = await guardApiCall(async () => {
    const history = await fetchTrafficHistory(nextFilters);
    if (refreshVersion !== trafficRefreshVersion) {
      return;
    }
    applyTrafficHistory(history);
  }, "traffic_refresh_failed");
  if (!ok && refreshVersion === trafficRefreshVersion) {
    trafficHistoryLoading.value = false;
    return;
  }
  if (refreshVersion === trafficRefreshVersion) {
    trafficHistoryLoading.value = false;
  }
}

async function refreshAuditOnly() {
  await guardApiCall(async () => {
    applyAudit(await fetchAudit(auditFilters.value));
  }, "audit_refresh_failed");
}

async function hydrateSession() {
  sessionLoading.value = true;
  loginError.value = null;
  try {
    session.value = await fetchSession();
  } catch (error) {
    if (!(error instanceof ApiError && error.status === 401)) {
      loginError.value = resolveErrorMessage(error, "session_check_failed");
    }
    session.value = null;
  } finally {
    sessionLoading.value = false;
  }
}

const {
  status,
  lastHeartbeatAt,
  connect,
  restart,
  stop,
  waitForBootstrap,
} = useAdminSse({
  getDashboardFilters: () => dashboardFilters.value,
  onBootstrap: applyBootstrap,
  onOverview: (payload) => {
    applyOverview(payload);
    markLoaded();
  },
  onDailyMetrics: (payload) => {
    if (!matchesDailyFilters(payload)) {
      return;
    }
    applyDailyMetrics(payload);
    dailyMetricsLoading.value = false;
  },
  onHourlyMetrics: (payload) => {
    if (!matchesHourlyFilters(payload)) {
      return;
    }
    applyHourlyMetrics(payload);
    hourlyMetricsLoading.value = false;
  },
  onLiveTraffic: applyLiveTraffic,
  onTrafficHistory: (payload) => {
    if (payload.range !== trafficFilters.value.range || payload.granularity !== trafficFilters.value.granularity) {
      return;
    }
    applyTrafficHistory(payload);
    trafficHistoryLoading.value = false;
  },
  onAudit: applyAudit,
  onError: async () => {
    try {
      await fetchSession();
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        await handleUnauthorizedState();
      }
    }
  },
});

async function activateDashboard() {
  if (!session.value) {
    return;
  }
  resetDashboard();
  connect();
  const receivedBootstrap = await waitForBootstrap(1200);
  if (!receivedBootstrap) {
    await fallbackBootstrap();
  }
}

watch(
  auditFilters,
  async () => {
    if (!session.value) {
      return;
    }
    try {
      await refreshAuditOnly();
    } finally {
      if (session.value) {
        restart();
      }
    }
  },
  { deep: true },
);

watch(
  metricsFilters,
  async () => {
    if (!session.value) {
      return;
    }
    try {
      await refreshMetricsOnly();
    } finally {
      if (session.value) {
        restart();
      }
    }
  },
  { deep: true },
);

watch(
  trafficFilters,
  async () => {
    if (!session.value) {
      return;
    }
    try {
      await refreshTrafficOnly();
    } finally {
      if (session.value) {
        restart();
      }
    }
  },
  { deep: true },
);

onMounted(async () => {
  await hydrateSession();
  if (session.value) {
    await activateDashboard();
  } else {
    isLoading.value = false;
  }
});

const liveStatusLabel = computed(() => {
  if (status.value === "live") {
    return "Live";
  }
  if (status.value === "reconnecting") {
    return "Reconnecting";
  }
  return "Connecting";
});

const dailyChartKey = computed(() => `daily:${metricsFilters.value.dailyDays}:${metricsFilters.value.roomCode || "global"}`);
const hourlyChartKey = computed(() => `hourly:${metricsFilters.value.hourlyHours}:${metricsFilters.value.roomCode || "global"}`);
const trafficChartKey = computed(() => `traffic:${trafficFilters.value.range}:${trafficFilters.value.granularity}`);

const heroTags = computed(() => [
  `管理员 ${session.value?.actorId ?? "-"}`,
  `会话 ${session.value?.sessionId ?? "-"}`,
  `时区 ${overview.value?.timezone ?? "-"}`,
  `数据库 ${overview.value?.dbPathMasked ?? "-"}`,
  `广播 ${overview.value?.broadcastHz ?? "-"} Hz`,
  `状态 ${liveStatusLabel.value}`,
  `SSE ${overview.value?.observability?.sseSubscribers ?? 0}`,
  `代理头 ${overview.value?.observability?.trustProxyHeaders ? "已启用" : "未启用"}`,
  `API 错误 ${overview.value?.observability?.apiErrors ?? 0}`,
  `SSE 错误 ${overview.value?.observability?.sseErrors ?? 0}`,
  `清理 ${overview.value?.observability?.lastRetentionCleanup ?? "-"}`,
  `心跳 ${lastHeartbeatAt.value ? new Date(lastHeartbeatAt.value).toLocaleTimeString("zh-CN", { hour12: false }) : "-"}`,
]);

async function handleManualRefresh() {
  if (!session.value) {
    return;
  }
  restart();
  const receivedBootstrap = await waitForBootstrap(1200);
  if (!receivedBootstrap) {
    await fallbackBootstrap();
  }
}

async function handleLogin(payload: { username: string; password: string }) {
  loginLoading.value = true;
  loginError.value = null;
  try {
    session.value = await loginSession(payload.username, payload.password);
    await activateDashboard();
  } catch (error) {
    loginError.value = resolveErrorMessage(error, "login_failed");
  } finally {
    loginLoading.value = false;
  }
}

async function handleLogout() {
  try {
    await logoutSession();
  } catch (_error) {
    // Ignore logout errors locally; session will be cleared anyway.
  }
  stop();
  session.value = null;
  loginError.value = null;
  resetDashboard();
  isLoading.value = false;
}

function updateAuditFilters(value: AuditFiltersModel) {
  auditFilters.value = value;
}

function updateMetricsFilters(value: MetricsFilters) {
  metricsFilters.value = value;
}

function updateTrafficFilters(value: TrafficFilters) {
  trafficFilters.value = value;
}

function updateTrafficLayer(value: TrafficLayer) {
  trafficLayer.value = value;
}
</script>

<template>
  <el-skeleton v-if="sessionLoading" :rows="8" animated class="surface-card app-loading-shell" />

  <LoginPanel
    v-else-if="!session"
    :loading="loginLoading"
    :error-message="loginError"
    @login="handleLogin"
  />

  <div v-else class="app-shell">
    <header class="hero-panel">
      <div>
        <span class="hero-eyebrow">只读后台</span>
        <h1>TeamViewRelay Admin</h1>
        <p>
          统一查看在线概况、实时网速、自定义粒度历史流量、最近 DAU、小时活跃，以及实时审计日志。
        </p>
      </div>
      <div class="hero-actions">
        <el-tag :type="status === 'live' ? 'success' : status === 'reconnecting' ? 'warning' : 'info'" round>
          {{ liveStatusLabel }}
        </el-tag>
        <el-button @click="handleLogout">退出登录</el-button>
        <el-button type="primary" @click="handleManualRefresh">刷新全页</el-button>
      </div>
      <div class="hero-tag-row">
        <span v-for="tag in heroTags" :key="tag" class="hero-tag">{{ tag }}</span>
      </div>
    </header>

    <el-alert
      v-if="loadError"
      type="error"
      :closable="false"
      show-icon
      :title="`加载后台数据失败：${loadError}`"
      class="section-gap"
    />

    <el-skeleton v-if="isLoading && !overview" :rows="6" animated class="surface-card section-gap" />

    <template v-else>
      <OverviewCards :overview="overview" />
      <LiveTrafficCards
        :traffic="liveTraffic"
        :selected-layer="trafficLayer"
        @update:selected-layer="updateTrafficLayer"
      />

      <el-card shadow="never" class="surface-card">
        <TrafficToolbar
          :model-value="trafficFilters"
          :selected-layer="trafficLayer"
          @update:model-value="updateTrafficFilters"
          @update:selected-layer="updateTrafficLayer"
        />
      </el-card>

      <TrafficChartCard
        :key="trafficChartKey"
        title="历史流量"
        description="按所选范围与粒度汇总核心业务 WebSocket 双向流量。"
        :metrics="trafficHistory"
        :selected-layer="trafficLayer"
      />

      <el-card shadow="never" class="surface-card">
        <MetricsToolbar
          :model-value="metricsFilters"
          :room-options="roomOptions"
          @update:model-value="updateMetricsFilters"
        />
      </el-card>

      <section class="two-column-grid">
        <MetricChartCard
          :key="dailyChartKey"
          :title="`最近 ${metricsFilters.dailyDays} 天 DAU`"
          description="按 submitPlayerId 去重统计本地自然日活跃玩家，空桶自动补零。"
          :metrics="dailyMetrics"
          :loading="dailyMetricsLoading"
        />
        <MetricChartCard
          :key="hourlyChartKey"
          :title="`最近 ${metricsFilters.hourlyHours} 小时活跃`"
          description="按本地时区整点统计小时桶内的唯一活跃玩家，空桶自动补零。"
          :metrics="hourlyMetrics"
          :loading="hourlyMetricsLoading"
        />
      </section>

      <RoomOverviewTable :overview="overview" />
      <ConnectionStatusTable :overview="overview" />

      <section class="audit-stack">
        <AuditFilters
          :model-value="auditFilters"
          :event-types="eventTypes"
          @refresh="refreshAuditOnly"
          @update:model-value="updateAuditFilters"
        />
        <AuditTable :audit="auditPayload" />
      </section>
    </template>
  </div>
</template>

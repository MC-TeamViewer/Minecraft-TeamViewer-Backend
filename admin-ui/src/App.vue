<script setup lang="ts">
import ElAlert from "element-plus/es/components/alert/index";
import ElButton from "element-plus/es/components/button/index";
import ElCard from "element-plus/es/components/card/index";
import ElSkeleton from "element-plus/es/components/skeleton/index";
import ElTag from "element-plus/es/components/tag/index";
import { computed, defineAsyncComponent, onMounted, ref, watch } from "vue";

import { fetchAudit, fetchDailyMetrics, fetchHourlyMetrics } from "@/api";
import MetricChartCard from "@/components/MetricChartCard.vue";
import MetricsToolbar from "@/components/MetricsToolbar.vue";
import OverviewCards from "@/components/OverviewCards.vue";
import { loadAdminBootstrap } from "@/composables/useAdminBootstrap";
import { useAdminSse } from "@/composables/useAdminSse";
import { useAuditState } from "@/composables/useAuditState";
import { useMetricsState } from "@/composables/useMetricsState";
import { useOverviewState } from "@/composables/useOverviewState";
import type { AuditFilters as AuditFiltersModel, BootstrapPayload, DashboardFilters, MetricsFilters } from "@/types";
import { DEFAULT_AUDIT_FILTERS, DEFAULT_METRICS_FILTERS } from "@/types";

const RoomOverviewTable = defineAsyncComponent(() => import("@/components/RoomOverviewTable.vue"));
const ConnectionStatusTable = defineAsyncComponent(() => import("@/components/ConnectionStatusTable.vue"));
const AuditFilters = defineAsyncComponent(() => import("@/components/AuditFilters.vue"));
const AuditTable = defineAsyncComponent(() => import("@/components/AuditTable.vue"));

const auditFilters = ref<AuditFiltersModel>({ ...DEFAULT_AUDIT_FILTERS });
const metricsFilters = ref<MetricsFilters>({ ...DEFAULT_METRICS_FILTERS });
const isLoading = ref(true);
const loadError = ref<string | null>(null);
const dailyMetricsLoading = ref(false);
const hourlyMetricsLoading = ref(false);
let metricsRefreshVersion = 0;

const { overview, roomOptions, applyOverview } = useOverviewState();
const { dailyMetrics, hourlyMetrics, applyDailyMetrics, applyHourlyMetrics } = useMetricsState();
const { auditPayload, eventTypes, applyAudit } = useAuditState();

const dashboardFilters = computed<DashboardFilters>(() => ({
  audit: auditFilters.value,
  metrics: metricsFilters.value,
}));

function markLoaded() {
  isLoading.value = false;
  loadError.value = null;
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
  applyAudit(payload.audit);
  dailyMetricsLoading.value = false;
  hourlyMetricsLoading.value = false;
  markLoaded();
}

async function fallbackBootstrap() {
  try {
    applyBootstrap(await loadAdminBootstrap(dashboardFilters.value));
  } catch (error) {
    loadError.value = error instanceof Error ? error.message : "bootstrap_failed";
    isLoading.value = false;
  }
}

async function refreshMetricsOnly() {
  const refreshVersion = ++metricsRefreshVersion;
  const nextFilters = { ...metricsFilters.value };
  dailyMetricsLoading.value = true;
  hourlyMetricsLoading.value = true;
  try {
    const [daily, hourly] = await Promise.all([
      fetchDailyMetrics(nextFilters),
      fetchHourlyMetrics(nextFilters),
    ]);
    if (refreshVersion !== metricsRefreshVersion) {
      return;
    }
    applyDailyMetrics(daily);
    applyHourlyMetrics(hourly);
  } finally {
    if (refreshVersion === metricsRefreshVersion) {
      dailyMetricsLoading.value = false;
      hourlyMetricsLoading.value = false;
    }
  }
}

async function refreshAuditOnly() {
  applyAudit(await fetchAudit(auditFilters.value));
}

const { status, lastHeartbeatAt, connect, restart, waitForBootstrap } = useAdminSse({
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
  onAudit: applyAudit,
});

watch(
  auditFilters,
  async () => {
    try {
      await refreshAuditOnly();
    } finally {
      restart();
    }
  },
  { deep: true },
);

watch(
  metricsFilters,
  async () => {
    try {
      await refreshMetricsOnly();
    } finally {
      restart();
    }
  },
  { deep: true },
);

onMounted(async () => {
  connect();
  const receivedBootstrap = await waitForBootstrap(1200);
  if (!receivedBootstrap) {
    await fallbackBootstrap();
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

const heroTags = computed(() => [
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
  restart();
  const receivedBootstrap = await waitForBootstrap(1200);
  if (!receivedBootstrap) {
    await fallbackBootstrap();
  }
}

function updateAuditFilters(value: AuditFiltersModel) {
  auditFilters.value = value;
}

function updateMetricsFilters(value: MetricsFilters) {
  metricsFilters.value = value;
}
</script>

<template>
  <div class="app-shell">
    <header class="hero-panel">
      <div>
        <span class="hero-eyebrow">只读后台</span>
        <h1>TeamViewRelay Admin</h1>
        <p>
          统一查看在线概况、房间状态、连接详情、最近 DAU、小时活跃，以及实时审计日志。
        </p>
      </div>
      <div class="hero-actions">
        <el-tag :type="status === 'live' ? 'success' : status === 'reconnecting' ? 'warning' : 'info'" round>
          {{ liveStatusLabel }}
        </el-tag>
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
          @update:model-value="updateAuditFilters"
          @refresh="refreshAuditOnly"
        />
        <AuditTable :audit="auditPayload" />
      </section>
    </template>
  </div>
</template>

<style scoped>
.audit-stack {
  display: grid;
  gap: 16px;
}
</style>

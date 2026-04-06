<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";

import { fetchAudit } from "@/api";
import AuditFilters from "@/components/AuditFilters.vue";
import AuditTable from "@/components/AuditTable.vue";
import ConnectionStatusTable from "@/components/ConnectionStatusTable.vue";
import MetricChartCard from "@/components/MetricChartCard.vue";
import OverviewCards from "@/components/OverviewCards.vue";
import RoomOverviewTable from "@/components/RoomOverviewTable.vue";
import { loadAdminBootstrap } from "@/composables/useAdminBootstrap";
import { useAdminSse } from "@/composables/useAdminSse";
import type {
  AuditFilters as AuditFiltersModel,
  AuditPayload,
  BootstrapPayload,
  MetricsPayload,
  OverviewPayload,
} from "@/types";
import { DEFAULT_AUDIT_FILTERS } from "@/types";

const overview = ref<OverviewPayload | null>(null);
const dailyMetrics = ref<MetricsPayload | null>(null);
const hourlyMetrics = ref<MetricsPayload | null>(null);
const auditPayload = ref<AuditPayload | null>(null);
const auditFilters = ref<AuditFiltersModel>({ ...DEFAULT_AUDIT_FILTERS });
const isLoading = ref(true);
const loadError = ref<string | null>(null);
const eventTypes = ref<string[]>([]);

function mergeAuditEventTypes(payload: AuditPayload | null) {
  const merged = new Set(eventTypes.value);
  for (const item of payload?.items ?? []) {
    if (item.eventType) {
      merged.add(item.eventType);
    }
  }
  eventTypes.value = [...merged].sort();
}

function applyOverview(payload: OverviewPayload) {
  overview.value = payload;
}

function applyDailyMetrics(payload: MetricsPayload) {
  dailyMetrics.value = payload;
}

function applyHourlyMetrics(payload: MetricsPayload) {
  hourlyMetrics.value = payload;
}

function applyAudit(payload: AuditPayload) {
  auditPayload.value = payload;
  mergeAuditEventTypes(payload);
}

function applyBootstrap(payload: BootstrapPayload) {
  applyOverview(payload.overview);
  applyDailyMetrics(payload.dailyMetrics);
  applyHourlyMetrics(payload.hourlyMetrics);
  applyAudit(payload.audit);
}

async function refreshAuditOnly() {
  applyAudit(await fetchAudit(auditFilters.value));
}

async function bootstrap() {
  isLoading.value = true;
  loadError.value = null;
  try {
    applyBootstrap(await loadAdminBootstrap(auditFilters.value));
  } catch (error) {
    loadError.value = error instanceof Error ? error.message : "bootstrap_failed";
  } finally {
    isLoading.value = false;
  }
}

const { status, lastHeartbeatAt, connect, restart, stop } = useAdminSse({
  getAuditFilters: () => auditFilters.value,
  onBootstrap: applyBootstrap,
  onOverview: applyOverview,
  onDailyMetrics: applyDailyMetrics,
  onHourlyMetrics: applyHourlyMetrics,
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

onMounted(async () => {
  await bootstrap();
  connect();
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

const heroTags = computed(() => [
  `时区 ${overview.value?.timezone ?? "-"}`,
  `数据库 ${overview.value?.dbPathMasked ?? "-"}`,
  `广播 ${overview.value?.broadcastHz ?? "-"} Hz`,
  `状态 ${liveStatusLabel.value}`,
  `心跳 ${lastHeartbeatAt.value ? new Date(lastHeartbeatAt.value).toLocaleTimeString("zh-CN", { hour12: false }) : "-"}`,
]);

async function handleManualRefresh() {
  await bootstrap();
  restart();
}

function updateAuditFilters(value: AuditFiltersModel) {
  auditFilters.value = value;
}
</script>

<template>
  <div class="app-shell">
    <header class="hero-panel">
      <div>
        <span class="hero-eyebrow">只读后台</span>
        <h1>TeamViewRelay Admin</h1>
        <p>
          统一查看在线概况、房间状态、连接详情、最近 30 天 DAU、最近 48 小时活跃，以及实时审计日志。
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

      <section class="two-column-grid">
        <MetricChartCard
          title="最近 30 天 DAU"
          description="按 submitPlayerId 去重统计本地自然日活跃玩家，空桶自动补零。"
          :metrics="dailyMetrics"
        />
        <MetricChartCard
          title="最近 48 小时活跃"
          description="按本地时区整点统计小时桶内的唯一活跃玩家，空桶自动补零。"
          :metrics="hourlyMetrics"
        />
      </section>

      <RoomOverviewTable :overview="overview" />
      <ConnectionStatusTable :overview="overview" />

      <section class="audit-stack">
        <AuditFilters :model-value="auditFilters" :event-types="eventTypes" @update:model-value="updateAuditFilters" @refresh="refreshAuditOnly" />
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

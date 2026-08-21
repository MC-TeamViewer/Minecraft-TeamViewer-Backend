<script setup lang="ts">
import ElButton from "element-plus/es/components/button/index";
import ElCard from "element-plus/es/components/card/index";
import ElPagination from "element-plus/es/components/pagination/index";
import { ElOption, ElSelect } from "element-plus/es/components/select/index";
import { ElTable, ElTableColumn } from "element-plus/es/components/table/index";
import { ElTabPane, ElTabs } from "element-plus/es/components/tabs/index";
import { computed, ref, watch } from "vue";

import { fetchRuntimeState } from "@/api";
import type { RuntimeStateItem, RuntimeStateKind, RuntimeStatePayload } from "@/types";

const props = defineProps<{
  refreshKey: number;
}>();

const activeKind = ref<RuntimeStateKind>("tab-reports");
const roomCode = ref("");
const page = ref(1);
const pageSize = 50;
const payload = ref<RuntimeStatePayload | null>(null);
const loading = ref(false);

const tabs: Array<{ label: string; value: RuntimeStateKind }> = [
  { label: "Tab 上报", value: "tab-reports" },
  { label: "玩家", value: "players" },
  { label: "实体", value: "entities" },
  { label: "路标", value: "waypoints" },
  { label: "战局区块", value: "battle-chunks" },
  { label: "玩家标记", value: "player-marks" },
];

const rows = computed(() => payload.value?.items ?? []);
const rooms = computed(() => payload.value?.availableRooms ?? []);

function formatTimestamp(value: number | null | undefined): string {
  if (!value) return "-";
  return new Date(value).toLocaleString("zh-CN", { hour12: false });
}

function formatJson(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2);
  } catch (_error) {
    return "{}";
  }
}

function summary(row: RuntimeStateItem): string {
  const data = row.data;
  if (activeKind.value === "tab-reports") return `玩家 ${data.playerCount ?? 0} 人`;
  if (activeKind.value === "players" || activeKind.value === "entities") {
    const name = data.playerName ?? data.entityName ?? data.entityType ?? "-";
    const coords = [data.x, data.y, data.z].every((value) => typeof value === "number")
      ? `${Number(data.x).toFixed(1)}, ${Number(data.y).toFixed(1)}, ${Number(data.z).toFixed(1)}`
      : "-";
    return `${name} · ${data.dimension ?? "-"} · ${coords}`;
  }
  if (activeKind.value === "waypoints") return `${data.name ?? "-"} · ${data.dimension ?? "-"}`;
  if (activeKind.value === "battle-chunks") return `${data.dimension ?? "-"} · (${data.chunkX ?? "-"}, ${data.chunkZ ?? "-"})`;
  return `${data.team ?? "neutral"} · ${data.label ?? "-"} · ${data.color ?? "-"}`;
}

async function load() {
  loading.value = true;
  try {
    const next = await fetchRuntimeState(activeKind.value, {
      roomCode: roomCode.value,
      page: page.value,
      pageSize,
    });
    payload.value = next;
    if (!roomCode.value && next.roomCode) {
      roomCode.value = next.roomCode;
    }
  } finally {
    loading.value = false;
  }
}

function changeTab(kind: string | number) {
  activeKind.value = kind as RuntimeStateKind;
  page.value = 1;
}

function changeRoom() {
  page.value = 1;
  void load();
}

function changePage(nextPage: number) {
  page.value = nextPage;
  void load();
}

watch([activeKind, page], () => {
  void load();
});
watch(() => props.refreshKey, () => {
  void load();
}, { immediate: true });
</script>

<template>
  <el-card shadow="never" class="surface-card">
    <template #header>
      <div class="section-header">
        <div>
          <h2>实时业务状态</h2>
          <p>按房间查看当前有效业务状态；页面会通过 SSE 自动更新，所有记录均为只读。</p>
        </div>
        <el-button plain :loading="loading" @click="load">刷新当前页</el-button>
      </div>
    </template>

    <div class="audit-filter-row data-manager-toolbar">
      <el-select v-model="roomCode" clearable filterable placeholder="默认房间" class="filter-control" @change="changeRoom">
        <el-option label="默认房间" value="" />
        <el-option v-for="room in rooms" :key="room" :label="room" :value="room" />
      </el-select>
    </div>

    <el-tabs :model-value="activeKind" @update:model-value="changeTab">
      <el-tab-pane v-for="tab in tabs" :key="tab.value" :label="tab.label" :name="tab.value" />
    </el-tabs>

    <el-table :data="rows" :loading="loading" border row-key="id" table-layout="fixed" class="admin-table" empty-text="暂无实时数据">
      <el-table-column type="expand" width="44">
        <template #default="{ row }"><pre class="audit-detail">{{ formatJson(row.data) }}</pre></template>
      </el-table-column>
      <el-table-column prop="id" label="ID" min-width="260" show-overflow-tooltip />
      <el-table-column prop="sourceId" label="来源" min-width="200" show-overflow-tooltip>
        <template #default="{ row }">{{ row.sourceId || "-" }}</template>
      </el-table-column>
      <el-table-column label="关键数据" min-width="360" show-overflow-tooltip>
        <template #default="{ row }">{{ summary(row) }}</template>
      </el-table-column>
      <el-table-column label="更新时间" min-width="180">
        <template #default="{ row }">{{ formatTimestamp(row.reportedAtUtcMs) }}</template>
      </el-table-column>
    </el-table>

    <div class="audit-pagination">
      <el-pagination
        layout="total, prev, pager, next"
        :total="payload?.total ?? 0"
        :page-size="pageSize"
        :current-page="page"
        background
        @update:current-page="changePage"
      />
    </div>
  </el-card>
</template>

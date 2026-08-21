<script setup lang="ts">
import ElButton from "element-plus/es/components/button/index";
import ElCard from "element-plus/es/components/card/index";
import ElInput from "element-plus/es/components/input/index";
import ElPagination from "element-plus/es/components/pagination/index";
import { ElOption, ElSelect } from "element-plus/es/components/select/index";
import { ElTable, ElTableColumn } from "element-plus/es/components/table/index";
import { ElMessage, ElMessageBox } from "element-plus";
import { computed, ref, watch } from "vue";

import {
  deleteLastSeenHistory,
  deleteTabHistory,
  fetchLastSeenHistory,
  fetchTabHistory,
} from "@/api";
import type {
  AdminPagePayload,
  HistoryQuery,
  LastSeenHistoryRecord,
  TabHistoryRecord,
} from "@/types";

type HistoryKind = "last-seen" | "tab";
type HistoryRecord = LastSeenHistoryRecord | TabHistoryRecord;

const props = defineProps<{
  kind: HistoryKind;
  refreshKey: number;
}>();

const query = ref<HistoryQuery>({
  roomCode: "",
  search: "",
  page: 1,
  pageSize: 50,
});
const payload = ref<AdminPagePayload<HistoryRecord> | null>(null);
const selected = ref<HistoryRecord[]>([]);
const loading = ref(false);
const deleting = ref(false);

const isLastSeen = computed(() => props.kind === "last-seen");
const title = computed(() => (isLastSeen.value ? "历史玩家下线位置" : "Tab 历史记录"));
const description = computed(() => (
  isLastSeen.value
    ? "逐条检查外部源上报的下线位置。删除只影响当前记录，后续有效上报仍会重新出现。"
    : "按房间保存的最新 Tab 标签镜像。删除会生成同步 tombstone，客户端可正确移除该条记录。"
));
const rows = computed(() => payload.value?.items ?? []);
const rooms = computed(() => payload.value?.availableRooms ?? []);

function rowKey(row: HistoryRecord): string {
  if (isLastSeen.value) {
    const record = row as LastSeenHistoryRecord;
    return `${record.roomCode}:${record.sourceId}:${record.playerUuid}`;
  }
  const record = row as TabHistoryRecord;
  return `${record.roomCode}:${record.playerUuid}`;
}

function formatTimestamp(value: number | null | undefined): string {
  if (!value) return "-";
  return new Date(value).toLocaleString("zh-CN", { hour12: false });
}

function formatCoordinates(row: LastSeenHistoryRecord): string {
  const values = [row.x, row.y, row.z];
  return values.every((value) => typeof value === "number")
    ? values.map((value) => Number(value).toFixed(2)).join(", ")
    : "-";
}

function tabValue(row: HistoryRecord, key: string): string {
  const player = (row as TabHistoryRecord).player;
  const value = player?.[key];
  return typeof value === "string" && value ? value : "-";
}

function formatJson(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2);
  } catch (_error) {
    return "{}";
  }
}

async function load() {
  loading.value = true;
  try {
    payload.value = isLastSeen.value
      ? await fetchLastSeenHistory(query.value)
      : await fetchTabHistory(query.value);
    selected.value = [];
  } catch (error) {
    ElMessage.error(error instanceof Error ? error.message : "加载历史数据失败");
  } finally {
    loading.value = false;
  }
}

function submitQuery() {
  query.value.page = 1;
  void load();
}

function changePage(page: number) {
  query.value.page = page;
  void load();
}

function changeSelection(value: HistoryRecord[]) {
  selected.value = value;
}

async function deleteSelected() {
  if (!selected.value.length) return;
  const count = selected.value.length;
  try {
    await ElMessageBox.confirm(
      `确认删除选中的 ${count} 条${isLastSeen.value ? "下线位置" : "Tab 历史"}记录？此操作不可恢复。`,
      "删除历史数据",
      { confirmButtonText: "删除", cancelButtonText: "取消", type: "warning" },
    );
  } catch (_error) {
    return;
  }

  deleting.value = true;
  try {
    const result = isLastSeen.value
      ? await deleteLastSeenHistory(selected.value.map((row) => {
        const record = row as LastSeenHistoryRecord;
        return { roomCode: record.roomCode, sourceId: record.sourceId, playerUuid: record.playerUuid };
      }))
      : await deleteTabHistory(selected.value.map((row) => {
        const record = row as TabHistoryRecord;
        return { roomCode: record.roomCode, playerUuid: record.playerUuid };
      }));
    ElMessage.success(`已删除 ${result.deleted} 条记录${result.missing ? `，${result.missing} 条不存在` : ""}`);
    if (rows.value.length <= result.deleted && query.value.page > 1) {
      query.value.page -= 1;
    }
    await load();
  } catch (error) {
    ElMessage.error(error instanceof Error ? error.message : "删除历史数据失败");
  } finally {
    deleting.value = false;
  }
}

watch(() => props.refreshKey, () => {
  void load();
}, { immediate: true });
</script>

<template>
  <el-card shadow="never" class="surface-card">
    <template #header>
      <div class="section-header">
        <div>
          <h2>{{ title }}</h2>
          <p>{{ description }}</p>
        </div>
        <el-button type="danger" :disabled="!selected.length" :loading="deleting" @click="deleteSelected">
          删除所选（{{ selected.length }}）
        </el-button>
      </div>
    </template>

    <div class="audit-filter-row data-manager-toolbar">
      <el-select v-model="query.roomCode" clearable filterable placeholder="全部房间" class="filter-control">
        <el-option label="全部房间" value="" />
        <el-option v-for="room in rooms" :key="room" :label="room" :value="room" />
      </el-select>
      <el-input
        v-model="query.search"
        clearable
        class="filter-control"
        placeholder="搜索 UUID、名称、来源或维度"
        @keyup.enter="submitQuery"
      />
      <el-button type="primary" plain :loading="loading" @click="submitQuery">查询</el-button>
    </div>

    <el-table
      :data="rows"
      :row-key="rowKey"
      :loading="loading"
      border
      table-layout="fixed"
      class="admin-table"
      empty-text="暂无记录"
      @selection-change="changeSelection"
    >
      <el-table-column type="selection" width="48" />
      <el-table-column type="expand" width="44">
        <template #default="{ row }">
          <pre class="audit-detail">{{ formatJson(isLastSeen ? row : row.player) }}</pre>
        </template>
      </el-table-column>

      <template v-if="isLastSeen">
        <el-table-column prop="roomCode" label="房间" min-width="130" show-overflow-tooltip />
        <el-table-column prop="sourceId" label="来源" min-width="200" show-overflow-tooltip />
        <el-table-column prop="playerName" label="玩家" min-width="140" show-overflow-tooltip />
        <el-table-column prop="playerUuid" label="UUID" min-width="260" show-overflow-tooltip />
        <el-table-column label="维度 / 坐标" min-width="240">
          <template #default="{ row }">
            {{ row.dimension || "-" }} · {{ formatCoordinates(row) }}
          </template>
        </el-table-column>
        <el-table-column label="下线判定" min-width="180">
          <template #default="{ row }">{{ formatTimestamp(row.offlineDetectedAtUtcMs) }}</template>
        </el-table-column>
        <el-table-column label="最后出现" min-width="180">
          <template #default="{ row }">{{ formatTimestamp(row.lastSeenAtUtcMs) }}</template>
        </el-table-column>
      </template>

      <template v-else>
        <el-table-column prop="roomCode" label="房间" min-width="130" show-overflow-tooltip />
        <el-table-column prop="playerUuid" label="UUID" min-width="260" show-overflow-tooltip />
        <el-table-column label="名称" min-width="160" show-overflow-tooltip>
          <template #default="{ row }">{{ tabValue(row, "name") }}</template>
        </el-table-column>
        <el-table-column label="显示标签" min-width="200" show-overflow-tooltip>
          <template #default="{ row }">{{ tabValue(row, "displayName") }}</template>
        </el-table-column>
        <el-table-column label="队伍前缀" min-width="170" show-overflow-tooltip>
          <template #default="{ row }">{{ tabValue(row, "scoreboardPrefix") }}</template>
        </el-table-column>
        <el-table-column label="最后观测" min-width="180">
          <template #default="{ row }">{{ formatTimestamp(row.lastObservedAtUtcMs) }}</template>
        </el-table-column>
        <el-table-column prop="revision" label="Revision" width="110" />
      </template>
    </el-table>

    <div class="audit-pagination">
      <el-pagination
        layout="total, prev, pager, next"
        :total="payload?.total ?? 0"
        :page-size="query.pageSize"
        :current-page="query.page"
        background
        @update:current-page="changePage"
      />
    </div>
  </el-card>
</template>

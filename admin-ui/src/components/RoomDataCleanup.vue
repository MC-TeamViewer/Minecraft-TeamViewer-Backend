<script setup lang="ts">
import ElAlert from "element-plus/es/components/alert/index";
import ElButton from "element-plus/es/components/button/index";
import ElCard from "element-plus/es/components/card/index";
import { ElOption, ElSelect } from "element-plus/es/components/select/index";
import ElTag from "element-plus/es/components/tag/index";
import { ElMessage, ElMessageBox } from "element-plus";
import { computed, onMounted, ref } from "vue";

import { ApiError, fetchRoomDataMaintenance, purgeRoomData } from "@/api";
import type { RoomDataMaintenancePayload, RoomDataSummary } from "@/types";

const emit = defineEmits<{
  purged: [roomCode: string];
}>();

const payload = ref<RoomDataMaintenancePayload | null>(null);
const selectedRoom = ref("");
const loading = ref(false);
const deleting = ref(false);

const selected = computed<RoomDataSummary | null>(() => (
  payload.value?.items.find((item) => item.roomCode === selectedRoom.value) ?? null
));
const activeConnections = computed(() => selected.value?.activeConnections ?? 0);
const counts = computed(() => {
  const item = selected.value;
  if (!item) return [];
  return [
    ["审计日志", item.auditEvents],
    ["日活跃行", item.dailyActivity],
    ["小时活跃行", item.hourlyActivity],
    ["Tab 条目", item.tabHistoryEntries],
    ["Tab 增量", item.tabHistoryDeltas],
    ["Tab 修订", item.tabHistoryRevisions],
    ["Tab Head", item.tabHistoryHeads],
    ["Last Seen", item.lastSeenRecords],
    ["孤立身份映射", item.identityMappings],
  ] as Array<[string, number]>;
});

function errorMessage(error: unknown, action: string): string {
  if (error instanceof ApiError) {
    if (error.detail === "room_has_active_connections") return "房间仍有在线连接，无法清理。";
    if (error.detail === "room_confirmation_mismatch") return "确认房间名不匹配。";
    if (error.detail === "room_maintenance") return "该房间已有清理任务正在执行。";
    return `${action}失败（HTTP ${error.status}：${error.detail ?? "未知错误"}）`;
  }
  return error instanceof Error ? error.message : `${action}失败`;
}

async function load(preferredRoom = selectedRoom.value) {
  loading.value = true;
  try {
    payload.value = await fetchRoomDataMaintenance();
    const rooms = payload.value.items.map((item) => item.roomCode);
    const suggested = [...payload.value.items]
      .sort((left, right) => right.totalRemovable - left.totalRemovable)
      .find((item) => item.activeConnections === 0)?.roomCode;
    selectedRoom.value = rooms.includes(preferredRoom)
      ? preferredRoom
      : (suggested ?? rooms[0] ?? preferredRoom);
  } catch (error) {
    ElMessage.error(errorMessage(error, "加载房间清理摘要"));
  } finally {
    loading.value = false;
  }
}

async function purge() {
  const room = selectedRoom.value.trim();
  if (!room || activeConnections.value > 0) return;
  let confirmation = "";
  try {
    const result = await ElMessageBox.prompt(
      `此操作会永久删除房间“${room}”可归属的审计、活跃指标、Tab History、Last Seen 与孤立身份映射。请输入完整房间名确认。`,
      "清理房间数据",
      {
        confirmButtonText: "永久删除",
        cancelButtonText: "取消",
        type: "warning",
        inputPlaceholder: room,
        inputValidator: (value: string) => value.trim() === room || "输入必须与房间名完全一致",
      },
    );
    confirmation = result.value;
  } catch (_error) {
    return;
  }

  deleting.value = true;
  try {
    const result = await purgeRoomData(room, confirmation);
    const total = result.deleted.totalDeleted ?? Object.values(result.deleted).reduce((sum, value) => sum + value, 0);
    ElMessage.success(`房间 ${room} 已清理，共移除 ${total} 项；全局历史流量已保留。`);
    await load(room);
    emit("purged", room);
  } catch (error) {
    ElMessage.error(errorMessage(error, "清理房间数据"));
    await load(room);
  } finally {
    deleting.value = false;
  }
}

onMounted(() => {
  void load();
});
</script>

<template>
  <el-card shadow="never" class="surface-card">
    <template #header>
      <div class="section-header">
        <div>
          <h2>按房间清理数据</h2>
          <p>用于清除压测或废弃房间留下的可归属数据。在线房间不会被删除。</p>
        </div>
        <el-button :loading="loading" plain @click="load()">刷新摘要</el-button>
      </div>
    </template>

    <el-alert
      type="warning"
      :closable="false"
      show-icon
      title="历史流量按全局聚合，无法按房间区分，因此清理房间时不会删除任何流量桶。"
    />

    <div class="room-cleanup-toolbar">
      <el-select
        v-model="selectedRoom"
        filterable
        allow-create
        default-first-option
        placeholder="选择或输入房间名"
        class="filter-control"
      >
        <el-option
          v-for="item in payload?.items ?? []"
          :key="item.roomCode"
          :label="`${item.roomCode}（${item.totalRemovable} 项）`"
          :value="item.roomCode"
        />
      </el-select>
      <el-tag v-if="selectedRoom" :type="activeConnections ? 'danger' : 'success'" round>
        {{ activeConnections ? `${activeConnections} 个在线连接` : "房间离线" }}
      </el-tag>
      <el-button
        type="danger"
        :disabled="!selectedRoom.trim() || activeConnections > 0"
        :loading="deleting"
        @click="purge"
      >
        永久清理该房间
      </el-button>
    </div>

    <div v-if="selected" class="cleanup-count-grid">
      <div v-for="([label, value]) in counts" :key="label" class="cleanup-count-item">
        <span>{{ label }}</span>
        <strong>{{ value }}</strong>
      </div>
      <div class="cleanup-count-item cleanup-total">
        <span>预计可移除</span>
        <strong>{{ selected.totalRemovable }}</strong>
      </div>
    </div>
    <p v-else-if="selectedRoom" class="muted-copy">
      这是未出现在摘要中的房间。后端仍会检查在线连接，并以幂等方式清理已有数据。
    </p>
  </el-card>
</template>

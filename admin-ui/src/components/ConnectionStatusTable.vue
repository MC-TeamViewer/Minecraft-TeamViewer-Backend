<script setup lang="ts">
import { computed } from "vue";

import { connectionColumns } from "@/connectionColumns";
import type { ConnectionDetail, OverviewPayload } from "@/types";

const props = defineProps<{
  overview: OverviewPayload | null;
}>();

const rows = computed(() => props.overview?.connectionDetails ?? []);

function formatChannel(value: ConnectionDetail["channel"]): string {
  if (value === "player") {
    return "游戏端";
  }
  if (value === "web_map") {
    return "网页端";
  }
  return value || "-";
}
</script>

<template>
  <el-card shadow="never" class="surface-card">
    <template #header>
      <div class="section-header">
        <div>
          <h2>当前连接状态</h2>
          <p>只统计已经完成 WebSocket 握手并登记到服务端内存态的连接。</p>
        </div>
      </div>
    </template>

    <section class="status-summary-grid">
      <div class="status-summary-card">
        <span class="status-summary-label">当前总连接</span>
        <strong class="status-summary-value">
          {{ (overview?.playerConnections ?? 0) + (overview?.webMapConnections ?? 0) }}
        </strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">游戏端</span>
        <strong class="status-summary-value">{{ overview?.playerConnections ?? 0 }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">网页端</span>
        <strong class="status-summary-value">{{ overview?.webMapConnections ?? 0 }}</strong>
      </div>
      <div class="status-summary-card">
        <span class="status-summary-label">活跃房间</span>
        <strong class="status-summary-value">{{ overview?.activeRooms ?? 0 }}</strong>
      </div>
    </section>

    <el-table
      :data="rows"
      border
      table-layout="fixed"
      class="admin-table"
      empty-text="暂无连接"
      row-key="actorId"
    >
      <el-table-column type="expand" width="44">
        <template #default="{ row }">
          <div class="expanded-detail-grid">
            <div><span class="detail-key">显示名</span><span>{{ row.displayName || "-" }}</span></div>
            <div><span class="detail-key">房间</span><span>{{ row.roomCode || "-" }}</span></div>
            <div><span class="detail-key">协议版本</span><span>{{ row.protocolVersion || "-" }}</span></div>
            <div><span class="detail-key">程序版本</span><span>{{ row.programVersion || "-" }}</span></div>
            <div><span class="detail-key">远端地址</span><span>{{ row.remoteAddr || "-" }}</span></div>
            <div><span class="detail-key">连接 ID</span><span class="mono-text">{{ row.actorId || "-" }}</span></div>
          </div>
        </template>
      </el-table-column>
      <el-table-column
        v-for="column in connectionColumns"
        :key="column.prop"
        :prop="column.prop"
        :label="column.label"
        :min-width="column.minWidth"
        :width="column.width"
        :show-overflow-tooltip="column.showOverflowTooltip"
        resizable
      >
        <template v-if="column.prop === 'channel'" #default="{ row }">
          {{ formatChannel(row.channel) }}
        </template>
        <template v-else #default="{ row }">
          <span :class="{ 'mono-text': column.prop === 'actorId' }">{{ row[column.prop] || "-" }}</span>
        </template>
      </el-table-column>
    </el-table>
  </el-card>
</template>

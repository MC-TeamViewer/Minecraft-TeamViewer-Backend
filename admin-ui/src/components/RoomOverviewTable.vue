<script setup lang="ts">
import { computed } from "vue";

import type { OverviewPayload } from "@/types";

const props = defineProps<{
  overview: OverviewPayload | null;
}>();

const rows = computed(() => props.overview?.rooms ?? []);
</script>

<template>
  <el-card shadow="never" class="surface-card">
    <template #header>
      <div class="section-header">
        <div>
          <h2>房间概览</h2>
          <p>按当前已登记到内存态的连接汇总房间视图。</p>
        </div>
      </div>
    </template>

    <el-table
      :data="rows"
      table-layout="fixed"
      class="admin-table"
      empty-text="暂无连接房间"
      border
    >
      <el-table-column prop="roomCode" label="房间" min-width="180" show-overflow-tooltip resizable />
      <el-table-column prop="playerConnections" label="玩家连接" width="110" resizable />
      <el-table-column prop="webMapConnections" label="网页端" width="100" resizable />
      <el-table-column label="玩家 ID" min-width="280" show-overflow-tooltip resizable>
        <template #default="{ row }">
          <span>{{ row.playerIds?.join(", ") || "-" }}</span>
        </template>
      </el-table-column>
      <el-table-column label="Web Map ID" min-width="240" show-overflow-tooltip resizable>
        <template #default="{ row }">
          <span>{{ row.webMapIds?.join(", ") || "-" }}</span>
        </template>
      </el-table-column>
    </el-table>
  </el-card>
</template>

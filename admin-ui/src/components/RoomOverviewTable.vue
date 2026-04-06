<script setup lang="ts">
import ElCard from "element-plus/es/components/card/index";
import { ElTable, ElTableColumn } from "element-plus/es/components/table/index";
import { computed } from "vue";

import { summarizeRoomIds } from "@/roomOverview";
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
      <el-table-column type="expand" width="44">
        <template #default="{ row }">
          <div class="expanded-detail-grid room-overview-expanded-grid">
            <div>
              <span class="detail-key">玩家 ID</span>
              <div v-if="row.playerIds?.length" class="room-id-list">
                <span v-for="playerId in row.playerIds" :key="playerId" class="room-id-chip mono-text">{{ playerId }}</span>
              </div>
              <span v-else>-</span>
            </div>
            <div>
              <span class="detail-key">Web Map ID</span>
              <div v-if="row.webMapIds?.length" class="room-id-list">
                <span v-for="webMapId in row.webMapIds" :key="webMapId" class="room-id-chip mono-text">{{ webMapId }}</span>
              </div>
              <span v-else>-</span>
            </div>
          </div>
        </template>
      </el-table-column>
      <el-table-column prop="roomCode" label="房间" min-width="180" show-overflow-tooltip resizable />
      <el-table-column prop="playerConnections" label="玩家连接" width="110" resizable />
      <el-table-column prop="webMapConnections" label="网页端" width="100" resizable />
      <el-table-column label="玩家 ID" width="120" resizable>
        <template #default="{ row }">
          <span>{{ summarizeRoomIds(row.playerIds) }}</span>
        </template>
      </el-table-column>
      <el-table-column label="Web Map ID" width="130" resizable>
        <template #default="{ row }">
          <span>{{ summarizeRoomIds(row.webMapIds) }}</span>
        </template>
      </el-table-column>
    </el-table>
  </el-card>
</template>

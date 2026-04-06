<script setup lang="ts">
import { computed } from "vue";

import type { OverviewPayload } from "@/types";

const props = defineProps<{
  overview: OverviewPayload | null;
}>();

const cards = computed(() => [
  {
    label: "在线玩家",
    value: props.overview?.playerConnections ?? "-",
  },
  {
    label: "在线 Web Map",
    value: props.overview?.webMapConnections ?? "-",
  },
  {
    label: "活跃房间",
    value: props.overview?.activeRooms ?? "-",
  },
  {
    label: "24h 峰值",
    value: props.overview?.hourlyPeak24h ?? "-",
  },
]);
</script>

<template>
  <section class="card-grid">
    <el-card v-for="card in cards" :key="card.label" shadow="never" class="surface-card">
      <div class="metric-label">{{ card.label }}</div>
      <div class="metric-value">{{ card.value }}</div>
    </el-card>
  </section>
</template>

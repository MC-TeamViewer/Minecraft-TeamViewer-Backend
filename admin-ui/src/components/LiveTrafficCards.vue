<script setup lang="ts">
import ElCard from "element-plus/es/components/card/index";
import { computed } from "vue";

import { formatRateValue } from "@/charts";
import type { LiveTrafficPayload } from "@/types";

const props = defineProps<{
  traffic: LiveTrafficPayload | null;
}>();

const cards = computed(() => [
  {
    label: "近 10 秒平均入站",
    value: props.traffic ? formatRateValue(props.traffic.totalIngressBps) : "-",
  },
  {
    label: "近 10 秒平均出站",
    value: props.traffic ? formatRateValue(props.traffic.totalEgressBps) : "-",
  },
]);

const splitTags = computed(() => {
  if (!props.traffic) {
    return [];
  }
  return [
    `游戏端入站 ${formatRateValue(props.traffic.playerIngressBps)}`,
    `游戏端出站 ${formatRateValue(props.traffic.playerEgressBps)}`,
    `网页端入站 ${formatRateValue(props.traffic.webMapIngressBps)}`,
    `网页端出站 ${formatRateValue(props.traffic.webMapEgressBps)}`,
  ];
});
</script>

<template>
  <section class="traffic-live-stack">
    <div class="card-grid two-up">
      <el-card v-for="card in cards" :key="card.label" shadow="never" class="surface-card">
        <div class="metric-label">{{ card.label }}</div>
        <div class="metric-value traffic-value">{{ card.value }}</div>
      </el-card>
    </div>
    <el-card shadow="never" class="surface-card">
      <div class="section-header">
        <div>
          <h2>方向拆分</h2>
          <p>核心业务 WebSocket 流量，不包含管理页 HTTP 与 SSE。</p>
        </div>
      </div>
      <div class="hero-tag-row">
        <span v-for="tag in splitTags" :key="tag" class="hero-tag">{{ tag }}</span>
      </div>
    </el-card>
  </section>
</template>

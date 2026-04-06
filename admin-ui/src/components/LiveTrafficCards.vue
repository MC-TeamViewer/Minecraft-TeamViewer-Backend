<script setup lang="ts">
import ElCard from "element-plus/es/components/card/index";
import { ElSegmented } from "element-plus/es/components/segmented/index";
import { computed } from "vue";

import { formatByteValue, formatRateValue } from "@/charts";
import { TRAFFIC_LAYER_LABELS, TRAFFIC_LAYER_OPTIONS } from "@/types";
import type { LiveTrafficPayload, TrafficLayer } from "@/types";

const props = defineProps<{
  traffic: LiveTrafficPayload | null;
  selectedLayer: TrafficLayer;
}>();

const emit = defineEmits<{
  "update:selectedLayer": [value: TrafficLayer];
}>();

const activeLayer = computed(() => props.traffic?.[props.selectedLayer] ?? null);

const cards = computed(() => [
  {
    label: "近 10 秒平均入站",
    value: activeLayer.value ? formatRateValue(activeLayer.value.totalIngressBps) : "-",
  },
  {
    label: "近 10 秒平均出站",
    value: activeLayer.value ? formatRateValue(activeLayer.value.totalEgressBps) : "-",
  },
]);

const splitTags = computed(() => {
  if (!activeLayer.value) {
    return [];
  }
  return [
    `游戏端入站 ${formatRateValue(activeLayer.value.playerIngressBps)}`,
    `游戏端出站 ${formatRateValue(activeLayer.value.playerEgressBps)}`,
    `网页端入站 ${formatRateValue(activeLayer.value.webMapIngressBps)}`,
    `网页端出站 ${formatRateValue(activeLayer.value.webMapEgressBps)}`,
  ];
});

const compareTags = computed(() => {
  if (!props.traffic) {
    return [];
  }
  const applicationTotal =
    props.traffic.application.totalIngressBps + props.traffic.application.totalEgressBps;
  const wireTotal = props.traffic.wire.totalIngressBps + props.traffic.wire.totalEgressBps;
  if (applicationTotal <= 0) {
    return [];
  }
  const compressionRatio = wireTotal / applicationTotal;
  const savedBytesPerSec = applicationTotal - wireTotal;
  return [
    `当前口径 ${TRAFFIC_LAYER_LABELS[props.selectedLayer]}`,
    `压缩率 ${(compressionRatio * 100).toFixed(1)}%`,
    `节省 ${formatByteValue(savedBytesPerSec)}/s`,
  ];
});
</script>

<template>
  <section class="traffic-live-stack">
    <div class="section-header">
      <div>
        <h2>实时流量</h2>
        <p>近 10 秒平均速率，可在应用层与实际 WS 传输层之间切换。</p>
      </div>
      <el-segmented
        :model-value="selectedLayer"
        :options="TRAFFIC_LAYER_OPTIONS"
        @update:model-value="(value: TrafficLayer) => emit('update:selectedLayer', value)"
      />
    </div>
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
      <div v-if="compareTags.length" class="hero-tag-row section-gap">
        <span v-for="tag in compareTags" :key="tag" class="hero-tag">{{ tag }}</span>
      </div>
    </el-card>
  </section>
</template>

<script setup lang="ts">
import { BarChart } from "echarts/charts";
import {
  GridComponent,
  LegendComponent,
  TitleComponent,
  TooltipComponent,
} from "echarts/components";
import { CanvasRenderer } from "echarts/renderers";
import * as echarts from "echarts/core";
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";

import { buildBarChartOption } from "@/charts";
import type { MetricsPayload } from "@/types";

echarts.use([BarChart, GridComponent, LegendComponent, TitleComponent, TooltipComponent, CanvasRenderer]);

const props = defineProps<{
  title: string;
  description: string;
  metrics: MetricsPayload | null;
}>();

const chartEl = ref<HTMLDivElement | null>(null);
let chart: echarts.ECharts | null = null;
let resizeObserver: ResizeObserver | null = null;

const metaText = computed(() => {
  if (!props.metrics) {
    return "加载中";
  }
  const latestItem = props.metrics.items[props.metrics.items.length - 1];
  const latest = latestItem?.activePlayers ?? 0;
  const max = Math.max(...props.metrics.items.map((item) => item.activePlayers), 0);
  return `${props.metrics.timezone} · 最新 ${latest} · 峰值 ${max}`;
});

const renderChart = () => {
  if (!chartEl.value) {
    return;
  }
  if (!chart) {
    chart = echarts.init(chartEl.value);
  }
  chart.setOption(buildBarChartOption(props.metrics), true);
  chart.resize();
};

onMounted(() => {
  renderChart();
  if (chartEl.value) {
    resizeObserver = new ResizeObserver(() => {
      chart?.resize();
    });
    resizeObserver.observe(chartEl.value);
  }
});

watch(
  () => props.metrics,
  () => {
    renderChart();
  },
  { deep: true },
);

onBeforeUnmount(() => {
  resizeObserver?.disconnect();
  chart?.dispose();
});
</script>

<template>
  <el-card shadow="never" class="surface-card chart-card">
    <template #header>
      <div class="section-header">
        <div>
          <h2>{{ title }}</h2>
          <p>{{ description }}</p>
        </div>
        <span class="chart-meta">{{ metaText }}</span>
      </div>
    </template>
    <div ref="chartEl" class="chart-host" />
  </el-card>
</template>

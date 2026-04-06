<script setup lang="ts">
import ElCard from "element-plus/es/components/card/index";
import { BarChart } from "echarts/charts";
import { GridComponent, TitleComponent, TooltipComponent } from "echarts/components";
import * as echarts from "echarts/core";
import { CanvasRenderer } from "echarts/renderers";
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";

import { buildBarChartOption } from "@/charts";
import type { MetricsPayload } from "@/types";

const props = defineProps<{
  title: string;
  description: string;
  metrics: MetricsPayload | null;
  loading?: boolean;
}>();

echarts.use([BarChart, GridComponent, TitleComponent, TooltipComponent, CanvasRenderer]);

const chartEl = ref<HTMLDivElement | null>(null);
let chart: echarts.ECharts | null = null;
let resizeObserver: ResizeObserver | null = null;
let intersectionObserver: IntersectionObserver | null = null;
const isVisible = ref(false);
let chartSignature = "";

const metaText = computed(() => {
  if (props.loading) {
    return "筛选更新中";
  }
  if (!props.metrics) {
    return "加载中";
  }
  const latestItem = props.metrics.items[props.metrics.items.length - 1];
  const latest = latestItem?.activePlayers ?? 0;
  const max = Math.max(...props.metrics.items.map((item) => item.activePlayers), 0);
  return `${props.metrics.timezone} · 最新 ${latest} · 峰值 ${max}`;
});

function buildSignature(metrics: MetricsPayload | null): string {
  return JSON.stringify({
    roomCode: metrics?.roomCode ?? "",
    days: metrics?.days ?? null,
    hours: metrics?.hours ?? null,
    itemLength: metrics?.items.length ?? 0,
  });
}

function disposeChart() {
  if (chart) {
    chart.dispose();
    chart = null;
  }
}

function ensureChart() {
  if (!chart && chartEl.value) {
    chart = echarts.init(chartEl.value);
  }
}

const renderChart = async (forceRebuild = false) => {
  if (!isVisible.value || !chartEl.value) {
    return;
  }
  if (props.loading || !props.metrics) {
    chart?.clear();
    return;
  }

  const nextSignature = buildSignature(props.metrics);
  if (forceRebuild || nextSignature !== chartSignature) {
    disposeChart();
    chartSignature = nextSignature;
  }

  ensureChart();
  chart?.clear();
  chart?.setOption(buildBarChartOption(props.metrics), true);
  chart?.resize();
};

onMounted(() => {
  if (chartEl.value) {
    intersectionObserver = new IntersectionObserver((entries) => {
      const visible = entries.some((entry) => entry.isIntersecting);
      if (visible && !isVisible.value) {
        isVisible.value = true;
        void renderChart(true);
      }
    });
    intersectionObserver.observe(chartEl.value);
  }
  if (chartEl.value) {
    resizeObserver = new ResizeObserver(() => {
      chart?.resize();
    });
    resizeObserver.observe(chartEl.value);
  }
});

watch(
  () => [props.metrics, props.loading] as const,
  () => {
    void renderChart();
  },
  { deep: true },
);

onBeforeUnmount(() => {
  intersectionObserver?.disconnect();
  resizeObserver?.disconnect();
  disposeChart();
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
    <div ref="chartEl" class="chart-host">
      <div v-if="!isVisible || loading || !metrics" class="chart-placeholder chart-placeholder-overlay">
        {{ !isVisible ? "图表进入可视区域后加载" : loading ? "筛选更新中" : "暂无图表数据" }}
      </div>
    </div>
  </el-card>
</template>

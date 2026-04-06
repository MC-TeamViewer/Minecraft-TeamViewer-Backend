<script setup lang="ts">
import ElCard from "element-plus/es/components/card/index";
import { LineChart } from "echarts/charts";
import { GridComponent, LegendComponent, TooltipComponent } from "echarts/components";
import * as echarts from "echarts/core";
import { CanvasRenderer } from "echarts/renderers";
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";

import { buildTrafficChartOption, formatByteValue } from "@/charts";
import { TRAFFIC_GRANULARITY_LABELS, TRAFFIC_RANGE_OPTIONS } from "@/types";
import type { TrafficHistoryPayload } from "@/types";

const props = defineProps<{
  title: string;
  description: string;
  metrics: TrafficHistoryPayload | null;
}>();

echarts.use([LineChart, GridComponent, LegendComponent, TooltipComponent, CanvasRenderer]);

const chartEl = ref<HTMLDivElement | null>(null);
let chart: echarts.ECharts | null = null;
let resizeObserver: ResizeObserver | null = null;
let intersectionObserver: IntersectionObserver | null = null;
const isVisible = ref(false);
let chartSignature = "";

const metaText = computed(() => {
  if (!props.metrics) {
    return "加载中";
  }
  const rangeLabel = TRAFFIC_RANGE_OPTIONS.find((item) => item.value === props.metrics?.range)?.label ?? props.metrics.range;
  const granularityLabel = TRAFFIC_GRANULARITY_LABELS[props.metrics.granularity] ?? props.metrics.granularity;
  return `${rangeLabel} · ${granularityLabel} · 总流量 ${formatByteValue(props.metrics.totalBytes)}`;
});

function buildSignature(metrics: TrafficHistoryPayload | null): string {
  return JSON.stringify({
    range: metrics?.range ?? null,
    granularity: metrics?.granularity ?? null,
    bucketSeconds: metrics?.bucketSeconds ?? null,
    itemLength: metrics?.items.length ?? 0,
    totalBytes: metrics?.totalBytes ?? 0,
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
  if (!props.metrics) {
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
  chart?.setOption(buildTrafficChartOption(props.metrics), true);
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
    resizeObserver = new ResizeObserver(() => {
      chart?.resize();
    });
    resizeObserver.observe(chartEl.value);
  }
});

watch(
  () => props.metrics,
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
      <div v-if="!isVisible || !metrics" class="chart-placeholder chart-placeholder-overlay">
        {{ !isVisible ? "图表进入可视区域后加载" : "暂无图表数据" }}
      </div>
    </div>
  </el-card>
</template>

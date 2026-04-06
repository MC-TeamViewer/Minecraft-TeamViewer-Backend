<script setup lang="ts">
import ElCard from "element-plus/es/components/card/index";
import { ElSegmented } from "element-plus/es/components/segmented/index";
import { LineChart } from "echarts/charts";
import { GridComponent, LegendComponent, TooltipComponent } from "echarts/components";
import * as echarts from "echarts/core";
import { CanvasRenderer } from "echarts/renderers";
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";

import { buildTrafficChartOption, formatByteValue } from "@/charts";
import {
  TRAFFIC_GRANULARITY_LABELS,
  TRAFFIC_HISTORY_DISPLAY_LABELS,
  TRAFFIC_MIXED_VIEW_OPTIONS,
  TRAFFIC_RANGE_OPTIONS,
} from "@/types";
import type { TrafficHistoryDisplayMode, TrafficHistoryPayload, TrafficMixedViewMode } from "@/types";

const props = defineProps<{
  title: string;
  description: string;
  metrics: TrafficHistoryPayload | null;
  selectedMode: TrafficHistoryDisplayMode;
  mixedViewMode: TrafficMixedViewMode;
}>();

const emit = defineEmits<{
  "update:mixedViewMode": [value: TrafficMixedViewMode];
}>();

echarts.use([LineChart, GridComponent, LegendComponent, TooltipComponent, CanvasRenderer]);

const chartEl = ref<HTMLDivElement | null>(null);
let chart: echarts.ECharts | null = null;
let resizeObserver: ResizeObserver | null = null;
let intersectionObserver: IntersectionObserver | null = null;
const isVisible = ref(false);
let chartSignature = "";

const selectedMetrics = computed(() => {
  if (!props.metrics) {
    return null;
  }
  if (props.selectedMode === "mixed") {
    return {
      totalBytes: Math.max(props.metrics.application.totalBytes, props.metrics.wire.totalBytes),
    };
  }
  return props.metrics[props.selectedMode];
});

const metaText = computed(() => {
  const metrics = props.metrics;
  const layerMetrics = selectedMetrics.value;
  if (!metrics || !layerMetrics) {
    return "加载中";
  }
  const rangeLabel = TRAFFIC_RANGE_OPTIONS.find((item) => item.value === metrics.range)?.label ?? metrics.range;
  const granularityLabel = TRAFFIC_GRANULARITY_LABELS[metrics.granularity] ?? metrics.granularity;
  const layerLabel = TRAFFIC_HISTORY_DISPLAY_LABELS[props.selectedMode] ?? props.selectedMode;
  if (props.selectedMode === "mixed") {
    const mixedLabel = props.mixedViewMode === "total" ? "总量对比" : "分流量细则";
    return `${rangeLabel} · ${granularityLabel} · ${layerLabel} · ${mixedLabel} · 应用层 ${formatByteValue(metrics.application.totalBytes)} · 传输层 ${formatByteValue(metrics.wire.totalBytes)}`;
  }
  return `${rangeLabel} · ${granularityLabel} · ${layerLabel} · 总流量 ${formatByteValue(layerMetrics.totalBytes)}`;
});

const compareSummary = computed(() => {
  const metrics = props.metrics;
  if (!metrics) {
    return null;
  }
  const wireItems = metrics.wire.items;
  const applicationItems = metrics.application.items;
  const firstWireIndex = wireItems.findIndex((item) => item.totalBytes > 0);
  if (firstWireIndex < 0) {
    return null;
  }

  let applicationComparableTotal = 0;
  let wireComparableTotal = 0;
  for (let index = firstWireIndex; index < Math.min(applicationItems.length, wireItems.length); index += 1) {
    applicationComparableTotal += applicationItems[index]?.totalBytes ?? 0;
    wireComparableTotal += wireItems[index]?.totalBytes ?? 0;
  }

  if (applicationComparableTotal <= 0) {
    return null;
  }

  return {
    applicationTotal: applicationComparableTotal,
    wireTotal: wireComparableTotal,
    savedBytes: applicationComparableTotal - wireComparableTotal,
    ratio: wireComparableTotal / applicationComparableTotal,
    comparisonStartBucket: wireItems[firstWireIndex]?.bucket ?? null,
    partialCoverage: firstWireIndex > 0,
  };
});

const compareTags = computed(() => {
  const summary = compareSummary.value;
  if (!summary) {
    return [];
  }
  const tags = [
    `压缩率 ${(summary.ratio * 100).toFixed(1)}%`,
    `节省 ${formatByteValue(summary.savedBytes)}`,
  ];
  if (summary.partialCoverage && summary.comparisonStartBucket) {
    tags.push(`对比区间自 ${summary.comparisonStartBucket} 起`);
  }
  return tags;
});

function buildSignature(
  metrics: TrafficHistoryPayload | null,
  mode: TrafficHistoryDisplayMode,
  mixedView: TrafficMixedViewMode,
): string {
  return JSON.stringify({
    range: metrics?.range ?? null,
    granularity: metrics?.granularity ?? null,
    startAt: metrics?.startAt ?? null,
    bucketSeconds: metrics?.bucketSeconds ?? null,
    mode,
    mixedView,
    applicationItems: metrics?.application.items.length ?? 0,
    wireItems: metrics?.wire.items.length ?? 0,
    applicationTotal: metrics?.application.totalBytes ?? 0,
    wireTotal: metrics?.wire.totalBytes ?? 0,
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

  const nextSignature = buildSignature(props.metrics, props.selectedMode, props.mixedViewMode);
  if (forceRebuild || nextSignature !== chartSignature) {
    disposeChart();
    chartSignature = nextSignature;
  }

  ensureChart();
  chart?.clear();
  chart?.setOption(buildTrafficChartOption(props.metrics, props.selectedMode, props.mixedViewMode), true);
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
  () => [props.metrics, props.selectedMode, props.mixedViewMode],
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
      <div v-if="selectedMode === 'mixed'" class="section-gap">
        <el-segmented
          :model-value="mixedViewMode"
          :options="TRAFFIC_MIXED_VIEW_OPTIONS"
          @update:model-value="(value: TrafficMixedViewMode) => emit('update:mixedViewMode', value)"
        />
      </div>
      <div v-if="compareTags.length" class="hero-tag-row section-gap">
        <span v-for="tag in compareTags" :key="tag" class="hero-tag">{{ tag }}</span>
      </div>
    </template>
    <div ref="chartEl" class="chart-host">
      <div v-if="!isVisible || !metrics" class="chart-placeholder chart-placeholder-overlay">
        {{ !isVisible ? "图表进入可视区域后加载" : "暂无图表数据" }}
      </div>
    </div>
    <div
      v-if="metrics && metrics.wire.totalBytes <= 0 && selectedMode !== 'application'"
      class="chart-placeholder section-gap"
    >
      该口径从当前版本部署后开始统计。
    </div>
  </el-card>
</template>

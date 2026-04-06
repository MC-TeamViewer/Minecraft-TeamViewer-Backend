import type { EChartsCoreOption } from "echarts/core";

import type { MetricsPayload } from "@/types";

function formatAxisLabel(bucket: string, metrics: MetricsPayload | null): string {
  if (!bucket) {
    return "";
  }
  if (metrics?.hours != null || bucket.includes("T")) {
    return bucket.replace("T", " ").slice(5, 16);
  }
  if (metrics?.days != null || /^\d{4}-\d{2}-\d{2}$/.test(bucket)) {
    return bucket.slice(5, 10);
  }
  return bucket;
}

export function buildBarChartOption(metrics: MetricsPayload | null): EChartsCoreOption {
  const items = metrics?.items ?? [];
  const labels = items.map((item) => formatAxisLabel(item.bucket || item.label, metrics));
  const values = metrics?.items.map((item) => item.activePlayers) ?? [];

  return {
    animationDuration: 220,
    color: ["#c26a18"],
    grid: {
      left: 18,
      right: 16,
      top: 24,
      bottom: 18,
      containLabel: true,
    },
    tooltip: {
      trigger: "axis",
      axisPointer: {
        type: "shadow",
      },
      formatter(params: unknown) {
        const firstParam = (Array.isArray(params) ? params[0] : params) as { dataIndex?: number } | undefined;
        const item = items[firstParam?.dataIndex ?? -1];
        if (!item) {
          return "";
        }
        return `${item.bucket}<br/>活跃玩家: ${item.activePlayers}`;
      },
    },
    xAxis: {
      type: "category",
      data: labels,
      axisTick: {
        alignWithLabel: true,
      },
      axisLabel: {
        hideOverlap: true,
        interval: "auto",
        color: "#6b7280",
      },
    },
    yAxis: {
      type: "value",
      minInterval: 1,
      axisLabel: {
        color: "#6b7280",
      },
      splitLine: {
        lineStyle: {
          color: "rgba(120, 53, 15, 0.12)",
        },
      },
    },
    series: [
      {
        type: "bar",
        barMaxWidth: 28,
        data: values,
        itemStyle: {
          borderRadius: [8, 8, 0, 0],
        },
      },
    ],
  };
}

import type { EChartsCoreOption } from "echarts/core";

import type { MetricsPayload } from "@/types";

export function buildBarChartOption(metrics: MetricsPayload | null): EChartsCoreOption {
  const labels = metrics?.items.map((item) => item.label) ?? [];
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

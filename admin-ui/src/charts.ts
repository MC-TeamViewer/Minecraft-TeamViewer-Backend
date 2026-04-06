import type { EChartsCoreOption } from "echarts/core";

import type {
  MetricsPayload,
  TrafficHistoryPayload,
  TrafficHistoryDisplayMode,
  TrafficMixedViewMode,
} from "@/types";

function formatAxisLabel(bucket: string, metrics: MetricsPayload | TrafficHistoryPayload | null): string {
  if (!bucket) {
    return "";
  }
  const hasHours = metrics != null && "hours" in metrics && metrics.hours != null;
  const hasDays = metrics != null && "days" in metrics && metrics.days != null;
  if (hasHours || bucket.includes("T")) {
    return bucket.replace("T", " ").slice(5, 16);
  }
  if (hasDays || /^\d{4}-\d{2}-\d{2}$/.test(bucket)) {
    return bucket.slice(5, 10);
  }
  return bucket;
}

export function formatByteValue(value: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let amount = Math.max(0, Number(value) || 0);
  let index = 0;
  while (amount >= 1024 && index < units.length - 1) {
    amount /= 1024;
    index += 1;
  }
  const fixed = amount >= 100 || index === 0 ? 0 : amount >= 10 ? 1 : 2;
  return `${amount.toFixed(fixed)} ${units[index]}`;
}

export function formatRateValue(value: number): string {
  return `${formatByteValue(value)}/s`;
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

export function buildTrafficChartOption(
  metrics: TrafficHistoryPayload | null,
  mode: TrafficHistoryDisplayMode,
  mixedView: TrafficMixedViewMode = "total",
): EChartsCoreOption {
  const items =
    mode === "mixed"
      ? metrics?.application.items ?? metrics?.wire.items ?? []
      : metrics?.[mode]?.items ?? [];
  const labels = items.map((item) => formatAxisLabel(item.bucket || item.label, metrics));
  const series =
    mode === "mixed" && mixedView === "total"
      ? [
          {
            name: "应用层总流量",
            type: "line",
            smooth: true,
            color: "#c26a18",
            lineStyle: {
              width: 3,
            },
            data: metrics?.application.items.map((item) => item.totalBytes) ?? [],
          },
          {
            name: "传输层总流量",
            type: "line",
            smooth: true,
            color: "#1d4ed8",
            lineStyle: {
              width: 3,
              type: "dashed",
            },
            data: metrics?.wire.items.map((item) => item.totalBytes) ?? [],
          },
        ]
      : mode === "mixed"
      ? [
          {
            name: "应用层 游戏端入站",
            type: "line",
            smooth: true,
            color: "#c26a18",
            lineStyle: {
              width: 3,
            },
            data: metrics?.application.items.map((item) => item.playerIngressBytes) ?? [],
          },
          {
            name: "传输层 游戏端入站",
            type: "line",
            smooth: true,
            color: "#c26a18",
            lineStyle: {
              width: 3,
              type: "dashed",
            },
            data: metrics?.wire.items.map((item) => item.playerIngressBytes) ?? [],
          },
          {
            name: "应用层 游戏端出站",
            type: "line",
            smooth: true,
            color: "#92400e",
            lineStyle: {
              width: 3,
            },
            data: metrics?.application.items.map((item) => item.playerEgressBytes) ?? [],
          },
          {
            name: "传输层 游戏端出站",
            type: "line",
            smooth: true,
            color: "#92400e",
            lineStyle: {
              width: 3,
              type: "dashed",
            },
            data: metrics?.wire.items.map((item) => item.playerEgressBytes) ?? [],
          },
          {
            name: "应用层 网页端入站",
            type: "line",
            smooth: true,
            color: "#0f766e",
            lineStyle: {
              width: 3,
            },
            data: metrics?.application.items.map((item) => item.webMapIngressBytes) ?? [],
          },
          {
            name: "传输层 网页端入站",
            type: "line",
            smooth: true,
            color: "#0f766e",
            lineStyle: {
              width: 3,
              type: "dashed",
            },
            data: metrics?.wire.items.map((item) => item.webMapIngressBytes) ?? [],
          },
          {
            name: "应用层 网页端出站",
            type: "line",
            smooth: true,
            color: "#1d4ed8",
            lineStyle: {
              width: 3,
            },
            data: metrics?.application.items.map((item) => item.webMapEgressBytes) ?? [],
          },
          {
            name: "传输层 网页端出站",
            type: "line",
            smooth: true,
            color: "#1d4ed8",
            lineStyle: {
              width: 3,
              type: "dashed",
            },
            data: metrics?.wire.items.map((item) => item.webMapEgressBytes) ?? [],
          },
        ]
      : [
          {
            name: "游戏端入站",
            type: "line",
            smooth: true,
            data: items.map((item) => item.playerIngressBytes),
          },
          {
            name: "游戏端出站",
            type: "line",
            smooth: true,
            data: items.map((item) => item.playerEgressBytes),
          },
          {
            name: "网页端入站",
            type: "line",
            smooth: true,
            data: items.map((item) => item.webMapIngressBytes),
          },
          {
            name: "网页端出站",
            type: "line",
            smooth: true,
            data: items.map((item) => item.webMapEgressBytes),
          },
        ];

  return {
    animationDuration: 220,
    color:
      mode === "mixed" && mixedView === "total"
        ? ["#c26a18", "#1d4ed8"]
        : mode === "mixed"
        ? ["#c26a18", "#c26a18", "#92400e", "#92400e", "#0f766e", "#0f766e", "#1d4ed8", "#1d4ed8"]
        : ["#c26a18", "#92400e", "#0f766e", "#1d4ed8"],
    grid: {
      left: 18,
      right: 16,
      top: 24,
      bottom: 18,
      containLabel: true,
    },
    legend: {
      top: 0,
      textStyle: {
        color: "#6b7280",
      },
    },
    tooltip: {
      trigger: "axis",
      formatter(params: unknown) {
        const entries = (Array.isArray(params) ? params : [params]) as Array<{ seriesName?: string; value?: number }>;
        const index = Number((entries[0] as { dataIndex?: number } | undefined)?.dataIndex ?? -1);
        const item = items[index];
        if (!item) {
          return "";
        }
        const lines = [
          item.bucket,
        ];
        if (mode === "mixed") {
          lines.push(`应用层总流量: ${formatByteValue(metrics?.application.items[index]?.totalBytes ?? 0)}`);
          lines.push(`传输层总流量: ${formatByteValue(metrics?.wire.items[index]?.totalBytes ?? 0)}`);
          if (mixedView === "total") {
            for (const entry of entries) {
              if (!entry.seriesName) {
                continue;
              }
              lines.push(`${entry.seriesName}: ${formatByteValue(Number(entry.value ?? 0))}`);
            }
            return lines.join("<br/>");
          }
          for (const entry of entries) {
            if (!entry.seriesName) {
              continue;
            }
            lines.push(`${entry.seriesName}: ${formatByteValue(Number(entry.value ?? 0))}`);
          }
          return lines.join("<br/>");
        }
        lines.push(`总流量: ${formatByteValue(item.totalBytes)}`);
        for (const entry of entries) {
          if (!entry.seriesName) {
            continue;
          }
          lines.push(`${entry.seriesName}: ${formatByteValue(Number(entry.value ?? 0))}`);
        }
        return lines.join("<br/>");
      },
    },
    xAxis: {
      type: "category",
      data: labels,
      axisLabel: {
        hideOverlap: true,
        interval: "auto",
        color: "#6b7280",
      },
    },
    yAxis: {
      type: "value",
      axisLabel: {
        color: "#6b7280",
        formatter(value: number) {
          return formatByteValue(value);
        },
      },
      splitLine: {
        lineStyle: {
          color: "rgba(120, 53, 15, 0.12)",
        },
      },
    },
    series,
  };
}

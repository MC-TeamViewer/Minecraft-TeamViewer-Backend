import { buildBarChartOption, buildTrafficChartOption, formatByteValue } from "@/charts";
import type { MetricsPayload, TrafficHistoryPayload } from "@/types";

describe("buildBarChartOption", () => {
  it("formats daily labels for a shorter x-axis while keeping series data", () => {
    const metrics: MetricsPayload = {
      timezone: "CST (UTC+08:00)",
      roomCode: null,
      days: 2,
      items: [
        { bucket: "2026-04-06", label: "2026-04-06", activePlayers: 4 },
        { bucket: "2026-04-07", label: "2026-04-07", activePlayers: 1 },
      ],
    };

    const option = buildBarChartOption(metrics);
    const xAxis = Array.isArray(option.xAxis) ? option.xAxis[0] : option.xAxis;
    const series = Array.isArray(option.series) ? option.series[0] : option.series;

    expect(xAxis?.data).toEqual(["04-06", "04-07"]);
    expect(series?.data).toEqual([4, 1]);
  });

  it("formats hourly labels differently from daily labels", () => {
    const metrics: MetricsPayload = {
      timezone: "CST (UTC+08:00)",
      roomCode: "room-alpha",
      hours: 2,
      items: [
        { bucket: "2026-04-06T00:00:00", label: "2026-04-06T00:00:00", activePlayers: 2 },
        { bucket: "2026-04-06T01:00:00", label: "2026-04-06T01:00:00", activePlayers: 5 },
      ],
    };

    const option = buildBarChartOption(metrics);
    const xAxis = Array.isArray(option.xAxis) ? option.xAxis[0] : option.xAxis;

    expect(xAxis?.data).toEqual(["04-06 00:00", "04-06 01:00"]);
  });
});

describe("buildTrafficChartOption", () => {
  it("formats traffic labels and series values", () => {
    const metrics: TrafficHistoryPayload = {
      timezone: "CST (UTC+08:00)",
      range: "48h",
      granularity: "1h",
      bucketSeconds: 3600,
      selectedLayer: "application",
      application: {
        totalIngressBytes: 250,
        totalEgressBytes: 200,
        totalBytes: 450,
        items: [
          {
            bucket: "2026-04-06T00:00:00",
            label: "2026-04-06T00:00:00",
            playerIngressBytes: 100,
            playerEgressBytes: 50,
            webMapIngressBytes: 20,
            webMapEgressBytes: 30,
            totalIngressBytes: 120,
            totalEgressBytes: 80,
            totalBytes: 200,
          },
          {
            bucket: "2026-04-06T01:00:00",
            label: "2026-04-06T01:00:00",
            playerIngressBytes: 130,
            playerEgressBytes: 70,
            webMapIngressBytes: 0,
            webMapEgressBytes: 50,
            totalIngressBytes: 130,
            totalEgressBytes: 120,
            totalBytes: 250,
          },
        ],
      },
      wire: {
        totalIngressBytes: 200,
        totalEgressBytes: 160,
        totalBytes: 360,
        items: [],
      },
    };

    const option = buildTrafficChartOption(metrics, "application");
    const xAxis = Array.isArray(option.xAxis) ? option.xAxis[0] : option.xAxis;
    const series = Array.isArray(option.series) ? option.series : [];

    expect(xAxis?.data).toEqual(["04-06 00:00", "04-06 01:00"]);
    expect(series[0]?.data).toEqual([100, 130]);
    expect(series[3]?.data).toEqual([30, 50]);
  });
});

describe("formatByteValue", () => {
  it("formats bytes into human-readable units", () => {
    expect(formatByteValue(512)).toBe("512 B");
    expect(formatByteValue(2048)).toBe("2.00 KB");
  });
});

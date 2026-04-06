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

  it("uses different layer series when application and wire data differ", () => {
    const metrics: TrafficHistoryPayload = {
      timezone: "CST (UTC+08:00)",
      range: "48h",
      granularity: "1h",
      bucketSeconds: 3600,
      selectedLayer: "application",
      application: {
        totalIngressBytes: 450,
        totalEgressBytes: 250,
        totalBytes: 700,
        items: [
          {
            bucket: "2026-04-06T00:00:00",
            label: "2026-04-06T00:00:00",
            playerIngressBytes: 300,
            playerEgressBytes: 120,
            webMapIngressBytes: 40,
            webMapEgressBytes: 20,
            totalIngressBytes: 340,
            totalEgressBytes: 140,
            totalBytes: 480,
          },
          {
            bucket: "2026-04-06T01:00:00",
            label: "2026-04-06T01:00:00",
            playerIngressBytes: 110,
            playerEgressBytes: 90,
            webMapIngressBytes: 0,
            webMapEgressBytes: 20,
            totalIngressBytes: 110,
            totalEgressBytes: 110,
            totalBytes: 220,
          },
        ],
      },
      wire: {
        totalIngressBytes: 260,
        totalEgressBytes: 160,
        totalBytes: 420,
        items: [
          {
            bucket: "2026-04-06T00:00:00",
            label: "2026-04-06T00:00:00",
            playerIngressBytes: 160,
            playerEgressBytes: 80,
            webMapIngressBytes: 10,
            webMapEgressBytes: 10,
            totalIngressBytes: 170,
            totalEgressBytes: 90,
            totalBytes: 260,
          },
          {
            bucket: "2026-04-06T01:00:00",
            label: "2026-04-06T01:00:00",
            playerIngressBytes: 70,
            playerEgressBytes: 60,
            webMapIngressBytes: 20,
            webMapEgressBytes: 10,
            totalIngressBytes: 90,
            totalEgressBytes: 70,
            totalBytes: 160,
          },
        ],
      },
    };

    const applicationOption = buildTrafficChartOption(metrics, "application");
    const wireOption = buildTrafficChartOption(metrics, "wire");
    const applicationSeries = Array.isArray(applicationOption.series) ? applicationOption.series : [];
    const wireSeries = Array.isArray(wireOption.series) ? wireOption.series : [];

    expect(applicationSeries[0]?.data).toEqual([300, 110]);
    expect(wireSeries[0]?.data).toEqual([160, 70]);
    expect(applicationSeries[0]?.data).not.toEqual(wireSeries[0]?.data);
    expect(applicationSeries[1]?.data).not.toEqual(wireSeries[1]?.data);
  });

  it("builds mixed-mode total series on the same axis scale by default", () => {
    const metrics: TrafficHistoryPayload = {
      timezone: "CST (UTC+08:00)",
      range: "48h",
      granularity: "1h",
      bucketSeconds: 3600,
      selectedLayer: "application",
      application: {
        totalIngressBytes: 450,
        totalEgressBytes: 250,
        totalBytes: 700,
        items: [
          {
            bucket: "2026-04-06T00:00:00",
            label: "2026-04-06T00:00:00",
            playerIngressBytes: 300,
            playerEgressBytes: 120,
            webMapIngressBytes: 40,
            webMapEgressBytes: 20,
            totalIngressBytes: 340,
            totalEgressBytes: 140,
            totalBytes: 480,
          },
          {
            bucket: "2026-04-06T01:00:00",
            label: "2026-04-06T01:00:00",
            playerIngressBytes: 110,
            playerEgressBytes: 90,
            webMapIngressBytes: 0,
            webMapEgressBytes: 20,
            totalIngressBytes: 110,
            totalEgressBytes: 110,
            totalBytes: 220,
          },
        ],
      },
      wire: {
        totalIngressBytes: 260,
        totalEgressBytes: 160,
        totalBytes: 420,
        items: [
          {
            bucket: "2026-04-06T00:00:00",
            label: "2026-04-06T00:00:00",
            playerIngressBytes: 160,
            playerEgressBytes: 80,
            webMapIngressBytes: 10,
            webMapEgressBytes: 10,
            totalIngressBytes: 170,
            totalEgressBytes: 90,
            totalBytes: 260,
          },
          {
            bucket: "2026-04-06T01:00:00",
            label: "2026-04-06T01:00:00",
            playerIngressBytes: 70,
            playerEgressBytes: 60,
            webMapIngressBytes: 20,
            webMapEgressBytes: 10,
            totalIngressBytes: 90,
            totalEgressBytes: 70,
            totalBytes: 160,
          },
        ],
      },
    };

    const option = buildTrafficChartOption(metrics, "mixed");
    const series = Array.isArray(option.series) ? option.series : [];

    expect(series).toHaveLength(2);
    expect(series[0]?.data).toEqual([480, 220]);
    expect(series[1]?.data).toEqual([260, 160]);
  });

  it("builds mixed-mode breakdown series when requested", () => {
    const metrics: TrafficHistoryPayload = {
      timezone: "CST (UTC+08:00)",
      range: "48h",
      granularity: "1h",
      bucketSeconds: 3600,
      selectedLayer: "application",
      application: {
        totalIngressBytes: 450,
        totalEgressBytes: 250,
        totalBytes: 700,
        items: [
          {
            bucket: "2026-04-06T00:00:00",
            label: "2026-04-06T00:00:00",
            playerIngressBytes: 300,
            playerEgressBytes: 120,
            webMapIngressBytes: 40,
            webMapEgressBytes: 20,
            totalIngressBytes: 340,
            totalEgressBytes: 140,
            totalBytes: 480,
          },
          {
            bucket: "2026-04-06T01:00:00",
            label: "2026-04-06T01:00:00",
            playerIngressBytes: 110,
            playerEgressBytes: 90,
            webMapIngressBytes: 0,
            webMapEgressBytes: 20,
            totalIngressBytes: 110,
            totalEgressBytes: 110,
            totalBytes: 220,
          },
        ],
      },
      wire: {
        totalIngressBytes: 260,
        totalEgressBytes: 160,
        totalBytes: 420,
        items: [
          {
            bucket: "2026-04-06T00:00:00",
            label: "2026-04-06T00:00:00",
            playerIngressBytes: 160,
            playerEgressBytes: 80,
            webMapIngressBytes: 10,
            webMapEgressBytes: 10,
            totalIngressBytes: 170,
            totalEgressBytes: 90,
            totalBytes: 260,
          },
          {
            bucket: "2026-04-06T01:00:00",
            label: "2026-04-06T01:00:00",
            playerIngressBytes: 70,
            playerEgressBytes: 60,
            webMapIngressBytes: 20,
            webMapEgressBytes: 10,
            totalIngressBytes: 90,
            totalEgressBytes: 70,
            totalBytes: 160,
          },
        ],
      },
    };

    const option = buildTrafficChartOption(metrics, "mixed", "breakdown");
    const series = Array.isArray(option.series) ? option.series : [];

    expect(series).toHaveLength(8);
    expect(series[0]?.data).toEqual([300, 110]);
    expect(series[1]?.data).toEqual([160, 70]);
    expect(series[2]?.data).toEqual([120, 90]);
    expect(series[3]?.data).toEqual([80, 60]);
    expect(series[4]?.data).toEqual([40, 0]);
    expect(series[5]?.data).toEqual([10, 20]);
    expect(series[6]?.data).toEqual([20, 20]);
    expect(series[7]?.data).toEqual([10, 10]);
  });

  it("returns different mixed-series output for total and breakdown views", () => {
    const metrics: TrafficHistoryPayload = {
      timezone: "CST (UTC+08:00)",
      range: "48h",
      granularity: "1h",
      bucketSeconds: 3600,
      selectedLayer: "application",
      application: {
        totalIngressBytes: 30,
        totalEgressBytes: 10,
        totalBytes: 40,
        items: [
          {
            bucket: "2026-04-06T00:00:00",
            label: "2026-04-06T00:00:00",
            playerIngressBytes: 10,
            playerEgressBytes: 6,
            webMapIngressBytes: 8,
            webMapEgressBytes: 2,
            totalIngressBytes: 18,
            totalEgressBytes: 8,
            totalBytes: 26,
          },
        ],
      },
      wire: {
        totalIngressBytes: 20,
        totalEgressBytes: 8,
        totalBytes: 28,
        items: [
          {
            bucket: "2026-04-06T00:00:00",
            label: "2026-04-06T00:00:00",
            playerIngressBytes: 7,
            playerEgressBytes: 5,
            webMapIngressBytes: 6,
            webMapEgressBytes: 2,
            totalIngressBytes: 13,
            totalEgressBytes: 7,
            totalBytes: 20,
          },
        ],
      },
    };

    const totalOption = buildTrafficChartOption(metrics, "mixed", "total");
    const breakdownOption = buildTrafficChartOption(metrics, "mixed", "breakdown");
    const totalSeries = Array.isArray(totalOption.series) ? totalOption.series : [];
    const breakdownSeries = Array.isArray(breakdownOption.series) ? breakdownOption.series : [];

    expect(totalSeries).toHaveLength(2);
    expect(breakdownSeries).toHaveLength(8);
    expect(totalSeries[0]?.data).not.toEqual(breakdownSeries[0]?.data);
  });
});

describe("formatByteValue", () => {
  it("formats bytes into human-readable units", () => {
    expect(formatByteValue(512)).toBe("512 B");
    expect(formatByteValue(2048)).toBe("2.00 KB");
  });
});

import { buildBarChartOption } from "@/charts";
import type { MetricsPayload } from "@/types";

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

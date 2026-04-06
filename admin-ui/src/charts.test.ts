import { buildBarChartOption } from "@/charts";
import type { MetricsPayload } from "@/types";

describe("buildBarChartOption", () => {
  it("uses backend labels directly for the x-axis", () => {
    const metrics: MetricsPayload = {
      timezone: "CST (UTC+08:00)",
      roomCode: null,
      items: [
        { bucket: "2026-04-06", label: "2026-04-06", activePlayers: 4 },
        { bucket: "2026-04-07", label: "2026-04-07", activePlayers: 1 },
      ],
    };

    const option = buildBarChartOption(metrics);
    const xAxis = Array.isArray(option.xAxis) ? option.xAxis[0] : option.xAxis;
    const series = Array.isArray(option.series) ? option.series[0] : option.series;

    expect(xAxis?.data).toEqual(["2026-04-06", "2026-04-07"]);
    expect(series?.data).toEqual([4, 1]);
  });
});

import { mount } from "@vue/test-utils";
import { nextTick } from "vue";

import type { MetricsPayload } from "@/types";

const echartsMocks = vi.hoisted(() => {
  const clearMock = vi.fn();
  const disposeMock = vi.fn();
  const resizeMock = vi.fn();
  const setOptionMock = vi.fn();
  const initMock = vi.fn(() => ({
    clear: clearMock,
    dispose: disposeMock,
    resize: resizeMock,
    setOption: setOptionMock,
  }));

  return {
    clearMock,
    disposeMock,
    resizeMock,
    setOptionMock,
    initMock,
  };
});

vi.mock("echarts/core", () => ({
  init: echartsMocks.initMock,
  use: vi.fn(),
}));

vi.mock("echarts/charts", () => ({
  BarChart: {},
}));

vi.mock("echarts/components", () => ({
  GridComponent: {},
  TitleComponent: {},
  TooltipComponent: {},
}));

vi.mock("echarts/renderers", () => ({
  CanvasRenderer: {},
}));

import MetricChartCard from "@/components/MetricChartCard.vue";

class ResizeObserverMock {
  observe() {}
  disconnect() {}
}

class IntersectionObserverMock {
  private readonly callback: IntersectionObserverCallback;

  constructor(callback: IntersectionObserverCallback) {
    this.callback = callback;
  }

  observe() {
    this.callback([{ isIntersecting: true } as IntersectionObserverEntry], this as unknown as IntersectionObserver);
  }

  disconnect() {}
}

function buildMetricsPayload(length: number, mode: "daily" | "hourly"): MetricsPayload {
  return {
    timezone: "CST (UTC+08:00)",
    roomCode: null,
    days: mode === "daily" ? length : undefined,
    hours: mode === "hourly" ? length : undefined,
    items: Array.from({ length }, (_value, index) => ({
      bucket: mode === "daily" ? `2026-04-${String(index + 1).padStart(2, "0")}` : `2026-04-06T${String(index).padStart(2, "0")}:00:00`,
      label: `${index}`,
      activePlayers: index + 1,
    })),
  };
}

describe("MetricChartCard", () => {
  beforeEach(() => {
    echartsMocks.clearMock.mockClear();
    echartsMocks.disposeMock.mockClear();
    echartsMocks.resizeMock.mockClear();
    echartsMocks.setOptionMock.mockClear();
    echartsMocks.initMock.mockClear();
    vi.stubGlobal("ResizeObserver", ResizeObserverMock);
    vi.stubGlobal("IntersectionObserver", IntersectionObserverMock);
  });

  it("rebuilds the chart when the bucket count changes", async () => {
    const wrapper = mount(MetricChartCard, {
      props: {
        title: "最近 30 天 DAU",
        description: "desc",
        metrics: buildMetricsPayload(30, "daily"),
        loading: false,
      },
    });

    await nextTick();
    await nextTick();

    expect(echartsMocks.setOptionMock).toHaveBeenCalled();
    const initialOption = echartsMocks.setOptionMock.mock.calls[echartsMocks.setOptionMock.mock.calls.length - 1]?.[0];
    const initialXAxis = Array.isArray(initialOption?.xAxis) ? initialOption.xAxis[0] : initialOption?.xAxis;
    expect(initialXAxis?.data).toHaveLength(30);

    await wrapper.setProps({
      metrics: buildMetricsPayload(7, "daily"),
    });
    await nextTick();
    await nextTick();

    const nextOption = echartsMocks.setOptionMock.mock.calls[echartsMocks.setOptionMock.mock.calls.length - 1]?.[0];
    const nextXAxis = Array.isArray(nextOption?.xAxis) ? nextOption.xAxis[0] : nextOption?.xAxis;
    expect(nextXAxis?.data).toHaveLength(7);
    expect(echartsMocks.disposeMock).toHaveBeenCalled();
  });
});

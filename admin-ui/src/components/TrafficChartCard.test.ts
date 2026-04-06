import { mount } from "@vue/test-utils";
import { nextTick } from "vue";

import TrafficChartCard from "@/components/TrafficChartCard.vue";
import type { TrafficHistoryPayload } from "@/types";

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
  LineChart: {},
}));

vi.mock("echarts/components", () => ({
  GridComponent: {},
  LegendComponent: {},
  TooltipComponent: {},
}));

vi.mock("echarts/renderers", () => ({
  CanvasRenderer: {},
}));

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

function buildTrafficPayload(): TrafficHistoryPayload {
  return {
    timezone: "CST (UTC+08:00)",
    range: "48h",
    granularity: "1h",
    bucketSeconds: 3600,
    selectedLayer: "application",
    application: {
      totalIngressBytes: 640,
      totalEgressBytes: 160,
      totalBytes: 800,
      items: [
        {
          bucket: "2026-04-06T00:00:00",
          label: "2026-04-06T00:00:00",
          playerIngressBytes: 300,
          playerEgressBytes: 80,
          webMapIngressBytes: 100,
          webMapEgressBytes: 20,
          totalIngressBytes: 400,
          totalEgressBytes: 100,
          totalBytes: 500,
        },
        {
          bucket: "2026-04-06T01:00:00",
          label: "2026-04-06T01:00:00",
          playerIngressBytes: 180,
          playerEgressBytes: 40,
          webMapIngressBytes: 60,
          webMapEgressBytes: 20,
          totalIngressBytes: 240,
          totalEgressBytes: 60,
          totalBytes: 300,
        },
      ],
    },
    wire: {
      totalIngressBytes: 320,
      totalEgressBytes: 80,
      totalBytes: 400,
      items: [
        {
          bucket: "2026-04-06T00:00:00",
          label: "2026-04-06T00:00:00",
          playerIngressBytes: 140,
          playerEgressBytes: 40,
          webMapIngressBytes: 60,
          webMapEgressBytes: 10,
          totalIngressBytes: 200,
          totalEgressBytes: 50,
          totalBytes: 250,
        },
        {
          bucket: "2026-04-06T01:00:00",
          label: "2026-04-06T01:00:00",
          playerIngressBytes: 70,
          playerEgressBytes: 20,
          webMapIngressBytes: 50,
          webMapEgressBytes: 10,
          totalIngressBytes: 120,
          totalEgressBytes: 30,
          totalBytes: 150,
        },
      ],
    },
  };
}

describe("TrafficChartCard", () => {
  beforeEach(() => {
    echartsMocks.clearMock.mockClear();
    echartsMocks.disposeMock.mockClear();
    echartsMocks.resizeMock.mockClear();
    echartsMocks.setOptionMock.mockClear();
    echartsMocks.initMock.mockClear();
    vi.stubGlobal("ResizeObserver", ResizeObserverMock);
    vi.stubGlobal("IntersectionObserver", IntersectionObserverMock);
  });

  it("switches chart series when the selected layer changes", async () => {
    const wrapper = mount(TrafficChartCard, {
      props: {
        title: "历史流量",
        description: "desc",
        metrics: buildTrafficPayload(),
        selectedLayer: "application",
      },
    });

    await nextTick();
    await nextTick();

    const initialOption = echartsMocks.setOptionMock.mock.calls[echartsMocks.setOptionMock.mock.calls.length - 1]?.[0];
    const initialSeries = Array.isArray(initialOption?.series) ? initialOption.series : [];
    expect(initialSeries[0]?.data).toEqual([300, 180]);

    await wrapper.setProps({
      selectedLayer: "wire",
    });
    await nextTick();
    await nextTick();

    const nextOption = echartsMocks.setOptionMock.mock.calls[echartsMocks.setOptionMock.mock.calls.length - 1]?.[0];
    const nextSeries = Array.isArray(nextOption?.series) ? nextOption.series : [];
    expect(nextSeries[0]?.data).toEqual([140, 70]);
    expect(nextSeries[0]?.data).not.toEqual(initialSeries[0]?.data);
    expect(echartsMocks.disposeMock).toHaveBeenCalled();
  });
});

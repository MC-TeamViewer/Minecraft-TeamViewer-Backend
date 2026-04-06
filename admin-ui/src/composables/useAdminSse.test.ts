import { effectScope } from "vue";

import { DEFAULT_AUDIT_FILTERS, DEFAULT_METRICS_FILTERS, DEFAULT_TRAFFIC_FILTERS } from "@/types";
import { buildAdminEventsUrl, useAdminSse, type EventSourceLike } from "@/composables/useAdminSse";

class MockEventSource implements EventSourceLike {
  static instances: MockEventSource[] = [];

  url: string;
  onerror: ((event: Event) => void) | null = null;
  listeners = new Map<string, Array<(event: MessageEvent<string>) => void>>();
  closed = false;

  constructor(url: string) {
    this.url = url;
    MockEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void {
    const entries = this.listeners.get(type) ?? [];
    entries.push(listener);
    this.listeners.set(type, entries);
  }

  emit(type: string, payload: unknown): void {
    const message = new MessageEvent<string>(type, {
      data: JSON.stringify(payload),
    });
    for (const listener of this.listeners.get(type) ?? []) {
      listener(message);
    }
  }

  fail(): void {
    this.onerror?.(new Event("error"));
  }

  close(): void {
    this.closed = true;
  }
}

describe("buildAdminEventsUrl", () => {
  it("encodes repeated actorTypes in the SSE url", () => {
    const url = new URL(
      buildAdminEventsUrl(
        {
          audit: { ...DEFAULT_AUDIT_FILTERS, actorTypes: ["player", "system"], success: "false" },
          metrics: { ...DEFAULT_METRICS_FILTERS, roomCode: "room-a", dailyDays: 14, hourlyHours: 24 },
          traffic: { range: "24h", granularity: "15m" },
        },
        "http://testserver",
      ),
    );

    expect(url.searchParams.getAll("auditActorTypes")).toEqual(["player", "system"]);
    expect(url.searchParams.get("auditSuccess")).toBe("false");
    expect(url.searchParams.get("dailyRoomCode")).toBe("room-a");
    expect(url.searchParams.get("dailyDays")).toBe("14");
    expect(url.searchParams.get("trafficRange")).toBe("24h");
    expect(url.searchParams.get("trafficGranularity")).toBe("15m");
  });
});

describe("useAdminSse", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    MockEventSource.instances = [];
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("applies bootstrap and reconnects after errors", async () => {
    const onBootstrap = vi.fn();
    const onOverview = vi.fn();
    const onDailyMetrics = vi.fn();
    const onHourlyMetrics = vi.fn();
    const onLiveTraffic = vi.fn();
    const onTrafficHistory = vi.fn();
    const onAudit = vi.fn();

    const scope = effectScope();
    const api = scope.run(() =>
      useAdminSse({
        getDashboardFilters: () => ({
          audit: DEFAULT_AUDIT_FILTERS,
          metrics: DEFAULT_METRICS_FILTERS,
          traffic: DEFAULT_TRAFFIC_FILTERS,
        }),
        onBootstrap,
        onOverview,
        onDailyMetrics,
        onHourlyMetrics,
        onLiveTraffic,
        onTrafficHistory,
        onAudit,
        createEventSource: (url) => new MockEventSource(url),
        reconnectDelayMs: 2000,
      }),
    );

    expect(api).toBeTruthy();
    api!.connect();
    const bootstrapPromise = api!.waitForBootstrap(500);
    expect(MockEventSource.instances).toHaveLength(1);
    MockEventSource.instances[0].emit("open", {});
    expect(api!.status.value).toBe("live");

    MockEventSource.instances[0].emit("bootstrap", {
      serverTime: 1,
      overview: {
        playerConnections: 0,
        webMapConnections: 0,
        activeRooms: 0,
        rooms: [],
        connectionDetails: [],
        timezone: "UTC",
        dbPathMasked: ".../teamviewer-admin.db",
        broadcastHz: 10,
        hourlyPeak24h: 0,
        observability: { sseSubscribers: 1, lastRetentionCleanup: null, apiErrors: 0, sseErrors: 0, trustProxyHeaders: false },
      },
      dailyMetrics: { timezone: "UTC", roomCode: null, items: [] },
      hourlyMetrics: { timezone: "UTC", roomCode: null, items: [] },
      liveTraffic: {
        sampleWindowSec: 10,
        selectedLayer: "application",
        application: {
          playerIngressBps: 0,
          playerEgressBps: 0,
          webMapIngressBps: 0,
          webMapEgressBps: 0,
          totalIngressBps: 0,
          totalEgressBps: 0,
        },
        wire: {
          playerIngressBps: 0,
          playerEgressBps: 0,
          webMapIngressBps: 0,
          webMapEgressBps: 0,
          totalIngressBps: 0,
          totalEgressBps: 0,
        },
      },
      trafficHistory: {
        timezone: "UTC",
        range: "48h",
        granularity: "1h",
        bucketSeconds: 3600,
        selectedLayer: "application",
        application: {
          totalIngressBytes: 0,
          totalEgressBytes: 0,
          totalBytes: 0,
          items: [],
        },
        wire: {
          totalIngressBytes: 0,
          totalEgressBytes: 0,
          totalBytes: 0,
          items: [],
        },
      },
      audit: { items: [], nextBeforeId: null, limit: 100, availableEventTypes: [] },
    });
    await expect(bootstrapPromise).resolves.toBe(true);
    expect(onBootstrap).toHaveBeenCalledTimes(1);

    MockEventSource.instances[0].fail();
    expect(api!.status.value).toBe("reconnecting");
    await vi.advanceTimersByTimeAsync(2000);
    expect(MockEventSource.instances).toHaveLength(2);

    scope.stop();
  });
});

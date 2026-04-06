import { effectScope } from "vue";

import { DEFAULT_AUDIT_FILTERS } from "@/types";
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
        { ...DEFAULT_AUDIT_FILTERS, actorTypes: ["player", "system"], success: "false" },
        "http://testserver",
      ),
    );

    expect(url.searchParams.getAll("auditActorTypes")).toEqual(["player", "system"]);
    expect(url.searchParams.get("auditSuccess")).toBe("false");
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
    const onAudit = vi.fn();

    const scope = effectScope();
    const api = scope.run(() =>
      useAdminSse({
        getAuditFilters: () => DEFAULT_AUDIT_FILTERS,
        onBootstrap,
        onOverview,
        onDailyMetrics,
        onHourlyMetrics,
        onAudit,
        createEventSource: (url) => new MockEventSource(url),
        reconnectDelayMs: 2000,
      }),
    );

    expect(api).toBeTruthy();
    api!.connect();
    expect(MockEventSource.instances).toHaveLength(1);
    MockEventSource.instances[0].emit("open", {});
    expect(api!.status.value).toBe("live");

    MockEventSource.instances[0].emit("bootstrap", {
      serverTime: 1,
      overview: { playerConnections: 0, webMapConnections: 0, activeRooms: 0, rooms: [], connectionDetails: [], timezone: "UTC", dbPathMasked: ".../teamviewer-admin.db", broadcastHz: 10, hourlyPeak24h: 0 },
      dailyMetrics: { timezone: "UTC", roomCode: null, items: [] },
      hourlyMetrics: { timezone: "UTC", roomCode: null, items: [] },
      audit: { items: [], nextBeforeId: null, limit: 100 },
    });
    expect(onBootstrap).toHaveBeenCalledTimes(1);

    MockEventSource.instances[0].fail();
    expect(api!.status.value).toBe("reconnecting");
    await vi.advanceTimersByTimeAsync(2000);
    expect(MockEventSource.instances).toHaveLength(2);

    scope.stop();
  });
});

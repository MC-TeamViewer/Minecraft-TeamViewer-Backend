import { getCurrentInstance, onBeforeUnmount, ref } from "vue";

import type {
  AuditPayload,
  BootstrapPayload,
  DashboardFilters,
  LiveStatus,
  LiveTrafficPayload,
  MetricsPayload,
  OverviewPayload,
  TrafficHistoryPayload,
} from "@/types";

export interface EventSourceLike {
  addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void;
  close(): void;
  onerror: ((event: Event) => void) | null;
}

export interface UseAdminSseOptions {
  getDashboardFilters: () => DashboardFilters;
  onBootstrap: (payload: BootstrapPayload) => void;
  onOverview: (payload: OverviewPayload) => void;
  onDailyMetrics: (payload: MetricsPayload) => void;
  onHourlyMetrics: (payload: MetricsPayload) => void;
  onLiveTraffic: (payload: LiveTrafficPayload) => void;
  onTrafficHistory: (payload: TrafficHistoryPayload) => void;
  onAudit: (payload: AuditPayload) => void;
  onHeartbeat?: (serverTime: number | undefined) => void;
  onError?: () => void | Promise<void>;
  createEventSource?: (url: string) => EventSourceLike;
  reconnectDelayMs?: number;
}

export function buildAdminEventsUrl(filters: DashboardFilters, origin = window.location.origin): string {
  const url = new URL("/admin/api/events", origin);
  url.searchParams.set("auditLimit", "100");
  if (filters.audit.eventType) {
    url.searchParams.set("auditEventType", filters.audit.eventType);
  }
  for (const actorType of filters.audit.actorTypes) {
    url.searchParams.append("auditActorTypes", actorType);
  }
  if (filters.audit.success) {
    url.searchParams.set("auditSuccess", filters.audit.success);
  }
  url.searchParams.set("dailyDays", String(filters.metrics.dailyDays));
  url.searchParams.set("hourlyHours", String(filters.metrics.hourlyHours));
  if (filters.metrics.dailyStartDate) {
    url.searchParams.set("dailyStartDate", filters.metrics.dailyStartDate);
  }
  if (filters.metrics.hourlyStartAt) {
    url.searchParams.set("hourlyStartAt", filters.metrics.hourlyStartAt);
  }
  if (filters.metrics.roomCode) {
    url.searchParams.set("dailyRoomCode", filters.metrics.roomCode);
    url.searchParams.set("hourlyRoomCode", filters.metrics.roomCode);
  }
  url.searchParams.set("trafficRange", filters.traffic.range);
  url.searchParams.set("trafficGranularity", filters.traffic.granularity);
  if (filters.traffic.startAt) {
    url.searchParams.set("trafficStartAt", filters.traffic.startAt);
  }
  return url.toString();
}

function parsePayload<T>(event: MessageEvent<string>): T | null {
  try {
    return JSON.parse(event.data) as T;
  } catch (_error) {
    return null;
  }
}

export function useAdminSse(options: UseAdminSseOptions) {
  const status = ref<LiveStatus>("connecting");
  const lastHeartbeatAt = ref<number | null>(null);

  const createEventSource =
    options.createEventSource ?? ((url: string) => new EventSource(url) as unknown as EventSourceLike);
  const reconnectDelayMs = options.reconnectDelayMs ?? 2000;

  let source: EventSourceLike | null = null;
  let reconnectTimer: number | null = null;
  let stopped = false;
  let bootstrapWaiters: Array<(received: boolean) => void> = [];

  const clearReconnectTimer = () => {
    if (reconnectTimer !== null) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
  };

  const closeSource = () => {
    if (source) {
      source.close();
      source = null;
    }
  };

  const resolveBootstrapWaiters = (received: boolean) => {
    const waiters = bootstrapWaiters;
    bootstrapWaiters = [];
    for (const waiter of waiters) {
      waiter(received);
    }
  };

  const scheduleReconnect = () => {
    clearReconnectTimer();
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null;
      if (!stopped) {
        open();
      }
    }, reconnectDelayMs);
  };

  const open = () => {
    clearReconnectTimer();
    closeSource();
    status.value = status.value === "reconnecting" ? "reconnecting" : "connecting";

    source = createEventSource(buildAdminEventsUrl(options.getDashboardFilters()));

    source.addEventListener("open", () => {
      status.value = "live";
    });
    source.addEventListener("bootstrap", (event) => {
      const payload = parsePayload<BootstrapPayload>(event);
      if (payload) {
        options.onBootstrap(payload);
        resolveBootstrapWaiters(true);
      }
    });
    source.addEventListener("overview", (event) => {
      const payload = parsePayload<OverviewPayload>(event);
      if (payload) {
        options.onOverview(payload);
      }
    });
    source.addEventListener("daily_metrics", (event) => {
      const payload = parsePayload<MetricsPayload>(event);
      if (payload) {
        options.onDailyMetrics(payload);
      }
    });
    source.addEventListener("hourly_metrics", (event) => {
      const payload = parsePayload<MetricsPayload>(event);
      if (payload) {
        options.onHourlyMetrics(payload);
      }
    });
    source.addEventListener("traffic_live", (event) => {
      const payload = parsePayload<LiveTrafficPayload>(event);
      if (payload) {
        options.onLiveTraffic(payload);
      }
    });
    source.addEventListener("traffic_history", (event) => {
      const payload = parsePayload<TrafficHistoryPayload>(event);
      if (payload) {
        options.onTrafficHistory(payload);
      }
    });
    source.addEventListener("audit", (event) => {
      const payload = parsePayload<AuditPayload>(event);
      if (payload) {
        options.onAudit(payload);
      }
    });
    source.addEventListener("heartbeat", (event) => {
      const payload = parsePayload<{ serverTime?: number }>(event);
      lastHeartbeatAt.value = Date.now();
      options.onHeartbeat?.(payload?.serverTime);
    });
    source.onerror = () => {
      if (stopped) {
        return;
      }
      status.value = "reconnecting";
      resolveBootstrapWaiters(false);
      closeSource();
      void options.onError?.();
      scheduleReconnect();
    };
  };

  const connect = () => {
    stopped = false;
    open();
  };

  const restart = () => {
    stopped = false;
    status.value = "reconnecting";
    open();
  };

  const stop = () => {
    stopped = true;
    clearReconnectTimer();
    resolveBootstrapWaiters(false);
    closeSource();
  };

  const waitForBootstrap = (timeoutMs = 1200) =>
    new Promise<boolean>((resolve) => {
      const timer = window.setTimeout(() => {
        bootstrapWaiters = bootstrapWaiters.filter((item) => item !== waiter);
        resolve(false);
      }, timeoutMs);

      const waiter = (received: boolean) => {
        window.clearTimeout(timer);
        resolve(received);
      };
      bootstrapWaiters.push(waiter);
    });

  if (getCurrentInstance()) {
    onBeforeUnmount(stop);
  }

  return {
    status,
    lastHeartbeatAt,
    connect,
    restart,
    stop,
    waitForBootstrap,
  };
}

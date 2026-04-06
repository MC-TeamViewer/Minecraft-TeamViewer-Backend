import { getCurrentInstance, onBeforeUnmount, ref } from "vue";

import type {
  AuditFilters,
  AuditPayload,
  BootstrapPayload,
  LiveStatus,
  MetricsPayload,
  OverviewPayload,
} from "@/types";

export interface EventSourceLike {
  addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void;
  close(): void;
  onerror: ((event: Event) => void) | null;
}

export interface UseAdminSseOptions {
  getAuditFilters: () => AuditFilters;
  onBootstrap: (payload: BootstrapPayload) => void;
  onOverview: (payload: OverviewPayload) => void;
  onDailyMetrics: (payload: MetricsPayload) => void;
  onHourlyMetrics: (payload: MetricsPayload) => void;
  onAudit: (payload: AuditPayload) => void;
  onHeartbeat?: (serverTime: number | undefined) => void;
  createEventSource?: (url: string) => EventSourceLike;
  reconnectDelayMs?: number;
}

export function buildAdminEventsUrl(filters: AuditFilters, origin = window.location.origin): string {
  const url = new URL("/admin/api/events", origin);
  url.searchParams.set("auditLimit", "100");
  if (filters.eventType) {
    url.searchParams.set("auditEventType", filters.eventType);
  }
  for (const actorType of filters.actorTypes) {
    url.searchParams.append("auditActorTypes", actorType);
  }
  if (filters.success) {
    url.searchParams.set("auditSuccess", filters.success);
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

    source = createEventSource(buildAdminEventsUrl(options.getAuditFilters()));

    source.addEventListener("open", () => {
      status.value = "live";
    });
    source.addEventListener("bootstrap", (event) => {
      const payload = parsePayload<BootstrapPayload>(event);
      if (payload) {
        options.onBootstrap(payload);
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
      closeSource();
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
    closeSource();
  };

  if (getCurrentInstance()) {
    onBeforeUnmount(stop);
  }

  return {
    status,
    lastHeartbeatAt,
    connect,
    restart,
    stop,
  };
}

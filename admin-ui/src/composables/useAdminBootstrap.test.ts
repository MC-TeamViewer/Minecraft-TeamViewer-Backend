import * as api from "@/api";
import { loadAdminBootstrap } from "@/composables/useAdminBootstrap";
import {
  DEFAULT_AUDIT_FILTERS,
  DEFAULT_METRICS_FILTERS,
  DEFAULT_TRAFFIC_FILTERS,
} from "@/types";

vi.mock("@/api", () => ({
  fetchAudit: vi.fn(),
  fetchDailyMetrics: vi.fn(),
  fetchHourlyMetrics: vi.fn(),
  fetchLiveTraffic: vi.fn(),
  fetchLiveProtobufTraffic: vi.fn(),
  fetchOverview: vi.fn(),
  fetchTrafficHistory: vi.fn(),
}));

const filters = {
  audit: DEFAULT_AUDIT_FILTERS,
  metrics: DEFAULT_METRICS_FILTERS,
  traffic: DEFAULT_TRAFFIC_FILTERS,
};

function mockRequiredPayloads() {
  vi.mocked(api.fetchOverview).mockResolvedValue({ marker: "overview" } as never);
  vi.mocked(api.fetchDailyMetrics).mockResolvedValue({ marker: "daily" } as never);
  vi.mocked(api.fetchHourlyMetrics).mockResolvedValue({ marker: "hourly" } as never);
  vi.mocked(api.fetchLiveTraffic).mockResolvedValue({ marker: "live" } as never);
  vi.mocked(api.fetchLiveProtobufTraffic).mockResolvedValue({ marker: "protobuf" } as never);
  vi.mocked(api.fetchTrafficHistory).mockResolvedValue({ marker: "history" } as never);
}

describe("loadAdminBootstrap", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    mockRequiredPayloads();
  });

  it("includes audit data when the optional request succeeds", async () => {
    const audit = {
      items: [],
      playerIdentityMappings: [],
      nextBeforeId: null,
      limit: 100,
      availableEventTypes: ["admin_session_started"],
    };
    vi.mocked(api.fetchAudit).mockResolvedValue(audit);

    const payload = await loadAdminBootstrap(filters);

    expect(payload.audit).toEqual(audit);
  });

  it("keeps core dashboard data when audit loading fails", async () => {
    const warning = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    vi.mocked(api.fetchAudit).mockRejectedValue(new Error("audit unavailable"));

    const payload = await loadAdminBootstrap(filters);

    expect(payload.overview).toEqual({ marker: "overview" });
    expect(payload.audit).toEqual({
      items: [],
      playerIdentityMappings: [],
      nextBeforeId: null,
      limit: 100,
      availableEventTypes: [],
    });
    expect(warning).toHaveBeenCalledOnce();
    warning.mockRestore();
  });
});

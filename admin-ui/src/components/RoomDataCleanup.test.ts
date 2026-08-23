import { flushPromises, mount } from "@vue/test-utils";

import * as api from "@/api";
import RoomDataCleanup from "@/components/RoomDataCleanup.vue";

const { prompt, success, error } = vi.hoisted(() => ({
  prompt: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
}));

vi.mock("element-plus", () => ({
  ElMessage: { success, error },
  ElMessageBox: { prompt },
}));

vi.mock("@/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api")>();
  return {
    ...actual,
    fetchRoomDataMaintenance: vi.fn(),
    purgeRoomData: vi.fn(),
  };
});

function summary(activeConnections = 0) {
  return {
    roomCode: "load-benchmark-v3",
    activeConnections,
    auditEvents: 40,
    dailyActivity: 2,
    hourlyActivity: 4,
    tabHistoryEntries: 1000,
    tabHistoryDeltas: 1000,
    tabHistoryRevisions: 2,
    tabHistoryHeads: 1,
    lastSeenRecords: 1000,
    identityMappings: 3,
    totalRemovable: 3052,
  };
}

describe("RoomDataCleanup", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    vi.mocked(api.fetchRoomDataMaintenance).mockResolvedValue({
      items: [summary()],
      trafficDataRetained: true,
      warning: "traffic retained",
    });
  });

  it("renders the cleanup preview and disables active rooms", async () => {
    vi.mocked(api.fetchRoomDataMaintenance).mockResolvedValue({
      items: [summary(2)],
      trafficDataRetained: true,
      warning: "traffic retained",
    });
    const wrapper = mount(RoomDataCleanup);
    await flushPromises();

    expect(wrapper.text()).toContain("历史流量按全局聚合");
    expect(wrapper.text()).toContain("Tab 条目");
    expect(wrapper.text()).toContain("3052");
    expect(wrapper.text()).toContain("2 个在线连接");
    const deleteButton = wrapper.findAll("button").find((button) => button.text().includes("永久清理"));
    expect(deleteButton?.attributes("disabled")).toBeDefined();
  });

  it("requires the exact room confirmation and emits after a successful purge", async () => {
    prompt.mockResolvedValue({ value: "load-benchmark-v3" });
    vi.mocked(api.purgeRoomData).mockResolvedValue({
      ok: true,
      roomCode: "load-benchmark-v3",
      deleted: { auditEvents: 40, totalDeleted: 3052 },
      trafficDataRetained: true,
    });
    const wrapper = mount(RoomDataCleanup);
    await flushPromises();

    const deleteButton = wrapper.findAll("button").find((button) => button.text().includes("永久清理"));
    expect(deleteButton).toBeTruthy();
    await deleteButton!.trigger("click");
    await flushPromises();

    const options = prompt.mock.calls[0][2] as { inputValidator: (value: string) => boolean | string };
    expect(options.inputValidator("wrong-room")).toBe("输入必须与房间名完全一致");
    expect(options.inputValidator(" load-benchmark-v3 ")).toBe(true);
    expect(api.purgeRoomData).toHaveBeenCalledWith("load-benchmark-v3", "load-benchmark-v3");
    expect(wrapper.emitted("purged")?.[0]).toEqual(["load-benchmark-v3"]);
    expect(success).toHaveBeenCalled();
  });
});

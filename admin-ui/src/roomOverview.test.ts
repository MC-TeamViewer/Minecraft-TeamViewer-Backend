import { summarizeRoomIds } from "@/roomOverview";

describe("summarizeRoomIds", () => {
  it("returns count text for non-empty IDs", () => {
    expect(summarizeRoomIds(["player-1", "player-2", "player-3"])).toBe("3 个");
  });

  it("returns a dash for empty or missing IDs", () => {
    expect(summarizeRoomIds([])).toBe("-");
    expect(summarizeRoomIds(undefined)).toBe("-");
  });
});

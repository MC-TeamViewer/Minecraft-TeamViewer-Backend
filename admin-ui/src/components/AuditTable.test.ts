import { mount } from "@vue/test-utils";
import { nextTick } from "vue";

import AuditTable from "@/components/AuditTable.vue";
import type { AuditPayload } from "@/types";

function countOccurrences(haystack: string, needle: string): number {
  return haystack.split(needle).length - 1;
}

function buildAuditPayload(): AuditPayload {
  return {
    items: [
      {
        id: 2,
        occurredAt: Date.UTC(2026, 3, 9, 10, 0, 0),
        localDate: "2026-04-09",
        localHour: "2026-04-09T18:00:00",
        eventType: "player_handshake_success",
        actorType: "player",
        actorId: "player-1",
        resolvedActorName: "MappedNameAlpha",
        roomCode: "room-a",
        success: true,
        remoteAddr: "127.0.0.1",
        detail: { clientProtocol: "0.6.1" },
      },
      {
        id: 1,
        occurredAt: Date.UTC(2026, 3, 9, 9, 0, 0),
        localDate: "2026-04-09",
        localHour: "2026-04-09T17:00:00",
        eventType: "web_map_handshake_success",
        actorType: "web_map",
        actorId: "web-map-1",
        resolvedActorName: null,
        roomCode: "room-a",
        success: true,
        remoteAddr: "127.0.0.2",
        detail: { clientProtocol: "0.6.1" },
      },
    ],
    playerIdentityMappings: [
      {
        playerId: "player-1",
        username: "MappedNameAlpha",
        updatedAt: Date.UTC(2026, 3, 9, 10, 1, 0),
      },
    ],
    nextBeforeId: 1,
    limit: 100,
    availableEventTypes: ["player_handshake_success", "web_map_handshake_success"],
  };
}

describe("AuditTable", () => {
  it("renders identity mappings and switches player actor display to username", async () => {
    const wrapper = mount(AuditTable, {
      props: {
        audit: buildAuditPayload(),
      },
    });

    await nextTick();
    await nextTick();

    expect(wrapper.text()).toContain("玩家身份映射");
    expect(wrapper.text()).toContain("MappedNameAlpha");
    expect(wrapper.text()).toContain("显示原始 Actor ID");
    expect(wrapper.text()).toContain("显示 username");
    expect(countOccurrences(wrapper.text(), "MappedNameAlpha")).toBe(1);

    const buttons = wrapper.findAll("button");
    const usernameButton = buttons.find((button) => button.text().includes("显示 username"));
    expect(usernameButton).toBeTruthy();

    await usernameButton!.trigger("click");
    await nextTick();

    expect(countOccurrences(wrapper.text(), "MappedNameAlpha")).toBeGreaterThan(1);
    expect(wrapper.text()).toContain("web-map-1");
  });
});

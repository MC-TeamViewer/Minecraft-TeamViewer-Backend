import { connectionColumns } from "@/connectionColumns";

describe("connectionColumns", () => {
  it("marks long text columns as overflow-safe", () => {
    const overflowColumns = new Set(
      connectionColumns.filter((column) => column.showOverflowTooltip).map((column) => column.prop),
    );

    expect(overflowColumns).toEqual(
      new Set(["displayName", "roomCode", "protocolVersion", "programVersion", "remoteAddr", "actorId"]),
    );
  });

  it("shows the active compatibility rule count", () => {
    expect(connectionColumns).toContainEqual(
      expect.objectContaining({ prop: "compatibilityRuleCount", label: "兼容规则" }),
    );
  });
});

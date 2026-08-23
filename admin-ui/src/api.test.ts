import { ApiError, fetchAudit, purgeRoomData } from "@/api";
import { DEFAULT_AUDIT_FILTERS } from "@/types";

describe("admin room maintenance API", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("encodes the audit room and sends exact purge confirmation", async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(new Response(JSON.stringify({ items: [] }), { status: 200 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({ ok: true }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    await fetchAudit({ ...DEFAULT_AUDIT_FILTERS, roomCode: "load / 压测" });
    await purgeRoomData("load / 压测", "load / 压测");

    const auditUrl = new URL(fetchMock.mock.calls[0][0] as string);
    expect(auditUrl.searchParams.get("roomCode")).toBe("load / 压测");
    expect(fetchMock.mock.calls[1][1]).toMatchObject({
      method: "DELETE",
      body: JSON.stringify({ roomCode: "load / 压测", confirmation: "load / 压测" }),
    });
  });

  it("preserves the backend error detail", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(
      JSON.stringify({ detail: "room_has_active_connections" }),
      { status: 409 },
    )));

    await expect(purgeRoomData("active", "active")).rejects.toMatchObject({
      status: 409,
      detail: "room_has_active_connections",
    } satisfies Partial<ApiError>);
  });
});

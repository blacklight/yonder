import { describe, expect, it, vi, beforeEach } from "vitest";
import {
  accountStatuses,
  blockAccount,
  follow,
  getAccount,
  getRelationships,
  muteAccount,
  setNote,
  unblockAccount,
  unfollow,
  unmuteAccount,
  verifyCredentials,
} from "./accounts";

function mockFetch(status: number, body: unknown) {
  return vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(() =>
      Promise.resolve(new Response(JSON.stringify(body), { status })),
    );
}

describe("accounts api", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("verifyCredentials GETs the credential endpoint", async () => {
    const spy = mockFetch(200, { id: "1" });
    await verifyCredentials();
    expect(spy.mock.calls[0][0]).toBe("/api/v1/accounts/verify_credentials");
    expect(spy.mock.calls[0][1]!.method).toBe("GET");
  });

  it("getAccount encodes the id", async () => {
    const spy = mockFetch(200, { id: "1" });
    await getAccount("42");
    expect(spy.mock.calls[0][0]).toBe("/api/v1/accounts/42");
  });

  it("getRelationships repeats id[] params", async () => {
    const spy = mockFetch(200, []);
    await getRelationships(["1", "2"]);
    expect(spy.mock.calls[0][0]).toBe(
      "/api/v1/accounts/relationships?id%5B%5D=1&id%5B%5D=2",
    );
  });

  it.each([
    [follow, "follow"],
    [unfollow, "unfollow"],
    [muteAccount, "mute"],
    [unmuteAccount, "unmute"],
    [blockAccount, "block"],
    [unblockAccount, "unblock"],
  ] as const)("POSTs /accounts/{id}/%s", async (fn, action) => {
    const spy = mockFetch(200, {});
    await (fn as (id: string) => Promise<unknown>)("9");
    expect(spy.mock.calls[0][0]).toBe(`/api/v1/accounts/9/${action}`);
    expect(spy.mock.calls[0][1]!.method).toBe("POST");
  });

  it("setNote POSTs {comment} and returns the relationship", async () => {
    const spy = mockFetch(200, { id: "9", note: "met at fosdem" });
    const rel = await setNote("9", "met at fosdem");
    expect(spy.mock.calls[0][0]).toBe("/api/v1/accounts/9/note");
    expect(JSON.parse(spy.mock.calls[0][1]!.body as string)).toEqual({
      comment: "met at fosdem",
    });
    expect(rel.note).toBe("met at fosdem");
  });

  it("accountStatuses paginates with Link", async () => {
    vi.restoreAllMocks();
    const spy = vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(
        new Response(JSON.stringify([{ id: "1" }]), {
          status: 200,
          headers: { link: '</n>; rel="next"' },
        }),
      ),
    );
    const page = await accountStatuses("9", {
      only_media: true,
      pinned: true,
      limit: 10,
    });
    expect(spy.mock.calls[0][0]).toBe(
      "/api/v1/accounts/9/statuses?only_media=true&pinned=true&limit=10",
    );
    expect(page.next).toBe("/n");
  });
});

import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import FollowListPage from "./FollowListPage.vue";
import { ApiError } from "@/api/errors";
import type { Account } from "@/api/entities";

vi.mock("@/api/accounts", async (orig) => {
  const m = await orig<typeof import("@/api/accounts")>();
  return { ...m, lookup: vi.fn(), followers: vi.fn(), following: vi.fn() };
});
import { followers, following, lookup } from "@/api/accounts";

function account(partial: Partial<Account>): Account {
  return {
    id: "1",
    username: "alice",
    acct: "alice",
    display_name: "Alice",
    note: "",
    url: "https://example.org/@alice",
    avatar: "",
    header: "",
    locked: false,
    bot: false,
    created_at: new Date().toISOString(),
    followers_count: 0,
    following_count: 0,
    statuses_count: 0,
    fields: [],
    emojis: [],
    moved: null,
    ...partial,
  };
}

async function open(path: string, direction: "followers" | "following") {
  const { wrapper, router } = mountWith(FollowListPage, {
    props: { direction },
  });
  await router.push(path);
  await flushPromises();
  return { wrapper, router };
}

describe("FollowListPage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.mocked(lookup).mockReset();
    vi.mocked(followers).mockReset();
    vi.mocked(following).mockReset();
    document.title = "";
  });

  it("lists followers linking each account to its profile", async () => {
    vi.mocked(lookup).mockResolvedValue(account({ acct: "alice" }));
    vi.mocked(followers).mockResolvedValue({
      items: [
        account({ id: "2", acct: "bob", username: "bob" }),
        account({
          id: "3",
          acct: "dave@remote.example",
          username: "dave",
        }),
      ],
    });
    const { wrapper } = await open("/@alice/followers", "followers");
    const rows = wrapper.findAll(".network__row");
    expect(rows).toHaveLength(2);
    expect(rows[0].attributes("href")).toBe("/@bob");
    expect(rows[1].attributes("href")).toBe("/@dave@remote.example");
    expect(document.title).toBe("Followers — Alice (@alice) — Yonder");
    expect(vi.mocked(followers)).toHaveBeenCalledWith("1", {
      max_id: undefined,
    });
  });

  it("uses the following endpoint for the following direction", async () => {
    vi.mocked(lookup).mockResolvedValue(account({ acct: "alice" }));
    vi.mocked(following).mockResolvedValue({
      items: [account({ id: "4", acct: "erin", username: "erin" })],
    });
    const { wrapper } = await open("/@alice/following", "following");
    expect(vi.mocked(following)).toHaveBeenCalled();
    expect(vi.mocked(followers)).not.toHaveBeenCalled();
    expect(wrapper.findAll(".network__row")).toHaveLength(1);
    expect(document.title).toBe("Following — Alice (@alice) — Yonder");
  });

  it("appends the next page via load more", async () => {
    vi.mocked(lookup).mockResolvedValue(account({ acct: "alice" }));
    vi.mocked(followers)
      // The mount fires `load()` once before the route push resolves.
      .mockResolvedValueOnce({ items: [] })
      .mockResolvedValueOnce({
        items: [account({ id: "2", acct: "bob" })],
        next: "http://localhost/api/v1/accounts/1/followers?max_id=9",
      })
      .mockResolvedValueOnce({
        items: [account({ id: "3", acct: "erin" })],
      });
    const { wrapper } = await open("/@alice/followers", "followers");
    await wrapper.find(".network__more button").trigger("click");
    await flushPromises();
    expect(vi.mocked(followers)).toHaveBeenLastCalledWith("1", {
      max_id: "9",
    });
    expect(wrapper.findAll(".network__row")).toHaveLength(2);
  });

  it("navigates to the sibling tab", async () => {
    vi.mocked(lookup).mockResolvedValue(account({ acct: "alice" }));
    vi.mocked(followers).mockResolvedValue({ items: [] });
    vi.mocked(following).mockResolvedValue({ items: [] });
    const { wrapper, router } = await open("/@alice/followers", "followers");
    const buttons = wrapper.findAll(".tabs button");
    await buttons[1].trigger("click");
    await flushPromises();
    expect(router.currentRoute.value.path).toBe("/@alice/following");
  });

  it("shows not-found for a missing account", async () => {
    vi.mocked(lookup).mockRejectedValue(new ApiError("Record not found", 404));
    const { wrapper } = await open("/@ghost/followers", "followers");
    expect(wrapper.text()).toContain("Account not found.");
  });
});

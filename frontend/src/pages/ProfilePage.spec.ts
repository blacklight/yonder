import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import ProfilePage from "./ProfilePage.vue";
import { ApiError } from "@/api/errors";
import type { Account } from "@/api/entities";

vi.mock("@/api/accounts", async (orig) => {
  const m = await orig<typeof import("@/api/accounts")>();
  return { ...m, lookup: vi.fn(), accountStatuses: vi.fn() };
});
import { lookup, accountStatuses } from "@/api/accounts";

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

async function openProfile(path: string) {
  const { wrapper, router } = mountWith(ProfilePage);
  await router.push(path);
  await flushPromises();
  return wrapper;
}

describe("ProfilePage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.mocked(lookup).mockReset();
    vi.mocked(accountStatuses).mockReset();
    vi.mocked(accountStatuses).mockResolvedValue({ items: [] });
    document.title = "";
  });

  it("titles the page with the display name and handle", async () => {
    vi.mocked(lookup).mockResolvedValue(
      account({ acct: "alice@example.org", display_name: "Alice" }),
    );
    await openProfile("/@alice@example.org");
    expect(document.title).toBe("Alice (@alice@example.org) — Yonder");
  });

  it("falls back to the username when display_name is blank", async () => {
    vi.mocked(lookup).mockResolvedValue(
      account({ username: "alice", display_name: "" }),
    );
    await openProfile("/@alice");
    expect(document.title).toBe("alice (@alice) — Yonder");
  });

  it("titles a missing profile with the generic profile label", async () => {
    vi.mocked(lookup).mockRejectedValue(new ApiError("Record not found", 404));
    await openProfile("/@ghost");
    expect(document.title).toBe("Profile — Yonder");
  });
});

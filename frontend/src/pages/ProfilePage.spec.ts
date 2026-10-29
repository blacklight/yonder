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

  it("renders a raw-URL field with the scheme stripped for display", async () => {
    vi.mocked(lookup).mockResolvedValue(
      account({
        fields: [{ name: "Site", value: "https://ar.al/", verified_at: null }],
      }),
    );
    const wrapper = await openProfile("/@alice");
    const link = wrapper.find(".profile__fields dd a");
    expect(link.exists()).toBe(true);
    expect(link.attributes("href")).toBe("https://ar.al/");
    expect(link.text()).toBe("ar.al");
  });

  it("renders an anchor-valued field as a link, keeping its label", async () => {
    vi.mocked(lookup).mockResolvedValue(
      account({
        fields: [
          {
            name: "Site",
            value: '<a href="https://small-tech.org" rel="me">Small Tech</a>',
            verified_at: null,
          },
        ],
      }),
    );
    const wrapper = await openProfile("/@alice");
    const link = wrapper.find(".profile__fields dd a");
    expect(link.attributes("href")).toBe("https://small-tech.org/");
    expect(link.text()).toBe("Small Tech");
  });

  it("strips the scheme when an anchor's own text is the URL", async () => {
    vi.mocked(lookup).mockResolvedValue(
      account({
        fields: [
          {
            name: "Site",
            value:
              '<a href="https://kitten.small-web.org">https://kitten.small-web.org</a>',
            verified_at: null,
          },
        ],
      }),
    );
    const wrapper = await openProfile("/@alice");
    const link = wrapper.find(".profile__fields dd a");
    expect(link.attributes("href")).toBe("https://kitten.small-web.org/");
    expect(link.text()).toBe("kitten.small-web.org");
  });

  it("renders a fediverse handle field value as a profile link", async () => {
    vi.mocked(lookup).mockResolvedValue(
      account({
        fields: [
          {
            name: "Main account",
            value: "@fabio@manganiello.eu",
            verified_at: null,
          },
        ],
      }),
    );
    const wrapper = await openProfile("/@alice");
    const link = wrapper.find(".profile__fields dd a");
    expect(link.attributes("href")).toBe("https://manganiello.eu/@fabio");
    expect(link.text()).toBe("@fabio@manganiello.eu");
    expect(link.attributes("target")).toBe("_blank");
  });

  it("renders an email field value as a mailto link", async () => {
    vi.mocked(lookup).mockResolvedValue(
      account({
        fields: [
          { name: "Mail", value: "alice@example.org", verified_at: null },
        ],
      }),
    );
    const wrapper = await openProfile("/@alice");
    const link = wrapper.find(".profile__fields dd a");
    expect(link.attributes("href")).toBe("mailto:alice@example.org");
    expect(link.text()).toBe("alice@example.org");
    expect(link.attributes("target")).toBeUndefined();
  });

  it("shows a verified marker on fields with verified_at", async () => {
    vi.mocked(lookup).mockResolvedValue(
      account({
        fields: [
          {
            name: "Site",
            value: "https://ar.al",
            verified_at: "2026-01-01T00:00:00Z",
          },
          { name: "Blog", value: "https://blog.example", verified_at: null },
        ],
      }),
    );
    const wrapper = await openProfile("/@alice");
    const cells = wrapper.findAll(".profile__fields dd");
    expect(cells).toHaveLength(2);
    expect(cells[0].classes()).toContain("profile__field--verified");
    expect(cells[0].find(".verified").exists()).toBe(true);
    expect(cells[1].classes()).not.toContain("profile__field--verified");
    expect(cells[1].find(".verified").exists()).toBe(false);
  });
});

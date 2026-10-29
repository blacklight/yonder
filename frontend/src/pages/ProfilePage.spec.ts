import { describe, expect, it, vi, beforeEach } from "vitest";
import { DOMWrapper, flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import ProfilePage from "./ProfilePage.vue";
import { ApiError } from "@/api/errors";
import type { Account } from "@/api/entities";

vi.mock("@/api/accounts", async (orig) => {
  const m = await orig<typeof import("@/api/accounts")>();
  return {
    ...m,
    lookup: vi.fn(),
    accountStatuses: vi.fn(),
    getRelationships: vi.fn(),
  };
});
import { lookup, accountStatuses, getRelationships } from "@/api/accounts";

// The menu list teleports to <body> — query it at document level.
const menuItems = () =>
  [...document.querySelectorAll<HTMLElement>("[role='menuitem']")].map(
    (el) => new DOMWrapper(el),
  );

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
    vi.mocked(getRelationships).mockReset();
    vi.mocked(getRelationships).mockResolvedValue([]);
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

  it("offers 'Open on <domain>' for remote accounts, opening their url", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    const openSpy = vi.spyOn(window, "open").mockImplementation(() => null);
    vi.mocked(lookup).mockResolvedValue(
      account({
        acct: "alice@example.org",
        url: "https://example.org/@alice",
      }),
    );
    const wrapper = await openProfile("/@alice@example.org");
    await wrapper.get(".menu__trigger").trigger("click");
    const item = menuItems().find((i) =>
      i.text().includes("Open on example.org"),
    );
    expect(item).toBeDefined();
    await item!.trigger("click");
    await flushPromises();
    expect(openSpy).toHaveBeenCalledWith(
      "https://example.org/@alice",
      "_blank",
      "noopener,noreferrer",
    );
  });

  it("has no 'Open on' entry for local accounts", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(account({ acct: "alice" }));
    const wrapper = await openProfile("/@alice");
    await wrapper.get(".menu__trigger").trigger("click");
    expect(menuItems().some((i) => i.text().includes("Open on"))).toBe(false);
  });

  it("marks destructive menu items as danger", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(account({ acct: "alice@example.org" }));
    const wrapper = await openProfile("/@alice@example.org");
    await wrapper.get(".menu__trigger").trigger("click");
    const items = menuItems();
    const byText = (s: string) => items.find((i) => i.text().trim() === s);
    for (const label of ["Mute", "Block", "Block domain", "Report"]) {
      expect(byText(label)?.classes()).toContain("menu__item--danger");
    }
    for (const label of ["Mention", "Note", "Open on example.org"]) {
      expect(byText(label)?.classes()).not.toContain("menu__item--danger");
    }
  });

  it("hides 'Make moderator' on remote profiles", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    auth.role = "admin";
    vi.mocked(lookup).mockResolvedValue(account({ acct: "alice@example.org" }));
    const wrapper = await openProfile("/@alice@example.org");
    await wrapper.get(".menu__trigger").trigger("click");
    const items = menuItems();
    expect(items.some((i) => i.text().includes("Make moderator"))).toBe(false);
    expect(items.some((i) => i.text().includes("Delete account"))).toBe(true);
  });

  it("shows 'Make moderator' on local profiles for admins", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    auth.role = "admin";
    vi.mocked(lookup).mockResolvedValue(account({ acct: "alice" }));
    const wrapper = await openProfile("/@alice");
    await wrapper.get(".menu__trigger").trigger("click");
    expect(menuItems().some((i) => i.text().includes("Make moderator"))).toBe(
      true,
    );
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

  it("renders the private note on the profile when one is set", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice@example.org" }),
    );
    vi.mocked(getRelationships).mockResolvedValue([
      {
        id: "1",
        following: false,
        followed_by: false,
        blocking: false,
        muting: false,
        note: "Met at FOSDEM",
      } as never,
    ]);
    const wrapper = await openProfile("/@alice@example.org");
    const note = wrapper.find(".profile__note");
    expect(note.exists()).toBe(true);
    expect(note.find(".profile__note-title").text()).toContain("Private note");
    expect(note.find(".profile__note-text").text()).toBe("Met at FOSDEM");
  });

  it("hides the private note block when no note is set", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice@example.org" }),
    );
    vi.mocked(getRelationships).mockResolvedValue([
      {
        id: "1",
        following: false,
        followed_by: false,
        blocking: false,
        muting: false,
        note: "",
      } as never,
    ]);
    const wrapper = await openProfile("/@alice@example.org");
    expect(wrapper.find(".profile__note").exists()).toBe(false);
  });

  it("opens the note editor prefilled, with modal buttons in the footer", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice@example.org" }),
    );
    vi.mocked(getRelationships).mockResolvedValue([
      {
        id: "1",
        following: false,
        followed_by: false,
        blocking: false,
        muting: false,
        note: "Met at FOSDEM",
      } as never,
    ]);
    const wrapper = await openProfile("/@alice@example.org");
    await wrapper.find(".profile__note-head button").trigger("click");
    const input = document.querySelector<HTMLTextAreaElement>(
      ".modal__panel .profile__note-input",
    );
    expect(input?.value).toBe("Met at FOSDEM");
    const buttons = [...document.querySelectorAll(".modal__footer .btn")].map(
      (b) => b.textContent?.trim(),
    );
    expect(buttons).toEqual(["Cancel", "Save"]);
  });
});

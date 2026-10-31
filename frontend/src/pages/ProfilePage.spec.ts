import { describe, expect, it, vi, beforeEach } from "vitest";
import { DOMWrapper, flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import ProfilePage from "./ProfilePage.vue";
import { ApiError } from "@/api/errors";
import type { Account, Status } from "@/api/entities";

vi.mock("@/api/accounts", async (orig) => {
  const m = await orig<typeof import("@/api/accounts")>();
  return {
    ...m,
    lookup: vi.fn(),
    getAccount: vi.fn(),
    accountStatuses: vi.fn(),
    getRelationships: vi.fn(),
    getAccountLists: vi.fn(),
  };
});
vi.mock("@/api/lists", async (orig) => {
  const m = await orig<typeof import("@/api/lists")>();
  return {
    ...m,
    listLists: vi.fn(),
    createList: vi.fn(),
    addListMembers: vi.fn(),
    removeListMembers: vi.fn(),
  };
});
vi.mock("@/api/admin", async (orig) => {
  const m = await orig<typeof import("@/api/admin")>();
  return { ...m, adminAccountAction: vi.fn(), setRole: vi.fn() };
});
vi.mock("@/api/adminUsers", async (orig) => {
  const m = await orig<typeof import("@/api/adminUsers")>();
  return { ...m, reverseAdminAction: vi.fn() };
});
import {
  lookup,
  getAccount,
  accountStatuses,
  getRelationships,
  getAccountLists,
} from "@/api/accounts";
import {
  listLists,
  createList,
  addListMembers,
  removeListMembers,
} from "@/api/lists";
import { adminAccountAction } from "@/api/admin";
import { reverseAdminAction } from "@/api/adminUsers";

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

function status(over: Partial<Status> = {}): Status {
  return {
    id: "10",
    uri: "https://example.org/s/10",
    url: "https://example.org/s/10",
    account: account({ id: "1" }) as Status["account"],
    content: "<p>regular</p>",
    created_at: new Date().toISOString(),
    edited_at: null,
    visibility: "public",
    sensitive: false,
    spoiler_text: "",
    media_attachments: [],
    mentions: [],
    tags: [],
    emojis: [],
    card: null,
    poll: null,
    application: null,
    language: "en",
    in_reply_to_id: null,
    in_reply_to_account_id: null,
    reblog: null,
    replies_count: 0,
    reblogs_count: 0,
    favourites_count: 0,
    quotes_count: 0,
    quote_id: null,
    quote: null,
    reblogged: false,
    favourited: false,
    bookmarked: false,
    pinned: false,
    muted: false,
    pleroma: { emoji_reactions: [] },
    ...over,
  } as Status;
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
    vi.mocked(getAccountLists).mockReset();
    vi.mocked(getAccountLists).mockResolvedValue([]);
    vi.mocked(listLists).mockReset();
    vi.mocked(listLists).mockResolvedValue([]);
    vi.mocked(addListMembers).mockReset();
    vi.mocked(removeListMembers).mockReset();
    vi.mocked(createList).mockReset();
    vi.mocked(getAccount).mockReset();
    vi.mocked(adminAccountAction).mockReset();
    vi.mocked(reverseAdminAction).mockReset();
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

  it("renders the pinned set first, marker only on the pinned card", async () => {
    vi.mocked(lookup).mockResolvedValue(account({}));
    vi.mocked(accountStatuses).mockImplementation((_id, q) =>
      Promise.resolve({
        items: q?.pinned
          ? [
              status({
                id: "9",
                pinned: true,
                content: "<p>pinned post</p>",
              }),
            ]
          : [status()],
      }),
    );
    const wrapper = await openProfile("/@alice");
    const cards = wrapper.findAll(".status-card");
    expect(cards).toHaveLength(2);
    // The pinned card leads and carries the marker; the same flag in
    // the chronological list gets none (Mastodon's profile rule).
    expect(cards[0].text()).toContain("pinned post");
    expect(cards[0].find(".status-card__pin").exists()).toBe(true);
    expect(cards[1].text()).toContain("regular");
    expect(cards[1].find(".status-card__pin").exists()).toBe(false);
    expect(
      vi.mocked(accountStatuses).mock.calls.some(([, q]) => q?.pinned === true),
    ).toBe(true);
  });

  it("opens a list picker with membership checkboxes", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice@example.org" }),
    );
    vi.mocked(listLists).mockResolvedValue([
      { id: "10", title: "Friends", replies_policy: "followed" },
      { id: "11", title: "Work", replies_policy: "followed" },
    ]);
    vi.mocked(getAccountLists).mockResolvedValue([
      { id: "10", title: "Friends", replies_policy: "followed" },
    ]);
    const wrapper = await openProfile("/@alice@example.org");
    await wrapper.get(".menu__trigger").trigger("click");
    await menuItems()
      .find((i) => i.text().includes("Add to list"))!
      .trigger("click");
    await flushPromises();

    const panel = [...document.querySelectorAll(".modal__panel")].at(-1)!;
    const rows = panel.querySelectorAll(".profile__list-row");
    expect(rows).toHaveLength(2);
    const boxes = [
      ...panel.querySelectorAll<HTMLInputElement>(
        ".profile__list-row input[type='checkbox']",
      ),
    ];
    expect(boxes[0].checked).toBe(true);
    expect(boxes[1].checked).toBe(false);
    expect(vi.mocked(getAccountLists)).toHaveBeenCalledWith("1");
  });

  it("adds and removes the account via the list picker", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice@example.org" }),
    );
    vi.mocked(listLists).mockResolvedValue([
      { id: "10", title: "Friends", replies_policy: "followed" },
    ]);
    vi.mocked(getAccountLists).mockResolvedValue([]);
    vi.mocked(addListMembers).mockResolvedValue({});
    vi.mocked(removeListMembers).mockResolvedValue({});
    const wrapper = await openProfile("/@alice@example.org");
    await wrapper.get(".menu__trigger").trigger("click");
    await menuItems()
      .find((i) => i.text().includes("Add to list"))!
      .trigger("click");
    await flushPromises();

    const panel = [...document.querySelectorAll(".modal__panel")].at(-1)!;
    const box = panel.querySelector<HTMLInputElement>(
      ".profile__list-row input[type='checkbox']",
    )!;
    box.click();
    await flushPromises();
    expect(vi.mocked(addListMembers)).toHaveBeenCalledWith("10", ["1"]);
    expect(box.checked).toBe(true);

    box.click();
    await flushPromises();
    expect(vi.mocked(removeListMembers)).toHaveBeenCalledWith("10", ["1"]);
    expect(box.checked).toBe(false);
  });

  it("points to list management when the viewer has no lists", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice@example.org" }),
    );
    vi.mocked(listLists).mockResolvedValue([]);
    vi.mocked(getAccountLists).mockResolvedValue([]);
    const wrapper = await openProfile("/@alice@example.org");
    await wrapper.get(".menu__trigger").trigger("click");
    await menuItems()
      .find((i) => i.text().includes("Add to list"))!
      .trigger("click");
    await flushPromises();

    const panel = [...document.querySelectorAll(".modal__panel")].at(-1)!;
    const empty = panel.querySelector(".profile__lists-empty");
    expect(empty?.textContent).toContain("don't have any lists");
    const link = empty?.querySelector("a");
    expect(link?.getAttribute("href")).toBe("/lists");
  });

  it("creates a list from the picker and adds the account to it", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice@example.org" }),
    );
    vi.mocked(listLists).mockResolvedValue([]);
    vi.mocked(getAccountLists).mockResolvedValue([]);
    vi.mocked(createList).mockResolvedValue({
      id: "20",
      title: "Close friends",
      replies_policy: "followed",
    });
    vi.mocked(addListMembers).mockResolvedValue({});
    const wrapper = await openProfile("/@alice@example.org");
    await wrapper.get(".menu__trigger").trigger("click");
    await menuItems()
      .find((i) => i.text().includes("Add to list"))!
      .trigger("click");
    await flushPromises();

    const panel = [...document.querySelectorAll(".modal__panel")].at(-1)!;
    const input = panel.querySelector<HTMLInputElement>(
      ".profile__lists-new .field__input",
    )!;
    input.value = "Close friends";
    input.dispatchEvent(new Event("input"));
    await flushPromises();
    const submit = [
      ...panel.querySelectorAll<HTMLButtonElement>("button"),
    ].find((b) => b.textContent?.trim() === "Create list")!;
    submit.click();
    await flushPromises();

    expect(vi.mocked(createList)).toHaveBeenCalledWith({
      title: "Close friends",
    });
    expect(vi.mocked(addListMembers)).toHaveBeenCalledWith("20", ["1"]);
    const box = panel.querySelector<HTMLInputElement>(
      ".profile__list-row input[type='checkbox']",
    )!;
    expect(box.checked).toBe(true);
    expect(panel.textContent).toContain("Close friends");
  });

  it("badges a local staff account as Admin/Mod", async () => {
    vi.mocked(lookup).mockResolvedValue(
      account({ yonder: { role: "admin" } } as Partial<Account>),
    );
    const wrapper = await openProfile("/@alice");
    const badges = wrapper.findAll(".profile__badge");
    expect(badges.some((b) => b.text() === "Admin")).toBe(true);

    vi.mocked(lookup).mockResolvedValue(
      account({ yonder: { role: "moderator" } } as Partial<Account>),
    );
    const wrapper2 = await openProfile("/@alice");
    expect(
      wrapper2.findAll(".profile__badge").some((b) => b.text() === "Mod"),
    ).toBe(true);
  });

  it("badges limited and suspended accounts, grayscale on suspended", async () => {
    vi.mocked(lookup).mockResolvedValue(account({ limited: true }));
    const wrapper = await openProfile("/@alice");
    expect(
      wrapper.findAll(".profile__badge").some((b) => b.text() === "Limited"),
    ).toBe(true);
    expect(wrapper.find(".profile__avatar--grayscale").exists()).toBe(false);

    vi.mocked(lookup).mockResolvedValue(account({ suspended: true }));
    const wrapper2 = await openProfile("/@alice");
    expect(
      wrapper2.findAll(".profile__badge").some((b) => b.text() === "Suspended"),
    ).toBe(true);
    expect(wrapper2.find(".profile__avatar--grayscale").exists()).toBe(true);
  });

  it("badges blocked and muted accounts, grayscale on blocked", async () => {
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
        blocking: true,
        muting: true,
        note: "",
      } as never,
    ]);
    const wrapper = await openProfile("/@alice@example.org");
    const badges = wrapper.findAll(".profile__badge");
    expect(badges.some((b) => b.text() === "Blocked")).toBe(true);
    expect(badges.some((b) => b.text() === "Muted")).toBe(true);
    expect(wrapper.find(".profile__avatar--grayscale").exists()).toBe(true);
  });

  it("offers 'Unlimit account' on a limited account and reverses", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    auth.role = "moderator";
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice", limited: true }),
    );
    vi.mocked(getAccount).mockResolvedValue(
      account({ id: "1", acct: "alice", limited: false }),
    );
    vi.mocked(reverseAdminAction).mockResolvedValue({} as never);
    const wrapper = await openProfile("/@alice");
    await wrapper.get(".menu__trigger").trigger("click");
    const items = menuItems();
    const unlimit = items.find((i) => i.text() === "Unlimit account");
    expect(unlimit).toBeTruthy();
    expect(items.some((i) => i.text() === "Limit account")).toBe(false);
    await unlimit!.trigger("click");
    // Destructive actions confirm first.
    const modal = document.querySelector(".modal__body .btn--danger");
    expect(modal).toBeTruthy();
    await (modal as HTMLElement).click();
    await flushPromises();
    expect(reverseAdminAction).toHaveBeenCalledWith("1", "unsilence");
    expect(adminAccountAction).not.toHaveBeenCalled();
    expect(getAccount).toHaveBeenCalledWith("1");
  });

  it("hides a limited profile's timeline behind a notice for strangers", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice", limited: true }),
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
    const wrapper = await openProfile("/@alice");
    const notice = wrapper.find(".profile__limited");
    expect(notice.exists()).toBe(true);
    expect(notice.text()).toContain("limited on this instance");
    // Posts are not fetched until the viewer opts in.
    expect(accountStatuses).not.toHaveBeenCalled();
    await notice.get("button").trigger("click");
    await flushPromises();
    expect(accountStatuses).toHaveBeenCalled();
    expect(wrapper.find(".profile__limited").exists()).toBe(false);
    expect(wrapper.find(".timeline").exists()).toBe(true);
  });

  it("shows a limited profile's timeline to its followers directly", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice", limited: true }),
    );
    vi.mocked(getRelationships).mockResolvedValue([
      {
        id: "1",
        following: true,
        followed_by: false,
        blocking: false,
        muting: false,
        note: "",
      } as never,
    ]);
    const wrapper = await openProfile("/@alice");
    expect(wrapper.find(".profile__limited").exists()).toBe(false);
    expect(accountStatuses).toHaveBeenCalled();
  });

  it("shows a limited profile's timeline to the owner directly", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "1" } as never;
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice", limited: true }),
    );
    const wrapper = await openProfile("/@alice");
    expect(wrapper.find(".profile__limited").exists()).toBe(false);
    expect(accountStatuses).toHaveBeenCalled();
  });

  it("offers 'Unsuspend account' on a suspended account and reverses", async () => {
    const auth = useAuthStore();
    auth.setToken("tk");
    auth.account = { id: "42" } as never;
    auth.role = "admin";
    vi.mocked(lookup).mockResolvedValue(
      account({ id: "1", acct: "alice", suspended: true }),
    );
    vi.mocked(getAccount).mockResolvedValue(
      account({ id: "1", acct: "alice", suspended: false }),
    );
    vi.mocked(reverseAdminAction).mockResolvedValue({} as never);
    const wrapper = await openProfile("/@alice");
    await wrapper.get(".menu__trigger").trigger("click");
    const unsuspend = menuItems().find((i) => i.text() === "Unsuspend account");
    expect(unsuspend).toBeTruthy();
    await unsuspend!.trigger("click");
    const modal = document.querySelector(".modal__body .btn--danger");
    expect(modal).toBeTruthy();
    await (modal as HTMLElement).click();
    await flushPromises();
    expect(reverseAdminAction).toHaveBeenCalledWith("1", "unsuspend");
  });
});

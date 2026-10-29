import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import StatusInteractionsPage from "./StatusInteractionsPage.vue";
import type { Account, Status } from "@/api/entities";

vi.mock("@/api/emojis", () => ({
  customEmojis: vi.fn().mockResolvedValue([]),
}));

function account(over: Partial<Account> = {}): Account {
  return {
    id: "7",
    username: "bob",
    acct: "bob",
    display_name: "Bob",
    avatar: "",
    url: "https://x.test/@bob",
    ...over,
  } as Account;
}

function status(over: Partial<Status> = {}): Status {
  return {
    id: "1",
    uri: "https://x.test/s/1",
    url: "https://x.test/s/1",
    account: account({ acct: "alice", username: "alice" }),
    content: "<p>hello</p>",
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

function mockApi(map: Record<string, unknown>) {
  return vi
    .spyOn(globalThis, "fetch")
    .mockImplementation((input: RequestInfo | URL) => {
      const url = String(input);
      for (const [path, body] of Object.entries(map)) {
        if (url.endsWith(path)) {
          return Promise.resolve(
            new Response(JSON.stringify(body), { status: 200 }),
          );
        }
      }
      return Promise.resolve(
        new Response(JSON.stringify({ error: "nf" }), { status: 404 }),
      );
    });
}

describe("StatusInteractionsPage", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("lists likers as account rows", async () => {
    const spy = mockApi({
      "/api/v1/statuses/1": status(),
      "/api/v1/statuses/1/favourited_by": [account()],
    });
    const { wrapper } = mountWith(StatusInteractionsPage, {
      props: { id: "1", kind: "likes" },
    });
    await flushPromises();
    expect(
      spy.mock.calls.some((c) =>
        String(c[0]).includes("/statuses/1/favourited_by"),
      ),
    ).toBe(true);
    const row = wrapper.get(".interactions__row");
    expect(row.text()).toContain("Bob");
    expect(row.attributes("href")).toBe("/@bob");
  });

  it("lists boosters via reblogged_by", async () => {
    const spy = mockApi({
      "/api/v1/statuses/1": status(),
      "/api/v1/statuses/1/reblogged_by": [account({ acct: "carol" })],
    });
    const { wrapper } = mountWith(StatusInteractionsPage, {
      props: { id: "1", kind: "boosts" },
    });
    await flushPromises();
    expect(
      spy.mock.calls.some((c) =>
        String(c[0]).includes("/statuses/1/reblogged_by"),
      ),
    ).toBe(true);
    expect(wrapper.get(".interactions__row").text()).toContain("carol");
  });

  it("lists quotes as status cards", async () => {
    const spy = mockApi({
      "/api/v1/statuses/1": status(),
      "/api/v1/statuses/1/quotes": [
        status({ id: "9", content: "<p>my take</p>" }),
      ],
    });
    const { wrapper } = mountWith(StatusInteractionsPage, {
      props: { id: "1", kind: "quotes" },
    });
    await flushPromises();
    expect(
      spy.mock.calls.some((c) => String(c[0]).includes("/statuses/1/quotes")),
    ).toBe(true);
    expect(wrapper.find(".status-card").exists()).toBe(true);
    expect(wrapper.find(".status-card__content").html()).toContain("my take");
  });

  it("switches lists through the tabs", async () => {
    mockApi({
      "/api/v1/statuses/1": status(),
      "/api/v1/statuses/1/reblogged_by": [account()],
    });
    const { wrapper, router } = mountWith(StatusInteractionsPage, {
      props: { id: "1", kind: "boosts" },
    });
    await flushPromises();
    const tabs = wrapper.findAll(".tabs button");
    await tabs.find((b) => b.text() === "Quotes")!.trigger("click");
    await flushPromises();
    expect(router.currentRoute.value.path).toBe("/statuses/1/quotes");
  });

  it("shows not-found for an invisible status", async () => {
    mockApi({});
    const { wrapper } = mountWith(StatusInteractionsPage, {
      props: { id: "1", kind: "likes" },
    });
    await flushPromises();
    expect(wrapper.text()).toContain("This status is not available.");
  });
});

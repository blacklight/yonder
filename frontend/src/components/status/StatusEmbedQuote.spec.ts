import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import StatusEmbedQuote from "./StatusEmbedQuote.vue";
import type { Account, Status } from "@/api/entities";

vi.mock("@/api/statuses", async (orig) => {
  const m = await orig<typeof import("@/api/statuses")>();
  return { ...m, getStatus: vi.fn() };
});

import { getStatus } from "@/api/statuses";

function account(over: Partial<Account> = {}): Account {
  return {
    id: "7",
    username: "alice",
    acct: "alice",
    display_name: "Alice",
    note: "",
    url: "https://x.test/@alice",
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
    ...over,
  } as Account;
}

function status(over: Partial<Status> = {}): Status {
  return {
    id: "10",
    uri: "https://x.test/s/10",
    url: "https://x.test/@alice/10",
    account: account(),
    content: "<p>post</p>",
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

const quoting = (target: Status | null, extra: Partial<Status> = {}) =>
  status({
    quote_id: target?.id ?? "99",
    quote: { state: "accepted", quoted_status: target },
    ...extra,
  });

describe("StatusEmbedQuote", () => {
  beforeEach(() => {
    vi.mocked(getStatus).mockReset();
  });

  it("renders a materialized quoted status without fetching", async () => {
    const inner = status({ id: "9", content: "<p>the original</p>" });
    const { wrapper } = mountWith(StatusEmbedQuote, {
      props: { status: quoting(inner) },
    });
    await flushPromises();
    expect(getStatus).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("the original");
    expect(wrapper.text()).toContain("@alice");
  });

  it("fetches a depth-trimmed quote and nests its own quote", async () => {
    // A quotes B quotes C: B's entity (nested in A) carries quote_id
    // but no quoted_status; fetching B yields C materialized.
    const c = status({ id: "8", content: "<p>deepest</p>" });
    const b = status({
      id: "9",
      content:
        '<p>middle</p><p>RE: <a href="https://x.test/@alice/8">RE</a></p>',
      quote_id: "8",
      quote: { state: "accepted", quoted_status: c },
    });
    vi.mocked(getStatus).mockResolvedValue(b);
    const a = quoting(null, { quote_id: "9" });
    const { wrapper } = mountWith(StatusEmbedQuote, {
      props: { status: a },
    });
    await flushPromises();

    expect(getStatus).toHaveBeenCalledWith("9");
    expect(wrapper.text()).toContain("middle");
    expect(wrapper.text()).toContain("deepest");
    // B's RE: link is redundant once C's card renders.
    expect(wrapper.text()).not.toContain("RE");
    expect(wrapper.findAll("[data-testid='embed-quote']")).toHaveLength(2);
  });

  it("shows the unavailable card when the fetch fails", async () => {
    vi.mocked(getStatus).mockRejectedValue(new Error("404"));
    const { wrapper } = mountWith(StatusEmbedQuote, {
      props: { status: quoting(null) },
    });
    await flushPromises();
    expect(wrapper.text()).toContain("unavailable");
  });

  it("links pending remote quotes to their AP uri", async () => {
    const s = status({
      quote: { state: "pending", quoted_status: null },
      yonder: { quote: { id: null, uri: "https://remote.test/s/1" } },
    });
    const { wrapper } = mountWith(StatusEmbedQuote, {
      props: { status: s },
    });
    await flushPromises();
    const link = wrapper.get(".embed-quote--unavailable");
    expect(link.attributes("href")).toBe("https://remote.test/s/1");
  });

  it("stops fetching at the depth cap", async () => {
    const { wrapper } = mountWith(StatusEmbedQuote, {
      props: { status: quoting(null), depth: 4 },
    });
    await flushPromises();
    expect(getStatus).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("unavailable");
  });

  it("keeps quoted CW media and link preview collapsed until expanded", async () => {
    const inner = status({
      id: "9",
      content: "<p>hidden post</p>",
      spoiler_text: "heads up",
      media_attachments: [
        {
          id: "m1",
          type: "image",
          url: "https://x.test/full.png",
          preview_url: "https://x.test/preview.png",
          remote_url: null,
          description: null,
          blurhash: null,
        },
      ],
      card: {
        url: "https://link.test/hidden",
        title: "Hidden link",
        description: "",
        image: null,
        type: "link",
      },
    });
    const { wrapper } = mountWith(StatusEmbedQuote, {
      props: { status: quoting(inner) },
    });

    expect(
      (wrapper.find(".embed-quote .cw__body").element as HTMLElement).style
        .display,
    ).toBe("none");
    expect(wrapper.find(".embed-quote .status-media img").exists()).toBe(true);
    expect(wrapper.find(".embed-quote .embed-card__preview").exists()).toBe(
      true,
    );
    await wrapper.get(".embed-quote .cw__toggle").trigger("click");
    await flushPromises();
    expect(
      (wrapper.find(".embed-quote .cw__body").element as HTMLElement).style
        .display,
    ).not.toBe("none");
  });
});

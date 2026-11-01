import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import StatusEmbedPage from "./StatusEmbedPage.vue";
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
    content: "<p>hello embed</p>",
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
    replies_count: 2,
    reblogs_count: 3,
    favourites_count: 4,
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

describe("StatusEmbedPage", () => {
  beforeEach(() => {
    vi.mocked(getStatus).mockReset();
  });

  it("renders the status with counts and reports its height", async () => {
    const postMessage = vi.spyOn(window.parent, "postMessage");
    vi.mocked(getStatus).mockResolvedValue(status());
    const { wrapper } = mountWith(StatusEmbedPage, { props: { id: "10" } });
    await flushPromises();

    expect(getStatus).toHaveBeenCalledWith("10");
    expect(wrapper.text()).toContain("Alice");
    expect(wrapper.text()).toContain("@alice");
    expect(wrapper.text()).toContain("hello embed");
    expect(wrapper.text()).toContain("2 replies");
    expect(wrapper.text()).toContain("3 boosts");
    expect(wrapper.text()).toContain("4 likes");
    expect(wrapper.get(".embed-card__permalink").attributes("href")).toBe(
      "https://x.test/@alice/10",
    );
    expect(postMessage).toHaveBeenCalledWith(
      expect.objectContaining({ source: "yonder-embed", type: "resize" }),
      "*",
    );
  });

  it("resolves a boost wrapper to the boosted status", async () => {
    const inner = status({ id: "9", content: "<p>the original</p>" });
    const boost = status({ id: "10", reblog: inner });
    vi.mocked(getStatus).mockResolvedValue(boost);
    const { wrapper } = mountWith(StatusEmbedPage, { props: { id: "10" } });
    await flushPromises();

    expect(wrapper.text()).toContain("the original");
    expect(wrapper.text()).toContain("boosted");
  });

  it("renders the quoted status and strips the RE: fallback", async () => {
    const inner = status({ id: "9", content: "<p>the original</p>" });
    vi.mocked(getStatus).mockResolvedValue(
      status({
        content:
          '<p>outer text</p><p>RE: <a href="https://x.test/@alice/9">RE</a></p>',
        quote_id: "9",
        quote: { state: "accepted", quoted_status: inner },
      }),
    );
    const { wrapper } = mountWith(StatusEmbedPage, { props: { id: "10" } });
    await flushPromises();

    expect(wrapper.text()).toContain("outer text");
    expect(wrapper.text()).toContain("the original");
    expect(wrapper.get("[data-testid='embed-quote']")).toBeTruthy();
    expect(wrapper.html()).not.toContain("RE:");
  });

  it("renders media attachments", async () => {
    vi.mocked(getStatus).mockResolvedValue(
      status({
        media_attachments: [
          {
            id: "m1",
            type: "image",
            url: "https://x.test/full.png",
            preview_url: "https://x.test/preview.png",
            remote_url: null,
            description: "a picture",
            blurhash: null,
          },
        ],
      }),
    );
    const { wrapper } = mountWith(StatusEmbedPage, { props: { id: "10" } });
    await flushPromises();

    expect(wrapper.find(".status-media").exists()).toBe(true);
    expect(wrapper.find(".status-media img").exists()).toBe(true);
  });

  it("renders the link-preview card", async () => {
    vi.mocked(getStatus).mockResolvedValue(
      status({
        card: {
          url: "https://blog.test/post",
          title: "A blog post",
          description: "Something worth reading",
          image: "https://x.test/card.png",
          type: "link",
          provider_name: "blog.test",
        },
      }),
    );
    const { wrapper } = mountWith(StatusEmbedPage, { props: { id: "10" } });
    await flushPromises();

    const card = wrapper.get(".embed-card__preview");
    expect(card.attributes("href")).toBe("https://blog.test/post");
    expect(card.attributes("target")).toBe("_blank");
    expect(card.text()).toContain("A blog post");
    expect(card.text()).toContain("blog.test");
  });

  it("hides preview cards with unsafe URLs", async () => {
    vi.mocked(getStatus).mockResolvedValue(
      status({
        card: {
          url: "javascript:alert(1)",
          title: "evil",
          description: "",
          image: null,
        },
      }),
    );
    const { wrapper } = mountWith(StatusEmbedPage, { props: { id: "10" } });
    await flushPromises();

    expect(wrapper.find(".embed-card__preview").exists()).toBe(false);
  });

  it("shows the unavailable state when the fetch fails", async () => {
    vi.mocked(getStatus).mockRejectedValue(new Error("404"));
    const { wrapper } = mountWith(StatusEmbedPage, { props: { id: "10" } });
    await flushPromises();

    expect(wrapper.get(".embed__error").text()).toContain("not available");
  });

  it("opens content links in a new tab instead of the frame", async () => {
    const open = vi
      .spyOn(window, "open")
      .mockImplementation(() => null as never);
    vi.mocked(getStatus).mockResolvedValue(
      status({ content: '<p><a href="/@alice">alice</a></p>' }),
    );
    const { wrapper } = mountWith(StatusEmbedPage, { props: { id: "10" } });
    await flushPromises();

    await wrapper.get(".embed-card__content a").trigger("click");
    expect(open).toHaveBeenCalledWith(
      `${window.location.origin}/@alice`,
      "_blank",
      "noopener",
    );
  });
});

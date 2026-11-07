import { describe, expect, it } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import StatusQuote from "./StatusQuote.vue";
import type { Account, MediaAttachment, Status } from "@/api/entities";

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

function media(over: Partial<MediaAttachment> = {}): MediaAttachment {
  return {
    id: "m1",
    type: "image",
    url: "https://x.test/full.png",
    preview_url: "https://x.test/preview.png",
    remote_url: null,
    description: "quoted image",
    blurhash: null,
    ...over,
  };
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

function quoting(quoted: Status): Status {
  return status({
    id: "11",
    quote_id: quoted.id,
    quote: { state: "accepted", quoted_status: quoted },
  });
}

describe("StatusQuote", () => {
  it("renders media attachments from the quoted status", () => {
    const quoted = status({
      id: "9",
      content: "<p>with image</p>",
      media_attachments: [media()],
      card: {
        url: "https://link.test/page",
        title: "Linked page",
        description: "A preview",
        image: null,
        type: "link",
        provider_name: "link.test",
      },
    });
    const { wrapper } = mountWith(StatusQuote, {
      props: { status: quoting(quoted) },
    });

    const img = wrapper.get(".quote .status-media img");
    expect(img.attributes("src")).toBe("https://x.test/preview.png");
    expect(img.attributes("alt")).toBe("quoted image");
    expect(wrapper.get(".quote .embed-card__preview").attributes("href")).toBe(
      "https://link.test/page",
    );
  });

  it("keeps quoted CW content, media and link preview collapsed until expanded", async () => {
    const quoted = status({
      id: "9",
      content: "<p>hidden post</p>",
      spoiler_text: "heads up",
      media_attachments: [media()],
      card: {
        url: "https://link.test/hidden",
        title: "Hidden link",
        description: "",
        image: null,
        type: "link",
      },
    });
    const { wrapper } = mountWith(StatusQuote, {
      props: { status: quoting(quoted) },
    });

    expect(
      (wrapper.find(".quote .cw__body").element as HTMLElement).style.display,
    ).toBe("none");
    await wrapper.get(".quote .cw__toggle").trigger("click");
    await flushPromises();
    expect(
      (wrapper.find(".quote .cw__body").element as HTMLElement).style.display,
    ).not.toBe("none");
    expect(wrapper.find(".quote .status-media img").exists()).toBe(true);
    expect(wrapper.find(".quote .embed-card__preview").exists()).toBe(true);
  });
});

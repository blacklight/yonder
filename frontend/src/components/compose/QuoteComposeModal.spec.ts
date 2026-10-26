import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import { useComposeStore } from "@/stores/compose";
import QuoteComposeModal from "./QuoteComposeModal.vue";
import type { Status } from "@/api/entities";

vi.mock("@/api/emojis", () => ({
  customEmojis: vi.fn().mockResolvedValue([]),
}));

function status(over: Partial<Status> = {}): Status {
  return {
    id: "1",
    uri: "https://x.test/s/1",
    url: "https://x.test/s/1",
    account: {
      id: "7",
      username: "alice",
      acct: "alice",
      display_name: "Alice",
      avatar: "",
      url: "https://x.test/@alice",
    } as Status["account"],
    content: "<p>quoted body</p>",
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

describe("QuoteComposeModal", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(new Response(JSON.stringify({}), { status: 200 })),
    );
    useAuthStore().setToken("t");
  });

  it("shows the quoted card alongside the compose form", async () => {
    const s = status();
    useComposeStore().openQuote(s);
    const { wrapper } = mountWith(QuoteComposeModal, {
      props: { status: s },
    });
    await flushPromises();
    // Modal teleports to body.
    expect(document.body.textContent).toContain("Alice");
    expect(document.body.textContent).toContain("@alice");
    expect(document.body.textContent).toContain("quoted body");
    expect(document.body.querySelector("[data-testid='compose']")).toBeTruthy();
    wrapper.unmount();
  });

  it("closing the modal clears the embed but keeps the draft", async () => {
    const s = status();
    const compose = useComposeStore();
    compose.openQuote(s);
    compose.update({ text: "witty commentary" });
    const { wrapper } = mountWith(QuoteComposeModal, {
      props: { status: s },
    });
    await flushPromises();
    document.querySelector<HTMLElement>(".modal__close")!.click();
    await flushPromises();
    expect(compose.quoteTarget).toBeNull();
    expect(compose.draft.text).toBe("witty commentary");
    expect(compose.draft.quoteId).toBe("1");
    wrapper.unmount();
  });
});

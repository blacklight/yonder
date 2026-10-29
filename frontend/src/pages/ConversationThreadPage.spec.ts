import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import { useComposeStore } from "@/stores/compose";
import type { Status } from "@/api/entities";
import ConversationThreadPage from "./ConversationThreadPage.vue";

vi.mock("@/api/emojis", () => ({
  customEmojis: vi.fn().mockResolvedValue([]),
}));

const KEY_A = "a".repeat(64);
const KEY_B = "b".repeat(64);

function dm(id: string, acct: string): Status {
  return {
    id,
    created_at: "2026-10-26T10:00:00Z",
    content: `<p>dm from ${acct}</p>`,
    visibility: "direct",
    account: {
      id: `acct-${id}`,
      acct,
      username: acct.split("@")[0],
      display_name: "",
      avatar: "",
      url: "",
    },
    mentions: [],
    media_attachments: [],
    tags: [],
    emojis: [],
    replies_count: 0,
    reblogs_count: 0,
    favourites_count: 0,
    quotes_count: 0,
    pleroma: { emoji_reactions: [] },
  } as unknown as Status;
}

const STATUS_A = dm("s-a", "x@a.test");
const STATUS_B = dm("s-b", "y@b.test");

/** Thread statuses keyed by conversation id; everything else 200s. */
function mockApi() {
  return vi.spyOn(globalThis, "fetch").mockImplementation((input) => {
    const url = String(input);
    const body = url.includes(`/conversations/${KEY_A}/statuses`)
      ? [STATUS_A]
      : url.includes(`/conversations/${KEY_B}/statuses`)
        ? [STATUS_B]
        : {};
    return Promise.resolve(new Response(JSON.stringify(body), { status: 200 }));
  });
}

function mount(id: string) {
  return mountWith(ConversationThreadPage, { props: { id } });
}

describe("ConversationThreadPage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAuthStore().setToken("tk");
    useComposeStore().reset();
  });

  it("seeds the recipients of the latest message", async () => {
    mockApi();
    mount(KEY_A);
    await flushPromises();
    const draft = useComposeStore().draft;
    expect(draft.text).toBe("@x@a.test ");
    expect(draft.visibility).toBe("direct");
    expect(draft.inReplyToId).toBe("s-a");
  });

  it("replaces an untouched seed when another conversation opens", async () => {
    mockApi();
    mount(KEY_A);
    await flushPromises();
    expect(useComposeStore().draft.text).toBe("@x@a.test ");
    // Back to the list, open B — a fresh mount with the A seed still
    // persisted in the draft.
    mount(KEY_B);
    await flushPromises();
    const draft = useComposeStore().draft;
    expect(draft.text).toBe("@y@b.test ");
    expect(draft.inReplyToId).toBe("s-b");
  });

  it("re-seeds on an in-place param change (unkeyed route)", async () => {
    mockApi();
    const { wrapper } = mount(KEY_A);
    await flushPromises();
    await wrapper.setProps({ id: KEY_B });
    await flushPromises();
    const draft = useComposeStore().draft;
    expect(draft.text).toBe("@y@b.test ");
    expect(draft.inReplyToId).toBe("s-b");
  });

  it("resumes a reply draft that already targets this thread", async () => {
    const compose = useComposeStore();
    compose.openDirectReply(STATUS_B);
    compose.update({ text: "@y@b.test half-written reply" });
    mockApi();
    mount(KEY_B);
    await flushPromises();
    expect(compose.draft.text).toBe("@y@b.test half-written reply");
    expect(compose.draft.inReplyToId).toBe("s-b");
  });

  it("asks before discarding typed text from another context", async () => {
    const compose = useComposeStore();
    compose.openDirectReply(STATUS_A);
    compose.update({ text: "@x@a.test draft for A" });
    mockApi();
    const { wrapper } = mount(KEY_B);
    await flushPromises();
    // Typed text isn't context — it's held behind a confirm.
    const panel = document.querySelector(".modal__panel");
    expect(panel).not.toBeNull();
    expect(compose.draft.text).toBe("@x@a.test draft for A");
    panel!
      .querySelector<HTMLElement>("[data-testid='reseed-confirm']")!
      .click();
    await flushPromises();
    expect(compose.draft.text).toBe("@y@b.test ");
    expect(compose.draft.inReplyToId).toBe("s-b");
    expect(wrapper.find(".compose__private").exists()).toBe(true);
  });

  it("cancelling the confirm keeps the foreign draft", async () => {
    const compose = useComposeStore();
    compose.openDirectReply(STATUS_A);
    compose.update({ text: "@x@a.test draft for A" });
    mockApi();
    mount(KEY_B);
    await flushPromises();
    document
      .querySelector<HTMLElement>(".modal__panel .btn--secondary")!
      .click();
    await flushPromises();
    expect(compose.draft.text).toBe("@x@a.test draft for A");
    expect(compose.draft.inReplyToId).toBe("s-a");
  });
});

import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import { useComposeStore } from "@/stores/compose";
import type { Account, Conversation, Status } from "@/api/entities";
import ConversationsPage from "./ConversationsPage.vue";

const FABIO: Account = {
  id: "7",
  acct: "fabio@manganiello.eu",
  username: "fabio",
  display_name: "Fabio Manganiello",
  note: "",
  url: "https://manganiello.eu/@fabio",
  avatar: "https://cdn.example/fabio.png",
  header: "",
  locked: false,
  bot: false,
  created_at: "2026-01-01T00:00:00Z",
  followers_count: 0,
  following_count: 0,
  statuses_count: 0,
  fields: [],
  emojis: [],
};

const LAST: Status = {
  id: "42",
  created_at: "2026-10-26T10:00:00Z",
  content: "<p>@fabio questa è una prova</p>",
  account: FABIO,
} as unknown as Status;

function conv(extra: Record<string, unknown> = {}): Conversation {
  return {
    id: "a".repeat(64),
    unread: false,
    accounts: [FABIO],
    last_status: LAST,
    ...extra,
  };
}

/** Answer the list endpoint; DELETE requests resolve empty. */
function mockApi(items: Conversation[] = [conv()]) {
  return vi.spyOn(globalThis, "fetch").mockImplementation((input, init) => {
    const url = String(input);
    if (init?.method === "DELETE")
      return Promise.resolve(new Response("{}", { status: 200 }));
    if (url.includes("/conversations"))
      return Promise.resolve(
        new Response(JSON.stringify(items), { status: 200 }),
      );
    return Promise.resolve(new Response("{}", { status: 200 }));
  });
}

describe("ConversationsPage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAuthStore().setToken("tk");
  });

  it("renders conversations as cards with names, preview and unread dot", async () => {
    mockApi([
      conv(),
      conv({
        id: "b".repeat(64),
        unread: true,
        accounts: [{ ...FABIO, id: "8", display_name: "", acct: "bob@x" }],
      }),
    ]);
    const { wrapper } = mountWith(ConversationsPage);
    await flushPromises();
    const rows = wrapper.findAll(".conv");
    expect(rows).toHaveLength(2);
    expect(rows[0].get(".conv-names").text()).toBe("Fabio Manganiello");
    expect(rows[0].get(".preview").text()).toContain("questa è una prova");
    expect(rows[0].find(".dot").exists()).toBe(false);
    expect(rows[1].classes()).toContain("unread");
    expect(rows[1].get(".dot").exists()).toBe(true);
    expect(rows[1].get(".conv-when").exists()).toBe(true);
  });

  it("opens the thread on click and on Enter", async () => {
    mockApi();
    const { wrapper, router } = mountWith(ConversationsPage);
    await flushPromises();
    await wrapper.get(".conv").trigger("click");
    await flushPromises();
    expect(router.currentRoute.value.path).toBe(`/messages/${"a".repeat(64)}`);
  });

  it("New message seeds a direct draft and opens /compose", async () => {
    mockApi();
    const { wrapper, router } = mountWith(ConversationsPage);
    await flushPromises();
    await wrapper.get("[data-testid='new-message']").trigger("click");
    await flushPromises();
    expect(useComposeStore().draft.visibility).toBe("direct");
    expect(router.currentRoute.value.path).toBe("/compose");
  });

  it("New message resumes an in-progress draft instead of clobbering it", async () => {
    const compose = useComposeStore();
    compose.update({ text: "half-written public post" });
    mockApi();
    const { wrapper, router } = mountWith(ConversationsPage);
    await flushPromises();
    await wrapper.get("[data-testid='new-message']").trigger("click");
    await flushPromises();
    expect(compose.draft.text).toBe("half-written public post");
    expect(compose.draft.visibility).toBe("public");
    expect(router.currentRoute.value.path).toBe("/compose");
  });

  it("delete asks for confirmation, then removes the row", async () => {
    const spy = mockApi();
    const { wrapper } = mountWith(ConversationsPage);
    await flushPromises();
    // The button stops propagation — clicking it must not open the thread.
    await wrapper.get(".conv .del").trigger("click");
    // The modal teleports to <body> — it's outside the wrapper tree.
    const panel = document.querySelector(".modal__panel");
    expect(panel).not.toBeNull();
    panel!.querySelector<HTMLElement>(".btn--danger")!.click();
    await flushPromises();
    expect(
      spy.mock.calls.some(
        ([u, i]) =>
          String(u).includes("/conversations/") && i?.method === "DELETE",
      ),
    ).toBe(true);
    expect(wrapper.findAll(".conv")).toHaveLength(0);
  });

  it("shows the empty state when there are no conversations", async () => {
    mockApi([]);
    const { wrapper } = mountWith(ConversationsPage);
    await flushPromises();
    expect(wrapper.findAll(".conv")).toHaveLength(0);
    expect(wrapper.get(".conv-state").text()).toContain("No conversations");
  });
});

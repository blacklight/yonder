import { describe, expect, it, vi, beforeEach } from "vitest";
import { DOMWrapper, flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import StatusCard from "./StatusCard.vue";
import type { Status } from "@/api/entities";

vi.mock("@/api/emojis", () => ({
  customEmojis: vi.fn().mockResolvedValue([]),
}));

// The menu list teleports to <body> — query it at document level.
const menuItems = () =>
  [...document.querySelectorAll<HTMLElement>("[role='menuitem']")].map(
    (el) => new DOMWrapper(el),
  );

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

function mockApi(status = 200, body: unknown = {}) {
  return vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(() =>
      Promise.resolve(new Response(JSON.stringify(body), { status })),
    );
}

// jsdom reports every scrollHeight as 0 — ShowMore reads it to decide
// whether a body needs the clamp, so tests drive it per case.
let scrollHeight = 0;

describe("StatusCard", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    scrollHeight = 0;
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe() {}
        unobserve() {}
        disconnect() {}
      },
    );
    vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(
      () => scrollHeight,
    );
  });

  it("renders header, content and footer", () => {
    const { wrapper } = mountWith(StatusCard, { props: { status: status() } });
    expect(wrapper.text()).toContain("Alice");
    expect(wrapper.text()).toContain("@alice");
    expect(wrapper.find(".status-card__content").html()).toContain("hello");
    expect(wrapper.find(".status-card__favicon").exists()).toBe(false); // local
  });

  it("shows favicon for remote accounts and hides it on error", async () => {
    const s = status();
    s.account.acct = "bob@remote.test";
    const { wrapper } = mountWith(StatusCard, { props: { status: s } });
    const icon = wrapper.find(".status-card__favicon");
    expect(icon.attributes("src")).toBe("https://remote.test/favicon.ico");
    await icon.trigger("error");
    expect(wrapper.find(".status-card__favicon").exists()).toBe(false);
  });

  it("renders the boost wrapper for reblogs", () => {
    const s = status({ reblog: status({ id: "9" }) });
    s.reblog!.content = "<p>inner</p>";
    const { wrapper } = mountWith(StatusCard, { props: { status: s } });
    expect(wrapper.text()).toContain("boosted");
    expect(wrapper.find(".status-card__content").html()).toContain("inner");
  });

  it("hides content behind a CW and toggles it", async () => {
    const s = status({ spoiler_text: "nsfw", sensitive: true });
    const { wrapper } = mountWith(StatusCard, { props: { status: s } });
    expect(wrapper.find(".cw__body").attributes("style")).toContain(
      "display: none",
    );
    await wrapper.get(".cw__toggle").trigger("click");
    await flushPromises();
    expect(wrapper.find(".cw__body").attributes("style") ?? "").not.toContain(
      "display: none",
    );
  });

  it("clamps tall bodies behind Show more — skipped for CW + expanded", async () => {
    scrollHeight = 1000;
    const media = {
      id: "m1",
      type: "image",
      url: "https://x.test/m.png",
      preview_url: null,
      remote_url: null,
      description: null,
      blurhash: null,
    } as Status["media_attachments"][number];
    const { wrapper } = mountWith(StatusCard, {
      props: { status: status({ media_attachments: [media] }) },
    });
    await flushPromises(); // the onMounted measure lands on next flush
    const toggle = wrapper.get(".show-more__toggle");
    expect(toggle.text()).toBe("Show more");
    await toggle.trigger("click");
    await flushPromises();
    expect(wrapper.get(".show-more__toggle").text()).toBe("Show less");

    // Media renders whole, outside the clamped text — never sliced
    // by the fade.
    const clamp = wrapper.get(".show-more");
    expect(clamp.find(".status-media").exists()).toBe(false);
    expect(wrapper.find(".status-media").exists()).toBe(true);

    // CW'd bodies are already gated by CwBlock — no second clamp.
    const { wrapper: cw } = mountWith(StatusCard, {
      props: { status: status({ spoiler_text: "nsfw" }) },
    });
    await flushPromises();
    expect(cw.find(".show-more__toggle").exists()).toBe(false);

    // The focal permalink status renders in full.
    const { wrapper: detail } = mountWith(StatusCard, {
      props: { status: status(), expanded: true },
    });
    await flushPromises();
    expect(detail.find(".show-more__toggle").exists()).toBe(false);
  });

  it("renders preview cards only for http(s) URLs", () => {
    const base = { title: "t", description: "d", image: null };
    const good = status({
      card: { ...base, url: "https://link.test/page" } as never,
    });
    const { wrapper: w1 } = mountWith(StatusCard, {
      props: { status: good },
    });
    expect(w1.get(".status-card__preview").attributes("href")).toBe(
      "https://link.test/page",
    );

    const evil = status({
      card: { ...base, url: "javascript:alert(1)" } as never,
    });
    const { wrapper: w2 } = mountWith(StatusCard, {
      props: { status: evil },
    });
    expect(w2.find(".status-card__preview").exists()).toBe(false);
  });

  it("sanitizes content", () => {
    const s = status({ content: "<p>ok</p><script>alert(1)</script>" });
    const { wrapper } = mountWith(StatusCard, { props: { status: s } });
    expect(wrapper.find(".status-card__content").html()).not.toContain(
      "script",
    );
  });

  it("like toggles optimistically and rolls back on ApiError", async () => {
    useAuthStore().setToken("t");
    const spy = mockApi(200, {});
    const s = status();
    const { wrapper } = mountWith(StatusCard, { props: { status: s } });
    await wrapper.get("[data-testid='like']").trigger("click");
    expect(s.favourited).toBe(true);
    expect(s.favourites_count).toBe(1);
    expect(spy.mock.calls[0][0]).toBe("/api/v1/statuses/1/favourite");

    // A failed *un*favourite rolls back to the pre-click state.
    vi.restoreAllMocks();
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(
        new Response(JSON.stringify({ error: "nope" }), { status: 422 }),
      ),
    );
    await wrapper.get("[data-testid='like']").trigger("click");
    await flushPromises();
    expect(s.favourited).toBe(true);
    expect(s.favourites_count).toBe(1);
  });

  it("dropdown copy link + mention + staff actions", async () => {
    const auth = useAuthStore();
    auth.setToken("t");
    auth.role = "moderator";
    auth.account = { id: "7" } as never;
    const spy = mockApi(200, {});
    const { wrapper } = mountWith(StatusCard, { props: { status: status() } });
    await wrapper.get(".menu__trigger").trigger("click");
    const items = menuItems();
    const texts = items.map((i) => i.text());
    expect(texts.some((x) => x.includes("Copy"))).toBe(true);
    expect(texts.some((x) => x.includes("Mark sensitive"))).toBe(true);
    await items
      .find((i) => i.text().includes("Mark sensitive"))!
      .trigger("click");
    await flushPromises();
    expect(
      spy.mock.calls.some((c) =>
        String(c[0]).includes("/admin/statuses/1/action"),
      ),
    ).toBe(true);
  });

  it("destructive menu actions require confirmation first", async () => {
    const auth = useAuthStore();
    auth.setToken("t");
    auth.account = { id: "7" } as never; // own status → delete item
    const spy = mockApi(200, {});
    const { wrapper } = mountWith(StatusCard, { props: { status: status() } });
    await wrapper.get(".menu__trigger").trigger("click");
    await menuItems()
      .find((i) => i.text().includes("Delete"))!
      .trigger("click");
    await flushPromises();
    // Nothing was sent — the confirm dialog is up (teleported to body).
    expect(spy).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain("cannot be undone");
    document.querySelector<HTMLElement>("[data-testid='confirm-yes']")!.click();
    await flushPromises();
    const del = spy.mock.calls.find(
      (c) => c[1]?.method === "DELETE" && String(c[0]).includes("/statuses/1"),
    );
    expect(del).toBeTruthy();
    expect(wrapper.emitted("removed")?.[0]).toEqual(["1"]);
  });

  it("admin-limit sends the limit verb after confirmation", async () => {
    const auth = useAuthStore();
    auth.setToken("t");
    auth.role = "moderator";
    const spy = mockApi(200, {});
    const { wrapper } = mountWith(StatusCard, { props: { status: status() } });
    await wrapper.get(".menu__trigger").trigger("click");
    await menuItems()
      .find((i) => i.text().includes("Limit author"))!
      .trigger("click");
    await flushPromises();
    document.querySelector<HTMLElement>("[data-testid='confirm-yes']")!.click();
    await flushPromises();
    const call = spy.mock.calls.find((c) =>
      String(c[0]).includes("/admin/accounts/7/action"),
    );
    expect(call).toBeTruthy();
    expect(JSON.parse(call![1]!.body as string)).toEqual({ type: "limit" });
  });

  it("reply asks before replacing a non-empty draft", async () => {
    useAuthStore().setToken("t");
    const { useComposeStore } = await import("@/stores/compose");
    const compose = useComposeStore();
    compose.update({ text: "work in progress" });
    const { wrapper } = mountWith(StatusCard, { props: { status: status() } });
    await wrapper.get(".menu__trigger").trigger("click");
    await menuItems()
      .find((i) => i.text().includes("Mention"))!
      .trigger("click");
    await flushPromises();
    // The draft survived — the discard confirm is showing instead.
    expect(compose.draft.text).toBe("work in progress");
    expect(document.body.textContent).toContain("replaced");
    document.querySelector<HTMLElement>("[data-testid='confirm-yes']")!.click();
    await flushPromises();
    expect(compose.draft.text).toBe("@alice ");
  });

  it("reply unwraps an inline compose form under the card", async () => {
    useAuthStore().setToken("t");
    mockApi(200, {});
    const { useComposeStore } = await import("@/stores/compose");
    const { wrapper } = mountWith(StatusCard, { props: { status: status() } });
    expect(wrapper.find("[data-testid='compose']").exists()).toBe(false);
    await wrapper.get("[data-testid='reply']").trigger("click");
    await flushPromises();
    const compose = useComposeStore();
    expect(compose.replyTargetId).toBe("1");
    expect(compose.draft.inReplyToId).toBe("1");
    const form = wrapper.get("[data-testid='compose']");
    expect(form.text()).toContain("Replying to");
    // Cancelling the reply context dismisses the form again.
    compose.reset();
    await flushPromises();
    expect(wrapper.find("[data-testid='compose']").exists()).toBe(false);
  });

  it("quote opens the compose modal on the quoted status", async () => {
    useAuthStore().setToken("t");
    const { useComposeStore } = await import("@/stores/compose");
    const { wrapper } = mountWith(StatusCard, { props: { status: status() } });
    await wrapper.get("[data-testid='quote']").trigger("click");
    await flushPromises();
    const compose = useComposeStore();
    expect(compose.quoteTarget?.id).toBe("1");
    expect(compose.draft.quoteId).toBe("1");
    // No inline reply form — the composer lives in the modal.
    expect(wrapper.find("[data-testid='compose']").exists()).toBe(false);
  });

  it("directReply retargets the thread composer as a DM, no inline form", async () => {
    useAuthStore().setToken("t");
    const { useComposeStore } = await import("@/stores/compose");
    const { wrapper } = mountWith(StatusCard, {
      props: { status: status(), directReply: true },
    });
    await wrapper.get("[data-testid='reply']").trigger("click");
    await flushPromises();
    const compose = useComposeStore();
    expect(compose.draft.visibility).toBe("direct");
    expect(compose.draft.inReplyToId).toBe("1");
    expect(compose.replyTargetId).toBeNull();
    expect(wrapper.find("[data-testid='compose']").exists()).toBe(false);
  });

  it("react/unreact via picker and existing chips", async () => {
    useAuthStore().setToken("t");
    const spy = mockApi(200, {});
    const s = status({
      pleroma: { emoji_reactions: [{ name: "👍", count: 2, me: true }] },
    });
    const { wrapper } = mountWith(StatusCard, { props: { status: s } });
    await wrapper.get("[data-testid='react']").trigger("click");
    expect(wrapper.find("[data-testid='reaction-picker']").exists()).toBe(true);
    await wrapper
      .findAll(".reaction-picker__item")
      .find((b) => b.text().includes("😂"))!
      .trigger("click");
    await flushPromises();
    expect(
      spy.mock.calls.some((c) =>
        String(c[0]).includes("/reactions/%F0%9F%98%82"),
      ),
    ).toBe(true);

    // Clicking a `me` reaction unreacts (DELETE).
    await wrapper.get(".status-card__reaction").trigger("click");
    await flushPromises();
    const del = spy.mock.calls.find((c) => c[1]?.method === "DELETE");
    expect(del).toBeTruthy();
    expect(String(del![0])).toContain("/reactions/");
  });
});

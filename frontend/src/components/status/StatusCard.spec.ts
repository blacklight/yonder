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

  it("a direct status hides boost/quote and reply seeds a DM draft", async () => {
    useAuthStore().setToken("t");
    const { useComposeStore } = await import("@/stores/compose");
    const { wrapper, router } = mountWith(StatusCard, {
      props: { status: status({ visibility: "direct" }) },
    });
    // No public surface — the buttons aren't rendered at all.
    expect(wrapper.find("[data-testid='boost']").exists()).toBe(false);
    expect(wrapper.find("[data-testid='quote']").exists()).toBe(false);
    await wrapper.get("[data-testid='reply']").trigger("click");
    await flushPromises();
    const compose = useComposeStore();
    expect(compose.draft.visibility).toBe("direct");
    expect(compose.draft.inReplyToId).toBe("1");
    // No inline composer for a DM — the seeded draft moves to /compose.
    expect(compose.replyTargetId).toBeNull();
    expect(router.currentRoute.value.path).toBe("/compose");
  });

  it("shows a visibility icon with the matching label", () => {
    const cases = [
      ["public", "fa-globe", "Public"],
      ["unlisted", "fa-lock-open", "Unlisted"],
      ["private", "fa-lock", "Followers only"],
      ["direct", "fa-envelope", "Direct"],
    ] as const;
    for (const [visibility, icon, label] of cases) {
      const { wrapper } = mountWith(StatusCard, {
        props: { status: status({ visibility }) },
      });
      const el = wrapper.get("[data-testid='visibility']");
      expect(el.attributes("aria-label")).toBe(label);
      expect(el.find(`.${icon}`).exists()).toBe(true);
      wrapper.unmount();
    }
  });

  it("translates a foreign-language public status and toggles back", async () => {
    const { useInstanceStore } = await import("@/stores/instance");
    useInstanceStore().entity = {
      configuration: { translation: { enabled: true } },
    };
    useAuthStore().setToken("t");
    const spy = vi.spyOn(globalThis, "fetch").mockImplementation((input) => {
      const url = String(input instanceof Request ? input.url : input);
      if (url.includes("/translate")) {
        return Promise.resolve(
          new Response(
            JSON.stringify({
              content: "<p>translated hello</p>",
              detected_source_language: "de",
              media_attachments: [{ id: "9", description: "alt-en" }],
              poll: { id: "5", options: [{ title: "yes-en" }] },
              provider: "TestTranslate",
              spoiler_text: "spoiler-en",
            }),
            { status: 200 },
          ),
        );
      }
      return Promise.resolve(new Response("{}", { status: 200 }));
    });
    const { wrapper } = mountWith(StatusCard, {
      props: {
        status: status({
          language: "de",
          spoiler_text: "spoiler-de",
          media_attachments: [
            {
              id: "9",
              type: "image",
              url: "https://x.test/i.png",
              preview_url: null,
              remote_url: null,
              description: "alt-de",
              blurhash: null,
            },
          ],
          poll: {
            id: "5",
            expires_at: null,
            expired: false,
            multiple: false,
            votes_count: 0,
            voters_count: 0,
            voted: false,
            own_votes: [],
            options: [{ title: "yes-de", votes_count: 0 }],
          },
        }),
      },
    });
    await flushPromises();
    const btn = wrapper.get("[data-testid='translate']");
    expect(btn.text()).toContain("Translate");
    await btn.trigger("click");
    await flushPromises();

    const call = spy.mock.calls.find(([i]) =>
      String(i instanceof Request ? i.url : i).includes("/translate"),
    );
    expect(call).toBeTruthy();
    expect(String(call![0] instanceof Request ? call![0].url : call![0])).toBe(
      "/api/v1/statuses/1/translate",
    );
    expect(JSON.parse(String(call![1]?.body))).toEqual({ lang: "en" });

    // Body, CW, poll options and alt text all swap to the translation.
    expect(wrapper.find(".status-card__content").text()).toBe(
      "translated hello",
    );
    expect(wrapper.text()).toContain("spoiler-en");
    expect(wrapper.text()).toContain("yes-en");
    expect(wrapper.find(".status-media img").attributes("alt")).toContain(
      "alt-en",
    );
    expect(wrapper.text()).toContain("TestTranslate");

    await wrapper.get("[data-testid='show-original']").trigger("click");
    expect(wrapper.find(".status-card__content").text()).toBe("hello");
    expect(wrapper.get("[data-testid='translate']").exists()).toBe(true);
  });

  it("hides translate when not eligible", async () => {
    const { useInstanceStore } = await import("@/stores/instance");
    const store = useInstanceStore();
    const cases = [
      // Same language as the interface.
      [{ language: "en", visibility: "public" }, true, false],
      // No declared language.
      [{ language: null, visibility: "public" }, true, false],
      // Foreign but the instance disabled translation.
      [{ language: "de", visibility: "public" }, false, false],
      // Foreign private/direct — visible to the viewer, so offered.
      [{ language: "de", visibility: "private" }, true, true],
      [{ language: "de", visibility: "direct" }, true, true],
    ] as const;
    for (const [over, enabled, expected] of cases) {
      store.entity = { configuration: { translation: { enabled } } };
      const { wrapper } = mountWith(StatusCard, {
        props: { status: status(over) },
      });
      await flushPromises();
      expect(wrapper.find("[data-testid='translate']").exists()).toBe(expected);
      wrapper.unmount();
    }
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

  it("expanded card links non-zero stats to the interaction lists", async () => {
    const s = status({
      reblogs_count: 3,
      quotes_count: 2,
      favourites_count: 0,
    });
    const { wrapper } = mountWith(StatusCard, {
      props: { status: s, expanded: true },
    });
    await flushPromises();
    const stats = wrapper.get(".status-card__stats");
    const links = stats.findAll("a.status-card__stat");
    expect(links.map((l) => l.attributes("href"))).toEqual([
      "/statuses/1/boosts",
      "/statuses/1/quotes",
    ]);
    // A zero counter renders as text, not a link.
    expect(stats.text()).toContain("0 likes");
    expect(stats.text()).toContain("3 boosts");
  });

  it("stats strip stays off timeline cards", () => {
    const { wrapper } = mountWith(StatusCard, {
      props: { status: status({ reblogs_count: 3 }) },
    });
    expect(wrapper.find(".status-card__stats").exists()).toBe(false);
  });

  it("own status gets a pin item that posts and flips the flag", async () => {
    const auth = useAuthStore();
    auth.setToken("t");
    auth.account = { id: "7" } as never;
    const s = status();
    const spy = mockApi(200, { ...s, pinned: true });
    const { wrapper } = mountWith(StatusCard, { props: { status: s } });
    await wrapper.get(".menu__trigger").trigger("click");
    await menuItems()
      .find((i) => i.text().includes("Pin on profile"))!
      .trigger("click");
    await flushPromises();
    const call = spy.mock.calls.find((c) =>
      String(c[0]).includes("/statuses/1/pin"),
    );
    expect(call).toBeTruthy();
    expect(s.pinned).toBe(true);
    expect(wrapper.emitted("pinned")?.[0][0]).toMatchObject({ pinned: true });
  });

  it("pinned own status offers unpin; foreign/direct/boost never offer pin", async () => {
    const auth = useAuthStore();
    auth.setToken("t");
    auth.account = { id: "7" } as never;
    const spy = mockApi(200, status({ pinned: false }));

    // Pinned own status → "Unpin from profile".
    const s = status({ pinned: true });
    const { wrapper } = mountWith(StatusCard, { props: { status: s } });
    await wrapper.get(".menu__trigger").trigger("click");
    await menuItems()
      .find((i) => i.text().includes("Unpin from profile"))!
      .trigger("click");
    await flushPromises();
    expect(
      spy.mock.calls.some((c) => String(c[0]).includes("/statuses/1/unpin")),
    ).toBe(true);
    expect(s.pinned).toBe(false);
    wrapper.unmount();

    const texts = async (st: Status) => {
      const { wrapper: w } = mountWith(StatusCard, {
        props: { status: st },
      });
      await w.get(".menu__trigger").trigger("click");
      const labels = menuItems().map((i) => i.text());
      w.unmount();
      document
        .querySelectorAll("[role='menuitem']")
        .forEach((el) => el.remove());
      return labels;
    };
    // Someone else's post — account.id differs from the viewer's.
    expect(
      (await texts(status({ account: { ...status().account, id: "9" } }))).some(
        (x) => x.includes("Pin"),
      ),
    ).toBe(false);
    // Direct posts can't be pinned.
    expect(
      (await texts(status({ visibility: "direct" }))).some((x) =>
        x.includes("Pin"),
      ),
    ).toBe(false);
    // A boost wrapper can't be pinned even when it's ours.
    const boost = status({ reblog: status({ id: "9" }) });
    boost.account.id = "7";
    expect((await texts(boost)).some((x) => x.includes("Pin"))).toBe(false);
  });

  it("pin marker renders only in profile context on a pinned status", () => {
    const pinned = status({ pinned: true });
    const { wrapper: timeline } = mountWith(StatusCard, {
      props: { status: pinned },
    });
    expect(timeline.find(".status-card__pin").exists()).toBe(false);
    timeline.unmount();

    const { wrapper: profile } = mountWith(StatusCard, {
      props: { status: pinned, pinnedContext: true },
    });
    expect(profile.get(".status-card__pin").text()).toContain("Pinned");
    profile.unmount();

    const { wrapper: plain } = mountWith(StatusCard, {
      props: { status: status(), pinnedContext: true },
    });
    expect(plain.find(".status-card__pin").exists()).toBe(false);
  });
});

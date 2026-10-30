import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createMemoryHistory, createRouter } from "vue-router";
import { i18n } from "@/i18n";
import { stubRoutes } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import { useNotificationsStore } from "@/stores/notifications";
import AppLayout from "./AppLayout.vue";

function layout() {
  vi.spyOn(globalThis, "fetch").mockImplementation((input) => {
    const url = String(input);
    if (url.includes("/instance"))
      return Promise.resolve(
        new Response(
          JSON.stringify({
            title: "Testville",
            version: "Yonder/9.9.9",
            domain: "testville.example",
          }),
          { status: 200 },
        ),
      );
    if (url.includes("unread_count"))
      return Promise.resolve(
        new Response(JSON.stringify({ count: 0 }), { status: 200 }),
      );
    if (url.includes("/conversations"))
      return Promise.resolve(new Response("[]", { status: 200 }));
    return Promise.resolve(new Response("{}", { status: 200 }));
  });
  const router = createRouter({
    history: createMemoryHistory(),
    routes: stubRoutes,
  });
  return mount(AppLayout, {
    global: { plugins: [i18n, router] },
    slots: { default: "<p>page</p>" },
  });
}

function labels(w: ReturnType<typeof layout>): string[] {
  return w
    .findAll(".layout__sidebar .navmenu__link")
    .map((n) => n.text().trim());
}

describe("AppLayout", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("anonymous menu shows public items only", () => {
    const w = layout();
    const names = labels(w);
    expect(names).toContain("Explore");
    expect(names).toContain("Local");
    expect(names).toContain("Sign in");
    expect(names).not.toContain("Home");
    expect(names).not.toContain("Notifications");
    expect(names).not.toContain("Moderation");
    expect(names).not.toContain("Admin");
  });

  it("logged-in user sees personal items", () => {
    const auth = useAuthStore();
    auth.setToken("t");
    auth.account = { acct: "alice" } as never;
    auth.role = "user";
    const w = layout();
    const names = labels(w);
    expect(names).toContain("Home");
    expect(names).toContain("Notifications");
    expect(names).toContain("Favourites");
    expect(names).toContain("Bookmarks");
    expect(names).toContain("Lists");
    expect(names).toContain("Messages");
    expect(names).not.toContain("Direct messages");
    expect(names).toContain("Sign out");
    expect(names).not.toContain("Moderation");
    expect(names).not.toContain("Admin");
  });

  it("moderator sees Moderation; admin sees both panels", () => {
    const auth = useAuthStore();
    auth.setToken("t");
    auth.role = "moderator";
    let names = labels(layout());
    expect(names).toContain("Moderation");
    expect(names).not.toContain("Administration");
    auth.role = "admin";
    names = labels(layout());
    expect(names).toContain("Administration");
    expect(names).toContain("Moderation");
  });

  it("admin section replaces the app menu with the panel menu", async () => {
    const auth = useAuthStore();
    auth.setToken("t");
    auth.role = "admin";
    const router = createRouter({
      history: createMemoryHistory(),
      routes: stubRoutes,
    });
    await router.push("/admin");
    vi.spyOn(globalThis, "fetch").mockImplementation((input) =>
      Promise.resolve(
        new Response(String(input).includes("/conversations") ? "[]" : "{}", {
          status: 200,
        }),
      ),
    );
    const w = mount(AppLayout, {
      global: { plugins: [i18n, router] },
    });
    const names = labels(w);
    expect(names[0]).toBe("Back");
    expect(names).toContain("General");
    expect(names).toContain("Users");
    expect(names).toContain("Audit log");
    expect(names).not.toContain("Explore");
    expect(names).not.toContain("Home");
    expect(names).not.toContain("Administration");
    expect(w.find(".layout__compose").exists()).toBe(false);
    // The Back entry leads out of the panel to the main app.
    const back = w.findAll(".layout__sidebar .navmenu__link")[0];
    expect(back.attributes("href")).toBe("/");
  });

  it("mobile bottom bar: anonymous sees browse shortcuts", () => {
    const w = layout();
    const links = w.findAll(".bottomnav__link");
    expect(links.map((l) => l.attributes("href"))).toEqual([
      "/",
      "/search",
      "/login",
    ]);
    expect(links.map((l) => l.attributes("aria-label"))).toEqual([
      "Home",
      "Search",
      "Sign in",
    ]);
    expect(w.find(".bottomnav .avatar").exists()).toBe(false);
  });

  it("mobile bottom bar: logged-in sees app shortcuts with badges", async () => {
    const auth = useAuthStore();
    auth.setToken("t");
    auth.account = {
      acct: "alice",
      avatar: "https://a.example/av.png",
    } as never;
    const n = useNotificationsStore();
    n.unread = 3;
    const { useConversationsStore } = await import("@/stores/conversations");
    useConversationsStore().items = [{ id: "c1", unread: true }] as never;

    const w = layout();
    const links = w.findAll(".bottomnav__link");
    expect(links.map((l) => l.attributes("href"))).toEqual([
      "/home",
      "/search",
      "/messages",
      "/notifications",
      "/profile",
    ]);
    // Count bubble on notifications, bare dot for unread DMs.
    const badges = w.findAll(".bottomnav__badge");
    expect(badges.map((b) => b.text())).toEqual(["", "3"]);
    expect(badges[0].classes()).toContain("bottomnav__badge--dot");
    expect(w.get(".bottomnav .avatar__img").attributes("src")).toBe(
      "https://a.example/av.png",
    );
  });

  it("renders the unread badge on notifications", () => {
    const auth = useAuthStore();
    auth.setToken("t");
    const n = useNotificationsStore();
    n.unread = 5;
    const w = layout();
    const badge = w.find('.navmenu__badge[data-badge-for="notifications"]');
    expect(badge.exists()).toBe(true);
    expect(badge.text()).toBe("5");
  });

  it("mobile menu opens, closes on Escape and overlay click", async () => {
    const w = layout();
    expect(document.querySelector("[data-testid='mobile-menu']")).toBeNull();
    await w.get(".layout__burger").trigger("click");
    expect(
      document.querySelector("[data-testid='mobile-menu']"),
    ).not.toBeNull();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await w.vm.$nextTick();
    expect(document.querySelector("[data-testid='mobile-menu']")).toBeNull();

    await w.get(".layout__burger").trigger("click");
    const overlay = document.querySelector(".mobile-menu__overlay")!;
    overlay.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await w.vm.$nextTick();
    expect(document.querySelector("[data-testid='mobile-menu']")).toBeNull();
  });

  it("wide-screen pane renders search, controls, version and links", async () => {
    const w = layout();
    await flushPromises();
    const aside = w.get(".layout__aside");
    expect(aside.find("input[type='search']").exists()).toBe(true);
    expect(aside.find("select.layout__locale").exists()).toBe(true);
    expect(aside.text()).toContain("Yonder/9.9.9");
    expect(aside.text()).toContain("testville.example");
    const links = aside.findAll(".layout__meta-links a");
    expect(links.map((a) => a.text())).toEqual(["About", "Source code"]);
  });

  it("search inputs show an X that clears the query", async () => {
    const w = layout();
    const inputs = w.findAll(".layout__search-input");
    expect(inputs.length).toBe(2);
    expect(w.find(".layout__search-clear").exists()).toBe(false);
    await inputs[0].setValue("cats");
    const clears = w.findAll(".layout__search-clear");
    // Both bars mirror the same query, so each shows its own X.
    expect(clears.length).toBe(2);
    await clears[1].trigger("click");
    expect(w.find(".layout__search-clear").exists()).toBe(false);
    expect(
      (w.findAll(".layout__search-input")[0].element as HTMLInputElement).value,
    ).toBe("");
  });

  it("sidebar keeps a search fallback below the wide breakpoint", () => {
    const w = layout();
    expect(
      w
        .find(".layout__sidebar .layout__search--sidebar input[type='search']")
        .exists(),
    ).toBe(true);
    expect(
      w
        .find(
          ".layout__sidebar .layout__controls--sidebar select.layout__locale",
        )
        .exists(),
    ).toBe(true);
  });

  it("shows a pending-verification notice only while the email is unconfirmed", () => {
    const auth = useAuthStore();
    auth.setToken("t");
    auth.account = {
      acct: "alice",
      yonder: { verification_pending: true },
    } as never;
    let w = layout();
    expect(w.get(".layout__notice").text()).toContain("isn't verified");
    auth.account = { acct: "alice" } as never;
    w = layout();
    expect(w.find(".layout__notice").exists()).toBe(false);
  });

  it("mobile menu closes on route change", async () => {
    const router = createRouter({
      history: createMemoryHistory(),
      routes: stubRoutes,
    });
    vi.spyOn(globalThis, "fetch").mockImplementation((input) =>
      Promise.resolve(
        new Response(String(input).includes("/conversations") ? "[]" : "{}", {
          status: 200,
        }),
      ),
    );
    const w = mount(AppLayout, {
      global: { plugins: [i18n, router] },
    });
    await w.get(".layout__burger").trigger("click");
    expect(
      document.querySelector("[data-testid='mobile-menu']"),
    ).not.toBeNull();
    await router.push("/local");
    await w.vm.$nextTick();
    expect(document.querySelector("[data-testid='mobile-menu']")).toBeNull();
  });

  async function layoutAt(path: string) {
    const auth = useAuthStore();
    auth.setToken("t");
    const router = createRouter({
      history: createMemoryHistory(),
      routes: stubRoutes,
    });
    await router.push(path);
    vi.spyOn(globalThis, "fetch").mockImplementation((input) =>
      Promise.resolve(
        new Response(String(input).includes("/conversations") ? "[]" : "{}", {
          status: 200,
        }),
      ),
    );
    const w = mount(AppLayout, {
      global: { plugins: [i18n, router] },
    });
    return { w, router };
  }

  it("compose on /messages becomes New message and seeds a direct draft", async () => {
    const { w, router } = await layoutAt("/messages");
    const btn = w.get("[data-testid='compose']");
    expect(btn.text()).toContain("New message");
    await btn.trigger("click");
    await flushPromises();
    const { useComposeStore } = await import("@/stores/compose");
    expect(useComposeStore().draft.visibility).toBe("direct");
    expect(router.currentRoute.value.path).toBe("/compose");
  });

  it("compose outside /messages stays a plain public draft", async () => {
    const { w, router } = await layoutAt("/local");
    const btn = w.get("[data-testid='compose']");
    expect(btn.text()).not.toContain("New message");
    await btn.trigger("click");
    await flushPromises();
    const { useComposeStore } = await import("@/stores/compose");
    expect(useComposeStore().draft.visibility).toBe("public");
    expect(router.currentRoute.value.path).toBe("/compose");
  });

  it("compose on /messages never clobbers an in-progress draft", async () => {
    const { useComposeStore } = await import("@/stores/compose");
    const compose = useComposeStore();
    compose.update({ text: "work in progress" });
    const { w, router } = await layoutAt("/messages");
    const btn = w.get("[data-testid='compose']");
    // The draft is resumed, not re-purposed — the label stays honest.
    expect(btn.text()).not.toContain("New message");
    await btn.trigger("click");
    await flushPromises();
    expect(compose.draft.text).toBe("work in progress");
    expect(compose.draft.visibility).toBe("public");
    expect(router.currentRoute.value.path).toBe("/compose");
  });
});

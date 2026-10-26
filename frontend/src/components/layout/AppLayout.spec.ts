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
            uri: "testville.example",
          }),
          { status: 200 },
        ),
      );
    if (url.includes("unread_count"))
      return Promise.resolve(
        new Response(JSON.stringify({ unread_count: 0 }), { status: 200 }),
      );
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
    expect(names).toContain("Direct messages");
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
    expect(aside.get("input[type='search']").exists()).toBe(true);
    expect(aside.get("select.layout__locale").exists()).toBe(true);
    expect(aside.text()).toContain("Yonder/9.9.9");
    expect(aside.text()).toContain("testville.example");
    const links = aside.findAll(".layout__meta-links a");
    expect(links.map((a) => a.text())).toEqual(["About", "Source code"]);
  });

  it("sidebar keeps a search fallback below the wide breakpoint", () => {
    const w = layout();
    expect(
      w
        .get(".layout__sidebar .layout__search--sidebar input[type='search']")
        .exists(),
    ).toBe(true);
    expect(
      w
        .get(
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
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(new Response("{}", { status: 200 })),
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

  it("direct item opens compose in direct mode and navigates", async () => {
    const auth = useAuthStore();
    auth.setToken("t");
    const router = createRouter({
      history: createMemoryHistory(),
      routes: stubRoutes,
    });
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(new Response("{}", { status: 200 })),
    );
    const w = mount(AppLayout, {
      global: { plugins: [i18n, router] },
    });
    const direct = w
      .findAll(".layout__sidebar .navmenu__button")
      .find((b) => b.text().includes("Direct"));
    expect(direct).toBeTruthy();
    await direct!.trigger("click");
    await flushPromises();
    const { useComposeStore } = await import("@/stores/compose");
    expect(useComposeStore().draft.visibility).toBe("direct");
    expect(router.currentRoute.value.path).toBe("/compose");
  });
});

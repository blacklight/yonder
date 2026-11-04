import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import BottomNav, { type BottomNavItem } from "./BottomNav.vue";

const items: BottomNavItem[] = [
  { id: "home", label: "Home", to: "/home", icon: "house" },
  { id: "search", label: "Search", to: "/search", icon: "magnifying-glass" },
  {
    id: "messages",
    label: "Messages",
    to: "/messages",
    icon: "comments",
    badge: true,
  },
  {
    id: "notifications",
    label: "Notifications",
    to: "/notifications",
    icon: "bell",
    badge: 4,
  },
  {
    id: "profile",
    label: "Profile",
    to: "/profile",
    avatar: true,
    avatarSrc: "https://example.com/a.png",
    active: true,
  },
];

describe("BottomNav", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("renders one link per item with accessible labels", () => {
    const { wrapper } = mountWith(BottomNav, { props: { items } });
    const links = wrapper.findAll<HTMLAnchorElement>(".bottomnav__link");
    expect(links).toHaveLength(5);
    expect(links.map((l) => l.attributes("href"))).toEqual([
      "/home",
      "/search",
      "/messages",
      "/notifications",
      "/profile",
    ]);
    expect(links.map((l) => l.attributes("aria-label"))).toEqual([
      "Home",
      "Search",
      "Messages",
      "Notifications",
      "Profile",
    ]);
  });

  it("shows the unread count and a bare dot for boolean badges", () => {
    const { wrapper } = mountWith(BottomNav, { props: { items } });
    const badges = wrapper.findAll(".bottomnav__badge");
    expect(badges).toHaveLength(2);
    expect(badges[0].classes()).toContain("bottomnav__badge--dot");
    expect(badges[0].text()).toBe("");
    expect(badges[1].text()).toBe("4");
    expect(badges[1].classes()).not.toContain("bottomnav__badge--dot");
  });

  it("renders the avatar for the profile shortcut", () => {
    const { wrapper } = mountWith(BottomNav, { props: { items } });
    const img = wrapper.get(".bottomnav__item:last-child .avatar__img");
    expect(img.attributes("src")).toBe("https://example.com/a.png");
  });

  it("applies the forced-active class when route matching can't see the target", () => {
    const { wrapper } = mountWith(BottomNav, { props: { items } });
    const profile = wrapper.get(".bottomnav__item:last-child .bottomnav__link");
    expect(profile.classes()).toContain("bottomnav__link--active");
  });

  it("re-tapping the current route's tab scrolls to top instead of navigating", async () => {
    const spy = vi.spyOn(window, "scrollTo").mockImplementation(() => {});
    const { wrapper, router } = mountWith(BottomNav, { props: { items } });
    await router.isReady();
    await router.push("/home");
    await wrapper.findAll(".bottomnav__link")[0].trigger("click");
    expect(spy).toHaveBeenCalledWith(expect.objectContaining({ top: 0 }));
    expect(router.currentRoute.value.path).toBe("/home");
  });

  it("a forced-active alias (home on `/`) scrolls to top too", async () => {
    const spy = vi.spyOn(window, "scrollTo").mockImplementation(() => {});
    const home: BottomNavItem[] = [
      { id: "home", label: "Home", to: "/home", icon: "house", active: true },
    ];
    const { wrapper, router } = mountWith(BottomNav, {
      props: { items: home },
    });
    await router.isReady();
    expect(router.currentRoute.value.path).toBe("/");
    await wrapper.get(".bottomnav__link").trigger("click");
    expect(spy).toHaveBeenCalled();
    expect(router.currentRoute.value.path).toBe("/");
  });

  it("tapping a different tab navigates without scrolling", async () => {
    const spy = vi.spyOn(window, "scrollTo").mockImplementation(() => {});
    const { wrapper, router } = mountWith(BottomNav, { props: { items } });
    await router.isReady();
    await router.push("/home");
    await wrapper.findAll(".bottomnav__link")[1].trigger("click");
    await flushPromises();
    expect(spy).not.toHaveBeenCalled();
    expect(router.currentRoute.value.path).toBe("/search");
  });
});

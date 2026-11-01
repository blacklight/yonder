import { describe, expect, it } from "vitest";
import { mountWith } from "@/test/render";
import NavMenu from "./NavMenu.vue";

describe("NavMenu", () => {
  it("renders the item icon before the label", () => {
    const { wrapper } = mountWith(NavMenu, {
      props: {
        items: [
          { id: "home", label: "Home", to: "/home", icon: "house" },
          { id: "plain", label: "Plain", to: "/local" },
        ],
      },
    });
    const links = wrapper.findAll(".navmenu__link");
    const icon = links[0].find("svg");
    expect(icon.exists()).toBe(true);
    // The `fa-<name>` class is what rtl.css's RTL mirroring keys on.
    expect(icon.classes()).toContain("fa-house");
    expect(icon.attributes("aria-hidden")).toBe("true");
    expect(links[1].find("svg").exists()).toBe(false);
  });

  it("renders `href` items as plain anchors for non-SPA pages", () => {
    const { wrapper } = mountWith(NavMenu, {
      props: {
        items: [
          { id: "home", label: "Home", to: "/home" },
          { id: "api", label: "API", href: "/swagger-ui/", icon: "code" },
        ],
      },
    });
    const links = wrapper.findAll(".navmenu__link");
    expect(links[1].element.tagName).toBe("A");
    // A full navigation, not a router push: the href is untouched.
    expect(links[1].attributes("href")).toBe("/swagger-ui/");
    expect(links[1].find("svg").classes()).toContain("fa-code");
  });
});

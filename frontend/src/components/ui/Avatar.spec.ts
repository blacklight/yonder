import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import Avatar from "./Avatar.vue";

describe("Avatar", () => {
  it("shows the first initial without an image", () => {
    const w = mount(Avatar, { props: { name: "alice" } });
    expect(w.text()).toBe("A");
    expect(w.attributes("aria-label")).toBe("alice");
  });
  it("renders an image when src is set", () => {
    const w = mount(Avatar, { props: { src: "/x.png", alt: "A" } });
    expect(w.get("img").attributes("src")).toBe("/x.png");
  });
});

import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import Skeleton from "./Skeleton.vue";

describe("Skeleton", () => {
  it("renders a decorative block hidden from assistive tech", () => {
    const w = mount(Skeleton);
    expect(w.attributes("aria-hidden")).toBe("true");
    expect(w.classes()).toContain("skeleton");
  });
  it("uses width for both dimensions when circle", () => {
    const w = mount(Skeleton, { props: { circle: true, width: "48px" } });
    expect(w.classes()).toContain("skeleton--circle");
    const style = w.attributes("style") ?? "";
    expect(style).toContain("width: 48px");
    expect(style).toContain("height: 48px");
  });
});

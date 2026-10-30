import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import AccountRowSkeleton from "./AccountRowSkeleton.vue";

describe("AccountRowSkeleton", () => {
  it("renders an avatar circle plus lines", () => {
    const w = mount(AccountRowSkeleton);
    expect(w.attributes("aria-hidden")).toBe("true");
    expect(w.findAll(".skeleton--circle")).toHaveLength(1);
  });
  it("can drop the avatar for text-only rows", () => {
    const w = mount(AccountRowSkeleton, { props: { avatar: false } });
    expect(w.findAll(".skeleton--circle")).toHaveLength(0);
    expect(w.findAll(".skeleton").length).toBeGreaterThan(0);
  });
});

import { describe, expect, it } from "vitest";
import { mountWith } from "@/test/render";
import Spinner from "./Spinner.vue";

describe("Spinner", () => {
  it("announces itself as a status with a sr-only label", () => {
    const { wrapper } = mountWith(Spinner);
    expect(wrapper.attributes("role")).toBe("status");
    expect(wrapper.find(".visually-hidden").text()).toBeTruthy();
  });
  it("applies the size class", () => {
    const { wrapper } = mountWith(Spinner, { props: { size: "sm" } });
    expect(wrapper.classes()).toContain("spinner--sm");
  });
});

import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import StatusCardSkeleton from "./StatusCardSkeleton.vue";

describe("StatusCardSkeleton", () => {
  it("renders a decorative card-shaped placeholder", () => {
    const w = mount(StatusCardSkeleton);
    expect(w.attributes("aria-hidden")).toBe("true");
    // avatar + 2 name lines + 3 body lines + 4 footer marks
    expect(w.findAll(".skeleton").length).toBe(10);
  });
});

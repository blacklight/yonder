import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import Card from "./Card.vue";

describe("Card", () => {
  it("renders header, body and footer slots", () => {
    const w = mount(Card, {
      slots: { header: "Head", default: "Body", footer: "Foot" },
    });
    expect(w.text()).toContain("Head");
    expect(w.text()).toContain("Body");
    expect(w.text()).toContain("Foot");
  });
});

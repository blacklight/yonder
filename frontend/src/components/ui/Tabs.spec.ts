import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import Tabs from "./Tabs.vue";

const items = [
  { key: "accounts", label: "Users" },
  { key: "statuses", label: "Posts" },
] as const;

describe("Tabs", () => {
  it("marks the active tab as pressed", () => {
    const w = mount(Tabs, { props: { items, modelValue: "statuses" } });
    const buttons = w.findAll("button");
    expect(buttons[0].attributes("aria-pressed")).toBe("false");
    expect(buttons[1].attributes("aria-pressed")).toBe("true");
  });
  it("emits the tab key on click", async () => {
    const w = mount(Tabs, { props: { items, modelValue: "accounts" } });
    await w.findAll("button")[1].trigger("click");
    expect(w.emitted("update:modelValue")?.[0]).toEqual(["statuses"]);
  });
  it("renders a nav label and slot actions", () => {
    const w = mount(Tabs, {
      props: { items, modelValue: "accounts", label: "sections" },
      slots: { default: '<button class="extra">Clear</button>' },
    });
    expect(w.get("nav").attributes("aria-label")).toBe("sections");
    expect(w.find(".extra").exists()).toBe(true);
  });
});

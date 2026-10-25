import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import Button from "./Button.vue";

describe("Button", () => {
  it("renders slot content and variant classes", () => {
    const w = mount(Button, {
      props: { variant: "danger" },
      slots: { default: "Delete" },
    });
    expect(w.text()).toBe("Delete");
    expect(w.classes()).toContain("btn--danger");
  });
  it("is disabled while loading and reports busy", () => {
    const w = mount(Button, { props: { loading: true } });
    expect(w.attributes("disabled")).toBeDefined();
    expect(w.attributes("aria-busy")).toBe("true");
  });
  it("emits a click when enabled", async () => {
    const w = mount(Button);
    await w.trigger("click");
    expect(w.emitted("click")).toBeTruthy();
  });
});

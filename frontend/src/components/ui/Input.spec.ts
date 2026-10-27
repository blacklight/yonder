import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import { i18n } from "@/i18n";
import Input from "./Input.vue";

function field(props: Record<string, unknown> = {}) {
  return mount(Input, {
    props: { label: "Name", ...props },
    global: { plugins: [i18n] },
  });
}

describe("Input", () => {
  it("associates the label with the control", () => {
    const w = field();
    const input = w.get("input");
    const label = w.get("label");
    expect(label.attributes("for")).toBe(input.attributes("id"));
  });
  it("emits updates and marks errors accessibly", async () => {
    const w = field({ label: "Email", error: "bad" });
    const input = w.get("input");
    expect(input.attributes("aria-invalid")).toBe("true");
    expect(input.attributes("aria-describedby")).toBeTruthy();
    expect(w.find("[role='alert']").text()).toBe("bad");
    await input.setValue("a@b.c");
    expect(w.emitted("update:modelValue")?.[0]).toEqual(["a@b.c"]);
  });
  it("clearable shows an X that empties the field and keeps focus", async () => {
    const w = mount(Input, {
      props: { label: "Search", clearable: true, modelValue: "" },
      global: { plugins: [i18n] },
      attachTo: document.body,
    });
    expect(w.find("[data-testid='input-clear']").exists()).toBe(false);
    await w.setProps({ modelValue: "cats" });
    const clear = w.get("[data-testid='input-clear']");
    expect(clear.attributes("aria-label")).toBeTruthy();
    await clear.trigger("click");
    expect(w.emitted("update:modelValue")?.at(-1)).toEqual([""]);
    expect(document.activeElement).toBe(w.get("input").element);
    await w.setProps({ modelValue: "" });
    expect(w.find("[data-testid='input-clear']").exists()).toBe(false);
  });
  it("clearable hides the X while disabled", async () => {
    const w = field({
      label: "Search",
      clearable: true,
      disabled: true,
      modelValue: "x",
    });
    expect(w.find("[data-testid='input-clear']").exists()).toBe(false);
  });
});

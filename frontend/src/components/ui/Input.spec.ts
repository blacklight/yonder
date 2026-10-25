import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import Input from "./Input.vue";

describe("Input", () => {
  it("associates the label with the control", () => {
    const w = mount(Input, { props: { label: "Name" } });
    const input = w.get("input");
    const label = w.get("label");
    expect(label.attributes("for")).toBe(input.attributes("id"));
  });
  it("emits updates and marks errors accessibly", async () => {
    const w = mount(Input, { props: { label: "Email", error: "bad" } });
    const input = w.get("input");
    expect(input.attributes("aria-invalid")).toBe("true");
    expect(input.attributes("aria-describedby")).toBeTruthy();
    expect(w.find("[role='alert']").text()).toBe("bad");
    await input.setValue("a@b.c");
    expect(w.emitted("update:modelValue")?.[0]).toEqual(["a@b.c"]);
  });
});

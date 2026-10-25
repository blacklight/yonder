import { describe, expect, it } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useThemeStore } from "@/stores/theme";
import SettingsPage from "./SettingsPage.vue";

describe("SettingsPage accent picker", () => {
  it("renders the preset swatches as a radiogroup", () => {
    const { wrapper } = mountWith(SettingsPage);
    const swatches = wrapper.findAll(".accent-picker__swatch");
    expect(swatches.length).toBe(8);
    expect(wrapper.find('[role="radiogroup"]').exists()).toBe(true);
  });

  it("applies a preset immediately on click and marks it active", async () => {
    const { wrapper } = mountWith(SettingsPage);
    const swatch = wrapper.findAll(".accent-picker__swatch")[1];
    await swatch.trigger("click");
    await flushPromises();
    const theme = useThemeStore();
    expect(theme.accent).toBe("#0ea5e9");
    expect(swatch.attributes("aria-checked")).toBe("true");
    expect(swatch.find("svg").exists()).toBe(true);
  });

  it("saves a custom hex from the textbox", async () => {
    const { wrapper } = mountWith(SettingsPage);
    await wrapper.get(".accent-picker__custom input").setValue("#ff0000");
    await wrapper.get(".accent-picker__custom button").trigger("click");
    await flushPromises();
    expect(useThemeStore().accent).toBe("#ff0000");
  });

  it("normalizes a hex without the leading #", async () => {
    const { wrapper } = mountWith(SettingsPage);
    await wrapper.get(".accent-picker__custom input").setValue("0ea5e9");
    await wrapper.get(".accent-picker__custom button").trigger("click");
    await flushPromises();
    expect(useThemeStore().accent).toBe("#0ea5e9");
  });

  it("rejects malformed input without changing the accent", async () => {
    const theme = useThemeStore();
    theme.setAccent("#14b8a6");
    const { wrapper } = mountWith(SettingsPage);
    await wrapper.get(".accent-picker__custom input").setValue("not-a-color");
    await wrapper.get(".accent-picker__custom button").trigger("click");
    await flushPromises();
    expect(theme.accent).toBe("#14b8a6");
    expect(wrapper.find(".field__error").exists()).toBe(true);
  });

  it("clears back to the instance default on an empty save", async () => {
    const theme = useThemeStore();
    theme.setAccent("#0ea5e9");
    const { wrapper } = mountWith(SettingsPage);
    await wrapper.get(".accent-picker__custom input").setValue("");
    await wrapper.get(".accent-picker__custom button").trigger("click");
    await flushPromises();
    expect(theme.accent).toBeNull();
    expect(wrapper.findAll(".accent-picker__swatch--active").length).toBe(0);
  });
});

import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import { useThemeStore } from "@/stores/theme";
import SettingsPage from "./SettingsPage.vue";

/** Answer the page's two mounts: `frontends` then the settings row. */
function mockApi(settings: Record<string, unknown>) {
  return vi.spyOn(globalThis, "fetch").mockImplementation((input) => {
    const url = String(input);
    const body = url.endsWith("/yonder/frontends")
      ? { frontends: [] }
      : settings;
    return Promise.resolve(new Response(JSON.stringify(body), { status: 200 }));
  });
}

describe("SettingsPage preference groups", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("hides account groups for anonymous visitors", () => {
    const { wrapper } = mountWith(SettingsPage);
    expect(wrapper.text()).not.toContain("Posting defaults");
  });

  it("renders and patches a boolean flag for signed-in users", async () => {
    useAuthStore().setToken("tok");
    const spy = mockApi({
      theme: "system",
      accent: null,
      cw_always_expand: false,
      email_notifications: {},
    });
    const { wrapper } = mountWith(SettingsPage);
    await flushPromises();

    // DOM order: default_sensitive, then cw_always_expand.
    const checkbox = wrapper.findAll('input[type="checkbox"]')[1];
    expect(wrapper.text()).toContain("Posting defaults");
    await checkbox.setValue(true);
    await flushPromises();

    const patchCall = spy.mock.calls.find(
      (c) => (c[1]?.method ?? "") === "PATCH",
    )!;
    expect(patchCall[0]).toBe("/api/v1/yonder/settings");
    expect(JSON.parse(patchCall[1]!.body as string)).toEqual({
      cw_always_expand: true,
    });
  });

  it("splits the language filter input into a tag array", async () => {
    useAuthStore().setToken("tok");
    const spy = mockApi({
      theme: "system",
      accent: null,
      filter_languages: ["en"],
      email_notifications: {},
    });
    const { wrapper } = mountWith(SettingsPage);
    await flushPromises();

    // First `.settings__stack` is the language-filter row (the second
    // holds the scheduled-deletion input).
    const stack = wrapper.findAll(".settings__stack")[0];
    const input = stack.get<HTMLInputElement>('input[type="text"]');
    expect(input.element.value).toBe("en");
    await input.setValue("de, ar es");
    await stack.get("button").trigger("click");
    await flushPromises();

    const patchCall = spy.mock.calls.find(
      (c) => (c[1]?.method ?? "") === "PATCH",
    )!;
    expect(JSON.parse(patchCall[1]!.body as string)).toEqual({
      filter_languages: ["de", "ar", "es"],
    });
  });
});

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

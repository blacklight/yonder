import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAdminSettingsStore } from "@/stores/adminSettings";
import SettingField from "./SettingField.vue";
import SettingsSection from "./SettingsSection.vue";
import type { AdminSetting } from "@/api/adminSettings";

function entry(over: Partial<AdminSetting> = {}): AdminSetting {
  return {
    toml_path: "instance.name",
    env: "YONDER_INSTANCE_NAME",
    source: "default",
    value: "Yonder",
    default: "Yonder",
    requires_restart: false,
    sensitive: false,
    writable: true,
    ...over,
  };
}

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    }),
  );
}

describe("SettingField", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAdminSettingsStore().entries = [];
  });

  it("saves a text value via PUT and upserts the returned entry", async () => {
    const updated = entry({ source: "overlay", value: "New" });
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json(updated));
    const { wrapper } = mountWith(SettingField, {
      props: { entry: entry() },
    });
    await wrapper.get(".setting__input").setValue("New");
    await wrapper.get("button.btn").trigger("click");
    await flushPromises();

    const [url, init] = fetchSpy.mock.calls[0] as [string, RequestInit];
    expect(url).toContain("/api/v1/yonder/admin/settings/instance.name");
    expect(init.method).toBe("PUT");
    expect(JSON.parse(init.body as string)).toEqual({ value: "New" });
    expect(wrapper.emitted("updated")?.[0]).toEqual([updated]);
    expect(useAdminSettingsStore().entries[0].source).toBe("overlay");
    expect(wrapper.find(".setting__saved").exists()).toBe(true);
  });

  it("auto-saves booleans on toggle", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json(entry({ value: false })));
    const { wrapper } = mountWith(SettingField, {
      props: {
        entry: entry({ toml_path: "instance.flag", value: true }),
      },
    });
    await wrapper.get('.setting__check input[type="checkbox"]').setValue(false);
    await flushPromises();

    const [url, init] = fetchSpy.mock.calls[0] as [string, RequestInit];
    expect(url).toContain("/instance.flag");
    expect(JSON.parse(init.body as string)).toEqual({ value: false });
  });

  it("sends DELETE on Reset, only shown for overlays", async () => {
    const overlaid = entry({ source: "overlay", value: "X" });
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json(entry()));
    const { wrapper } = mountWith(SettingField, {
      props: { entry: overlaid },
    });
    const reset = wrapper.findAll("button").find((b) => b.text() === "Reset");
    expect(reset).toBeDefined();
    await reset!.trigger("click");
    await flushPromises();

    const [url, init] = fetchSpy.mock.calls[0] as [string, RequestInit];
    expect(url).toContain("/instance.name");
    expect(init.method).toBe("DELETE");
  });

  it("renders enum-valued keys as a select and saves the choice", async () => {
    const updated = entry({
      toml_path: "email.smtp_tls",
      source: "overlay",
      value: "implicit",
    });
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json(updated));
    const { wrapper } = mountWith(SettingField, {
      props: {
        entry: entry({ toml_path: "email.smtp_tls", value: "starttls" }),
      },
    });

    const select = wrapper.get("select.setting__input");
    expect(select.findAll("option").map((o) => o.attributes("value"))).toEqual([
      "off",
      "starttls",
      "implicit",
    ]);
    expect((select.element as HTMLSelectElement).value).toBe("starttls");
    await select.setValue("implicit");
    await wrapper.get("button.btn").trigger("click");
    await flushPromises();

    const [, init] = fetchSpy.mock.calls[0] as [string, RequestInit];
    expect(JSON.parse(init.body as string)).toEqual({ value: "implicit" });
  });

  it("hides Reset when the value is not an overlay", () => {
    const { wrapper } = mountWith(SettingField, {
      props: { entry: entry() },
    });
    expect(wrapper.findAll("button").some((b) => b.text() === "Reset")).toBe(
      false,
    );
  });

  it("never displays sensitive values; save sends the typed value", async () => {
    const secret = entry({
      toml_path: "email.smtp_password",
      sensitive: true,
      set: true,
      value: null,
      default: null,
    });
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json(secret));
    const { wrapper } = mountWith(SettingField, {
      props: { entry: secret },
    });
    const input = wrapper.get(".setting__input");
    expect((input.element as HTMLInputElement).value).toBe("");
    expect(input.attributes("placeholder")).toContain("Set");
    await input.setValue("hunter2");
    await wrapper.get("button.btn").trigger("click");
    await flushPromises();
    const [, init] = fetchSpy.mock.calls[0] as [string, RequestInit];
    expect(JSON.parse(init.body as string)).toEqual({ value: "hunter2" });
  });

  it("rejects invalid JSON without a request", async () => {
    const fetchSpy = vi.spyOn(globalThis, "fetch");
    const { wrapper } = mountWith(SettingField, {
      props: {
        entry: entry({ toml_path: "instance.rules", value: ["a"] }),
      },
    });
    await wrapper.get(".setting__textarea").setValue("{broken");
    await wrapper.get("button.btn").trigger("click");
    await flushPromises();
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(wrapper.find(".setting__error").exists()).toBe(true);
  });

  it("renders read-only values without controls", () => {
    const { wrapper } = mountWith(SettingField, {
      props: { entry: entry({ writable: false, value: "10.0.0.1" }) },
    });
    expect(wrapper.find(".setting__input").exists()).toBe(false);
    expect(wrapper.find(".setting__value").text()).toBe("10.0.0.1");
  });

  it("marks requires_restart entries with a badge", () => {
    const { wrapper } = mountWith(SettingField, {
      props: { entry: entry({ requires_restart: true }) },
    });
    expect(wrapper.find(".setting__badge").exists()).toBe(true);
  });
});

describe("SettingsSection", () => {
  it("sorts entries by toml_path and renders the title", () => {
    const { wrapper } = mountWith(SettingsSection, {
      props: {
        sectionId: "instance",
        title: "Instance",
        entries: [
          entry({ toml_path: "instance.name" }),
          entry({ toml_path: "instance.description" }),
        ],
      },
    });
    expect(wrapper.find(".settings-group__title").text()).toBe("Instance");
    const keys = wrapper.findAll(".setting__key").map((k) => k.text());
    expect(keys).toEqual(["instance.description", "instance.name"]);
  });
});

import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { ref } from "vue";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import NotificationsSettingsPage from "./NotificationsSettingsPage.vue";

const pushState = ref("prompt");
const pushError = ref<string | null>(null);
const pushSupported = ref(true);
const pushSubscribed = ref(false);
const subscription = ref<{
  alerts: Record<string, boolean>;
  policy: string;
} | null>(null);

const load = vi.fn(async () => {});
const enable = vi.fn(async () => {
  pushSubscribed.value = true;
  pushState.value = "subscribed";
  subscription.value = {
    alerts: { mention: true, favourite: true },
    policy: "all",
  };
});
const disable = vi.fn(async () => {
  pushSubscribed.value = false;
  pushState.value = "prompt";
  subscription.value = null;
});
const update = vi.fn(async () => {});

vi.mock("@/composables/usePushNotifications", () => ({
  usePushNotifications: () => ({
    state: pushState,
    error: pushError,
    isSupported: pushSupported,
    isSubscribed: pushSubscribed,
    subscription,
    load,
    enable,
    disable,
    update,
  }),
}));

describe("NotificationsSettingsPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useAuthStore().setToken("tok");
    pushState.value = "prompt";
    pushError.value = null;
    pushSupported.value = true;
    pushSubscribed.value = false;
    subscription.value = null;
  });

  it("shows the enable button when push is available but off", async () => {
    const { wrapper } = mountWith(NotificationsSettingsPage);
    await flushPromises();
    expect(load).toHaveBeenCalled();
    expect(wrapper.text()).toContain("Push notifications are off");
    const button = wrapper.find(".notif-settings__row button");
    expect(button.text()).toContain("Enable");
    expect(button.attributes("disabled")).toBeUndefined();
  });

  it("enables push on click and then shows alert toggles", async () => {
    const { wrapper } = mountWith(NotificationsSettingsPage);
    await flushPromises();
    await wrapper.find(".notif-settings__row button").trigger("click");
    expect(enable).toHaveBeenCalled();
    await flushPromises();
    expect(wrapper.text()).toContain("Notify me about");
    expect(wrapper.find("select").exists()).toBe(true);
  });

  it("PATCHes alerts when a checkbox changes", async () => {
    pushSubscribed.value = true;
    pushState.value = "subscribed";
    subscription.value = {
      alerts: { mention: true, favourite: false },
      policy: "followed",
    };
    const { wrapper } = mountWith(NotificationsSettingsPage);
    await flushPromises();
    const mention = wrapper.findAll('input[type="checkbox"]')[0];
    await mention.setValue(false);
    expect(update).toHaveBeenCalledWith(
      expect.objectContaining({ mention: false }),
    );
  });

  it("PATCHes the policy select", async () => {
    pushSubscribed.value = true;
    pushState.value = "subscribed";
    subscription.value = { alerts: {}, policy: "all" };
    const { wrapper } = mountWith(NotificationsSettingsPage);
    await flushPromises();
    const select = wrapper.find("select");
    await select.setValue("follower");
    expect(update).toHaveBeenCalledWith(undefined, "follower");
  });

  it("disables the button when notifications are denied", async () => {
    pushState.value = "denied";
    const { wrapper } = mountWith(NotificationsSettingsPage);
    await flushPromises();
    expect(wrapper.text()).toContain("blocked");
    expect(
      wrapper.find(".notif-settings__row button").attributes("disabled"),
    ).toBeDefined();
  });

  it("hides the toggle entirely when push is unsupported", async () => {
    pushSupported.value = false;
    pushState.value = "unsupported";
    const { wrapper } = mountWith(NotificationsSettingsPage);
    await flushPromises();
    expect(wrapper.text()).toContain("not supported");
  });
});

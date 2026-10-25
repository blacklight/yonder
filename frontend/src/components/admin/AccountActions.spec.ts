import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import AccountActions from "./AccountActions.vue";
import type { AdminAccount } from "@/api/adminUsers";

function account(over: Partial<AdminAccount> = {}): AdminAccount {
  return {
    id: "7",
    username: "spammy",
    domain: null,
    role: "user",
    suspended: false,
    limited: false,
    disabled: false,
    force_sensitive: false,
    created_at: "2025-01-01T00:00:00Z",
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

function button(
  wrapper: ReturnType<typeof mountWith>["wrapper"],
  text: string,
) {
  return wrapper.findAll("button").find((b) => b.text() === text);
}

describe("AccountActions", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    const auth = useAuthStore();
    auth.setToken("t");
    auth.role = "admin";
  });

  it("shows state-appropriate actions", () => {
    const { wrapper } = mountWith(AccountActions, {
      props: { account: account() },
    });
    expect(button(wrapper, "Suspend account")).toBeDefined();
    expect(button(wrapper, "Unsuspend account")).toBeUndefined();
    expect(button(wrapper, "Limit account")).toBeDefined();
    expect(button(wrapper, "Make moderator")).toBeDefined();
    expect(button(wrapper, "Demote to user")).toBeUndefined();
  });

  it("offers reversals for the current state", () => {
    const { wrapper } = mountWith(AccountActions, {
      props: { account: account({ suspended: true, limited: true }) },
    });
    expect(button(wrapper, "Unsuspend account")).toBeDefined();
    expect(button(wrapper, "Unlimit account")).toBeDefined();
    expect(button(wrapper, "Suspend account")).toBeUndefined();
  });

  it("hides admin-only actions from moderators", () => {
    useAuthStore().role = "moderator";
    const { wrapper } = mountWith(AccountActions, {
      props: { account: account() },
    });
    expect(button(wrapper, "Delete account")).toBeUndefined();
    expect(button(wrapper, "Make admin")).toBeUndefined();
    expect(button(wrapper, "Suspend account")).toBeDefined();
  });

  it("confirms before suspending, then POSTs the action", async () => {
    const updated = account({ suspended: true });
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json(updated));
    const { wrapper } = mountWith(AccountActions, {
      props: { account: account(), reportId: 42 },
    });
    await button(wrapper, "Suspend account")!.trigger("click");
    await flushPromises();
    // The confirm dialog is teleported to <body>.
    const confirmBtn = document.body.querySelector(
      ".modal__footer .btn--danger",
    )!;
    (confirmBtn as HTMLElement).click();
    await flushPromises();

    const [url, init] = fetchSpy.mock.calls[0] as [string, RequestInit];
    expect(url).toContain("/api/v1/admin/accounts/7/action");
    expect(JSON.parse(init.body as string)).toEqual({
      type: "suspend",
      report_id: 42,
    });
    expect(wrapper.emitted("updated")?.[0]).toEqual([updated]);
  });

  it("runs non-dangerous actions without confirmation", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json(account({ limited: true })));
    const { wrapper } = mountWith(AccountActions, {
      props: { account: account() },
    });
    await button(wrapper, "Limit account")!.trigger("click");
    await flushPromises();
    const [url, init] = fetchSpy.mock.calls[0] as [string, RequestInit];
    expect(url).toContain("/admin/accounts/7/action");
    expect(JSON.parse(init.body as string)).toEqual({ type: "silence" });
  });

  it("reversals hit the verb routes", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json(account()));
    const { wrapper } = mountWith(AccountActions, {
      props: { account: account({ suspended: true }) },
    });
    await button(wrapper, "Unsuspend account")!.trigger("click");
    await flushPromises();
    const [url] = fetchSpy.mock.calls[0] as [string];
    expect(url).toContain("/admin/accounts/7/unsuspend");
  });

  it("shows the returned reset link", async () => {
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      json({ reset_url: "https://x/auth/password-reset/abc" }),
    );
    const { wrapper } = mountWith(AccountActions, {
      props: { account: account() },
    });
    await button(wrapper, "Password reset link")!.trigger("click");
    await flushPromises();
    expect(wrapper.find(".account-actions__link code").text()).toContain(
      "/auth/password-reset/abc",
    );
  });

  it("hides local-only hygiene actions for remote accounts", () => {
    const { wrapper } = mountWith(AccountActions, {
      props: { account: account({ domain: "remote.tld" }) },
    });
    expect(button(wrapper, "Disable 2FA")).toBeUndefined();
    expect(button(wrapper, "Mark email verified")).toBeUndefined();
  });
});

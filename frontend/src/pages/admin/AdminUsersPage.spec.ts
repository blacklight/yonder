import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import AdminUsersPage from "./AdminUsersPage.vue";
import type { AdminAccount } from "@/api/adminUsers";

function account(over: Partial<AdminAccount> = {}): AdminAccount {
  return {
    id: "7",
    username: "alice",
    domain: null,
    role: "user",
    email: "alice@x.test",
    email_verified: true,
    approved: true,
    suspended: false,
    limited: false,
    disabled: false,
    force_sensitive: false,
    created_at: "2025-01-01T00:00:00Z",
    ...over,
  };
}

function json(body: unknown, status = 200, headers: HeadersInit = {}) {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json", ...headers },
    }),
  );
}

describe("AdminUsersPage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    const auth = useAuthStore();
    auth.setToken("t");
    auth.role = "admin";
  });

  it("lists local accounts on load", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json([account()]));
    const { wrapper } = mountWith(AdminUsersPage);
    await flushPromises();
    const [url] = fetchSpy.mock.calls[0] as [string];
    expect(url).toContain("/api/v1/admin/accounts");
    expect(url).toContain("origin=local");
    expect(wrapper.text()).toContain("@alice");
    expect(wrapper.text()).toContain("alice@x.test");
  });

  it("applies search and status filters", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json([]));
    const { wrapper } = mountWith(AdminUsersPage);
    await flushPromises();
    await wrapper.find(".admin-users__filters input").setValue("alice@x");
    const statusSelect = wrapper.findAll(".admin-users__filters select").at(1)!;
    await statusSelect.setValue("suspended");
    await flushPromises();
    const [url] = fetchSpy.mock.calls.at(-1) as [string];
    expect(url).toContain("status=suspended");
    await wrapper.find(".admin-users__filters").trigger("submit");
    await flushPromises();
    const [url2] = fetchSpy.mock.calls.at(-1) as [string];
    expect(url2).toContain("search=alice%40x");
  });

  it("creates a user and reloads the list", async () => {
    const created = account({ id: "9", username: "newbie" });
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementationOnce(() => json([])) // initial list
      .mockImplementationOnce(() => json(created)) // POST
      .mockImplementationOnce(() => json([created])); // reload
    const { wrapper } = mountWith(AdminUsersPage);
    await flushPromises();

    const form = wrapper.find(".admin-users__create");
    const inputs = form.findAll("input");
    await inputs[0].setValue("newbie");
    await inputs[1].setValue("newbie@x.test");
    await inputs[2].setValue("hunter2-password");
    await form.trigger("submit");
    await flushPromises();

    const [url, init] = fetchSpy.mock.calls[1] as [string, RequestInit];
    expect(url).toContain("/api/v1/admin/accounts");
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body as string)).toEqual({
      username: "newbie",
      email: "newbie@x.test",
      password: "hunter2-password",
      role: "user",
      skip_verification: false,
    });
    expect(wrapper.text()).toContain("Created @newbie");
    expect(wrapper.text()).toContain("@newbie");
  });

  it("shows a create error", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementationOnce(() => json([]))
      .mockImplementationOnce(() =>
        json({ error: "username is already taken" }, 422),
      );
    const { wrapper } = mountWith(AdminUsersPage);
    await flushPromises();
    const form = wrapper.find(".admin-users__create");
    const inputs = form.findAll("input");
    await inputs[0].setValue("alice");
    await inputs[1].setValue("alice@x.test");
    await inputs[2].setValue("hunter2-password");
    await form.trigger("submit");
    await flushPromises();
    expect(fetchSpy).toHaveBeenCalledTimes(2);
    expect(wrapper.text()).toContain("username is already taken");
  });

  it("deletes an account after confirmation", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementationOnce(() => json([account()]))
      .mockImplementationOnce(() => json({ id: "7", deleted: true }));
    const { wrapper } = mountWith(AdminUsersPage);
    await flushPromises();

    const deleteBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === "Delete account")!;
    await deleteBtn.trigger("click");
    await flushPromises();
    // The confirm dialog is teleported to <body>.
    const confirmBtn = document.body.querySelector(
      ".modal__footer .btn--danger",
    )!;
    (confirmBtn as HTMLElement).click();
    await flushPromises();

    const [url, init] = fetchSpy.mock.calls[1] as [string, RequestInit];
    expect(url).toContain("/api/v1/admin/accounts/7");
    expect(init.method).toBe("DELETE");
    expect(wrapper.text()).not.toContain("@alice");
    expect(wrapper.text()).toContain("No accounts match");
  });
});

import { describe, expect, it, vi } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { useAuthStore } from "@/stores/auth";
import { mountWith } from "@/test/render";
import { ApiError } from "@/api/errors";
import RegisterPage from "./RegisterPage.vue";

const api = vi.hoisted(() => ({
  register: vi.fn(),
  getInstance: vi.fn(),
}));
vi.mock("@/api/accounts", async (importOriginal) => ({
  ...(await importOriginal<object>()),
  register: api.register,
}));
vi.mock("@/api/instance", () => ({
  getInstance: api.getInstance,
  composeLimits: () => ({}),
}));

async function fillForm(
  wrapper: ReturnType<typeof mountWith>["wrapper"],
  { agreement = true }: { agreement?: boolean } = {},
) {
  const inputs = wrapper.findAll(".register__form input");
  // Order: invite, username, email, password, confirm, [reason,] agreement.
  await inputs[1].setValue("alice");
  await inputs[2].setValue("alice@example.com");
  await inputs[3].setValue("secretpw1");
  await inputs[4].setValue("secretpw1");
  if (agreement)
    await wrapper.get('.register__form input[type="checkbox"]').setValue(true);
}

describe("RegisterPage", () => {
  it("registers, signs in with the returned token, and navigates to /", async () => {
    api.getInstance.mockResolvedValue({
      registrations: { enabled: true, approval_required: false },
    });
    api.register.mockResolvedValue({ access_token: "tok-new" });
    const auth = useAuthStore();
    const signIn = vi
      .spyOn(auth, "signInWithToken")
      .mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(RegisterPage);
    await router.push("/register");
    await flushPromises();
    await fillForm(wrapper);
    await wrapper.get("form.register__form").trigger("submit");
    await flushPromises();
    expect(api.register).toHaveBeenCalledWith(
      expect.objectContaining({
        username: "alice",
        email: "alice@example.com",
        agreement: true,
      }),
    );
    expect(signIn).toHaveBeenCalledWith("tok-new");
    expect(router.currentRoute.value.path).toBe("/");
  });

  it("requires the rules agreement before submitting", async () => {
    api.getInstance.mockResolvedValue({
      registrations: { enabled: true },
    });
    const { wrapper, router } = mountWith(RegisterPage);
    await router.push("/register");
    await flushPromises();
    await fillForm(wrapper, { agreement: false });
    await wrapper.get("form.register__form").trigger("submit");
    await flushPromises();
    expect(api.register).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("instance rules");
  });

  it("rejects mismatched passwords without calling the API", async () => {
    api.getInstance.mockResolvedValue({});
    const { wrapper, router } = mountWith(RegisterPage);
    await router.push("/register");
    await flushPromises();
    const inputs = wrapper.findAll(".register__form input");
    await inputs[1].setValue("alice");
    await inputs[2].setValue("a@b.c");
    await inputs[3].setValue("secretpw1");
    await inputs[4].setValue("different");
    await wrapper.get('.register__form input[type="checkbox"]').setValue(true);
    await wrapper.get("form.register__form").trigger("submit");
    await flushPromises();
    expect(api.register).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("do not match");
  });

  it("shows a reason field under approval-required registration", async () => {
    api.getInstance.mockResolvedValue({
      registrations: { enabled: true, approval_required: true },
    });
    api.register.mockResolvedValue({ access_token: "tok" });
    const auth = useAuthStore();
    vi.spyOn(auth, "signInWithToken").mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(RegisterPage);
    await router.push("/register");
    await flushPromises();
    // invite, username, email, password, confirm, reason, agreement
    const inputs = wrapper.findAll(".register__form input");
    await inputs[1].setValue("alice");
    await inputs[2].setValue("a@b.c");
    await inputs[3].setValue("secretpw1");
    await inputs[4].setValue("secretpw1");
    await inputs[5].setValue("I like cats");
    await wrapper.get('.register__form input[type="checkbox"]').setValue(true);
    await wrapper.get("form.register__form").trigger("submit");
    await flushPromises();
    expect(api.register).toHaveBeenCalledWith(
      expect.objectContaining({ reason: "I like cats" }),
    );
  });

  it("prefills the invite code from the query and sends it", async () => {
    api.getInstance.mockResolvedValue({
      registrations: { enabled: true },
    });
    api.register.mockResolvedValue({ access_token: "tok" });
    const auth = useAuthStore();
    vi.spyOn(auth, "signInWithToken").mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(RegisterPage);
    await router.push("/register?invite=ABC123");
    await flushPromises();
    await fillForm(wrapper);
    await wrapper.get("form.register__form").trigger("submit");
    await flushPromises();
    expect(api.register).toHaveBeenCalledWith(
      expect.objectContaining({ invite_code: "ABC123" }),
    );
  });

  it("hides the form when registrations are closed without an invite", async () => {
    api.getInstance.mockResolvedValue({
      registrations: { enabled: false },
    });
    const { wrapper, router } = mountWith(RegisterPage);
    await router.push("/register");
    await flushPromises();
    // Only the invite-code input is rendered.
    expect(wrapper.findAll(".register__form input")).toHaveLength(1);
    expect(wrapper.text()).toContain("closed");
  });

  it("surfaces API errors (e.g. username taken)", async () => {
    api.getInstance.mockResolvedValue({
      registrations: { enabled: true },
    });
    api.register.mockRejectedValue(
      new ApiError("username is already taken", 422),
    );
    const { wrapper, router } = mountWith(RegisterPage);
    await router.push("/register");
    await flushPromises();
    await fillForm(wrapper);
    await wrapper.get("form.register__form").trigger("submit");
    await vi.waitFor(() =>
      expect(wrapper.text()).toContain("username is already taken"),
    );
  });

  it("shows a pending-approval notice instead of signing in", async () => {
    api.getInstance.mockResolvedValue({
      registrations: { enabled: true, approval_required: true },
    });
    api.register.mockResolvedValue({ access_token: "tok-pending" });
    const auth = useAuthStore();
    const signIn = vi
      .spyOn(auth, "signInWithToken")
      .mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(RegisterPage);
    await router.push("/register");
    await flushPromises();
    await fillForm(wrapper);
    await wrapper.get("form.register__form").trigger("submit");
    await flushPromises();
    // Success screen, not an automatic sign-in.
    expect(wrapper.text()).toContain("waiting for an administrator");
    expect(signIn).not.toHaveBeenCalled();
    await wrapper.get(".register__form button").trigger("click");
    await flushPromises();
    expect(signIn).toHaveBeenCalledWith("tok-pending");
    expect(router.currentRoute.value.path).toBe("/");
  });

  it("shows the verify-email notice when the backend flags it", async () => {
    api.getInstance.mockResolvedValue({
      registrations: { enabled: true },
    });
    api.register.mockResolvedValue({
      access_token: "tok-v",
      verification_pending: true,
    });
    const auth = useAuthStore();
    const signIn = vi
      .spyOn(auth, "signInWithToken")
      .mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(RegisterPage);
    await router.push("/register");
    await flushPromises();
    await fillForm(wrapper);
    await wrapper.get("form.register__form").trigger("submit");
    await flushPromises();
    expect(wrapper.text()).toContain("Check your inbox");
    expect(signIn).not.toHaveBeenCalled();
    await wrapper.get(".register__form button").trigger("click");
    await flushPromises();
    expect(signIn).toHaveBeenCalledWith("tok-v");
    expect(router.currentRoute.value.path).toBe("/");
  });
});

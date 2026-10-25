import { describe, expect, it, vi } from "vitest";
import { flushPromises, type VueWrapper } from "@vue/test-utils";
import { useAuthStore } from "@/stores/auth";
import { mountWith } from "@/test/render";
import { ApiError } from "@/api/errors";
import SignInPage from "./SignInPage.vue";

const oauth = vi.hoisted(() => ({
  passwordToken: vi.fn(),
  twoFactorVerify: vi.fn(),
}));
vi.mock("@/api/oauth", async (importOriginal) => ({
  ...(await importOriginal<object>()),
  passwordToken: oauth.passwordToken,
  twoFactorVerify: oauth.twoFactorVerify,
}));

async function fillCredentials(wrapper: VueWrapper) {
  const inputs = wrapper.findAll(".signin input");
  await inputs[0].setValue("alice");
  await inputs[1].setValue("secret");
}

describe("SignInPage", () => {
  it("signs in with username and password and navigates to /", async () => {
    oauth.passwordToken.mockResolvedValue({ access_token: "tok-abc" });
    const auth = useAuthStore();
    const signIn = vi
      .spyOn(auth, "signInWithToken")
      .mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in");
    await fillCredentials(wrapper);
    await wrapper.get("form.signin__form").trigger("submit");
    await flushPromises();
    expect(oauth.passwordToken).toHaveBeenCalledWith({
      username: "alice",
      password: "secret",
    });
    expect(signIn).toHaveBeenCalledWith("tok-abc");
    expect(router.currentRoute.value.path).toBe("/");
  });

  it("honors the redirect query param", async () => {
    oauth.passwordToken.mockResolvedValue({ access_token: "tok" });
    const auth = useAuthStore();
    vi.spyOn(auth, "signInWithToken").mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in?redirect=/settings");
    await fillCredentials(wrapper);
    await wrapper.get("form.signin__form").trigger("submit");
    await flushPromises();
    expect(router.currentRoute.value.path).toBe("/settings");
  });

  it("rejects off-site redirect targets", async () => {
    oauth.passwordToken.mockResolvedValue({ access_token: "tok" });
    const auth = useAuthStore();
    vi.spyOn(auth, "signInWithToken").mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in?redirect=https%3A%2F%2Fevil.test%2Fx");
    await fillCredentials(wrapper);
    await wrapper.get("form.signin__form").trigger("submit");
    await flushPromises();
    expect(router.currentRoute.value.path).toBe("/");
  });

  it("shows the grant error on bad credentials", async () => {
    oauth.passwordToken.mockRejectedValue(
      new ApiError("The provided authorization grant is invalid.", 400),
    );
    const auth = useAuthStore();
    vi.spyOn(auth, "signInWithToken").mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in");
    await fillCredentials(wrapper);
    await wrapper.get("form.signin__form").trigger("submit");
    await vi.waitFor(() =>
      expect(wrapper.text()).toContain("authorization grant is invalid"),
    );
    expect(router.currentRoute.value.path).toBe("/sign-in");
  });

  it("requires credentials before submitting", async () => {
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in");
    await wrapper.get("form.signin__form").trigger("submit");
    expect(oauth.passwordToken).not.toHaveBeenCalled();
  });

  it("switches to the code form on mfa_required and finishes via 2fa verify", async () => {
    const { MfaRequired } = await import("@/api/oauth");
    oauth.passwordToken.mockRejectedValue(
      new MfaRequired("mfa-1", ["totp"], "Two-factor required."),
    );
    oauth.twoFactorVerify.mockResolvedValue({ access_token: "tok-2fa" });
    const auth = useAuthStore();
    const signIn = vi
      .spyOn(auth, "signInWithToken")
      .mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in");
    await fillCredentials(wrapper);
    await wrapper.get("form.signin__form").trigger("submit");
    await flushPromises();
    // MFA step replaces the credential fields.
    expect(wrapper.findAll(".signin input")).toHaveLength(1);
    await wrapper.get(".signin input").setValue("123456");
    await wrapper.get("form.signin__form").trigger("submit");
    await flushPromises();
    expect(oauth.twoFactorVerify).toHaveBeenCalledWith({
      mfaToken: "mfa-1",
      code: "123456",
    });
    expect(signIn).toHaveBeenCalledWith("tok-2fa");
    expect(router.currentRoute.value.path).toBe("/");
  });

  it("returns to credentials when the second factor fails", async () => {
    const { MfaRequired } = await import("@/api/oauth");
    oauth.passwordToken.mockRejectedValue(
      new MfaRequired("mfa-1", ["totp"], "Two-factor required."),
    );
    oauth.twoFactorVerify.mockRejectedValue(
      new ApiError("The provided two-factor credential is invalid.", 400),
    );
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in");
    await fillCredentials(wrapper);
    await wrapper.get("form.signin__form").trigger("submit");
    await flushPromises();
    await wrapper.get(".signin input").setValue("000000");
    await wrapper.get("form.signin__form").trigger("submit");
    await flushPromises();
    // Back on the credential form with the error surfaced.
    expect(wrapper.findAll(".signin input").length).toBeGreaterThanOrEqual(2);
    expect(wrapper.text()).toContain("two-factor credential is invalid");
  });

  it("does not burn a second-factor attempt on an empty code", async () => {
    const { MfaRequired } = await import("@/api/oauth");
    oauth.passwordToken.mockRejectedValue(
      new MfaRequired("mfa-1", ["totp"], "Two-factor required."),
    );
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in");
    await fillCredentials(wrapper);
    await wrapper.get("form.signin__form").trigger("submit");
    await flushPromises();
    // Submit with the code field left empty — the exchange must not fire.
    await wrapper.get("form.signin__form").trigger("submit");
    await flushPromises();
    expect(oauth.twoFactorVerify).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("Enter your two-factor code");
  });

  it("hides the code form for a webauthn-only account", async () => {
    const { MfaRequired } = await import("@/api/oauth");
    oauth.passwordToken.mockRejectedValue(
      new MfaRequired("mfa-1", ["webauthn"], "Two-factor required."),
    );
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in");
    await fillCredentials(wrapper);
    await wrapper.get("form.signin__form").trigger("submit");
    await flushPromises();
    expect(wrapper.findAll(".signin input")).toHaveLength(0);
    expect(wrapper.text()).toContain("passkey");
  });
});

import { describe, expect, it, vi } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { useAuthStore } from "@/stores/auth";
import { mountWith } from "@/test/render";
import SignInPage from "./SignInPage.vue";

describe("SignInPage", () => {
  it("signs in and navigates to /", async () => {
    const auth = useAuthStore();
    const signIn = vi
      .spyOn(auth, "signInWithToken")
      .mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in");
    await wrapper.get(".signin__form input").setValue("tok-abc");
    await wrapper.get(".signin__form").trigger("submit");
    await flushPromises();
    expect(signIn).toHaveBeenCalledWith("tok-abc");
    expect(router.currentRoute.value.path).toBe("/");
  });

  it("honors the redirect query param", async () => {
    const auth = useAuthStore();
    vi.spyOn(auth, "signInWithToken").mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in?redirect=/settings");
    await wrapper.get(".signin__form input").setValue("tok");
    await wrapper.get(".signin__form").trigger("submit");
    await flushPromises();
    expect(router.currentRoute.value.path).toBe("/settings");
  });

  it("rejects off-site redirect targets", async () => {
    const auth = useAuthStore();
    vi.spyOn(auth, "signInWithToken").mockResolvedValue(undefined);
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in?redirect=https%3A%2F%2Fevil.test%2Fx");
    await wrapper.get(".signin__form input").setValue("tok");
    await wrapper.get(".signin__form").trigger("submit");
    await flushPromises();
    expect(router.currentRoute.value.path).toBe("/");
  });

  it("shows the API error on failure", async () => {
    const auth = useAuthStore();
    const { ApiError } = await import("@/api/errors");
    vi.spyOn(auth, "signInWithToken").mockRejectedValue(
      new ApiError("invalid token", 401),
    );
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in");
    await wrapper.get(".signin__form input").setValue("bad");
    await wrapper.get(".signin__form").trigger("submit");
    await vi.waitFor(() => expect(wrapper.text()).toContain("invalid token"));
    expect(router.currentRoute.value.path).toBe("/sign-in");
  });

  it("requires a token before submitting", async () => {
    const auth = useAuthStore();
    const signIn = vi.spyOn(auth, "signInWithToken");
    const { wrapper, router } = mountWith(SignInPage);
    await router.push("/sign-in");
    await wrapper.get(".signin__form").trigger("submit");
    expect(signIn).not.toHaveBeenCalled();
  });
});

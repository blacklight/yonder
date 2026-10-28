import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { ApiError } from "@/api/errors";
import VerifyEmailPage from "./VerifyEmailPage.vue";

vi.mock("@/api/accounts", () => ({
  verifyEmail: vi.fn(),
}));
import { verifyEmail } from "@/api/accounts";

const mockedVerify = vi.mocked(verifyEmail);

async function mountPage(token = "abc123") {
  const mounted = mountWith(VerifyEmailPage);
  await mounted.router.push(`/verify-email/${token}`);
  await flushPromises();
  return mounted;
}

describe("VerifyEmailPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("posts the route token and shows the verified state", async () => {
    mockedVerify.mockResolvedValue({ verified: true });
    const { wrapper } = await mountPage("tok-xyz");

    expect(mockedVerify).toHaveBeenCalledWith("tok-xyz");
    expect(wrapper.text()).toContain("you're all set");
    expect(wrapper.find("button").text()).toBe("Sign in");
  });

  it("shows the failure state for an invalid token", async () => {
    mockedVerify.mockRejectedValue(
      new ApiError("invalid or expired token", 422),
    );
    const { wrapper } = await mountPage();

    expect(wrapper.find(".verify-email__error").exists()).toBe(true);
    expect(wrapper.text()).toContain("invalid or expired token");
  });

  it("shows a generic failure on a non-API error", async () => {
    mockedVerify.mockRejectedValue(new Error("network down"));
    const { wrapper } = await mountPage();

    expect(wrapper.text()).toContain(
      "This verification link is invalid or has expired.",
    );
  });
});

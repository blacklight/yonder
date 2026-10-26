import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import { ApiError } from "@/api/errors";
import OAuthAuthorizePage from "./OAuthAuthorizePage.vue";

const oauth = vi.hoisted(() => ({
  authorizeInfo: vi.fn(),
  authorizeApp: vi.fn(),
  navigateTo: vi.fn(),
}));
vi.mock("@/api/oauth", async (importOriginal) => ({
  ...(await importOriginal<object>()),
  authorizeInfo: oauth.authorizeInfo,
  authorizeApp: oauth.authorizeApp,
  navigateTo: oauth.navigateTo,
}));

const QUERY =
  "/oauth/authorize?client_id=cid-1" +
  "&redirect_uri=moshidon-android-auth%3A%2F%2Fcallback" +
  "&response_type=code&scope=read+write+push&state=xyz";

function button(
  wrapper: {
    findAll: (
      s: string,
    ) => { text: () => string; trigger: (e: string) => Promise<void> }[];
  },
  label: string,
) {
  const b = wrapper.findAll("button").find((x) => x.text().includes(label));
  if (!b)
    throw new Error(
      `no ${label} button: ${wrapper.findAll("button").map((x) => x.text())}`,
    );
  return b;
}

describe("OAuthAuthorizePage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    oauth.authorizeInfo.mockResolvedValue({
      client_name: "Moshidon",
      website: "github.com/LucasGGamerM/moshidon",
      scopes: ["read", "write", "push", "follow"],
      redirect_uri: "moshidon-android-auth://callback",
    });
  });

  it("shows the app name, website and requested scopes", async () => {
    const { wrapper, router } = mountWith(OAuthAuthorizePage);
    await router.push(QUERY);
    await flushPromises();
    expect(oauth.authorizeInfo).toHaveBeenCalledWith(
      expect.objectContaining({
        client_id: "cid-1",
        redirect_uri: "moshidon-android-auth://callback",
      }),
    );
    expect(wrapper.text()).toContain("Moshidon");
    expect(wrapper.text()).toContain("github.com/LucasGGamerM/moshidon");
    for (const s of ["read", "write", "push"])
      expect(wrapper.text()).toContain(s);
    // The app's website lacks a scheme — the link gains https://.
    expect(wrapper.get("a").attributes("href")).toBe(
      "https://github.com/LucasGGamerM/moshidon",
    );
  });

  it("authorize POSTs the grant and navigates to redirect_to", async () => {
    oauth.authorizeApp.mockResolvedValue({
      code: "code-1",
      state: "xyz",
      redirect_to: "moshidon-android-auth://callback?code=code-1&state=xyz",
    });
    const { wrapper, router } = mountWith(OAuthAuthorizePage);
    await router.push(QUERY);
    await flushPromises();
    await button(wrapper, "Authorize").trigger("click");
    await flushPromises();
    expect(oauth.authorizeApp).toHaveBeenCalledWith(
      expect.objectContaining({
        client_id: "cid-1",
        redirect_uri: "moshidon-android-auth://callback",
        response_type: "code",
        scope: "read write push",
        state: "xyz",
      }),
    );
    expect(oauth.navigateTo).toHaveBeenCalledWith(
      "moshidon-android-auth://callback?code=code-1&state=xyz",
    );
  });

  it("deny redirects the client with access_denied + state", async () => {
    const { wrapper, router } = mountWith(OAuthAuthorizePage);
    await router.push(QUERY);
    await flushPromises();
    await button(wrapper, "Deny").trigger("click");
    await flushPromises();
    const url = new URL(oauth.navigateTo.mock.calls[0][0] as string);
    expect(url.protocol).toBe("moshidon-android-auth:");
    expect(url.searchParams.get("error")).toBe("access_denied");
    expect(url.searchParams.get("state")).toBe("xyz");
    expect(oauth.authorizeApp).not.toHaveBeenCalled();
  });

  it("deny on an OOB redirect just confirms instead of navigating", async () => {
    const { wrapper, router } = mountWith(OAuthAuthorizePage);
    await router.push(
      "/oauth/authorize?client_id=cid-1&redirect_uri=" +
        encodeURIComponent("urn:ietf:wg:oauth:2.0:oob") +
        "&response_type=code",
    );
    await flushPromises();
    await button(wrapper, "Deny").trigger("click");
    await flushPromises();
    expect(oauth.navigateTo).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("close this window");
  });

  it("shows the signed-in account when the session is restored", async () => {
    const auth = useAuthStore();
    // `restore()` is mocked out in most specs; set the account the
    // backend would have returned — it drives the "signed in as" line.
    auth.account = { acct: "tester" } as never;
    const { wrapper, router } = mountWith(OAuthAuthorizePage);
    await router.push(QUERY);
    await flushPromises();
    expect(wrapper.text()).toContain("Signed in as @tester");
  });

  it("shows the unknown-app error when metadata 404s", async () => {
    oauth.authorizeInfo.mockRejectedValue(new ApiError("not found", 404));
    const { wrapper, router } = mountWith(OAuthAuthorizePage);
    await router.push(QUERY);
    await flushPromises();
    expect(wrapper.text()).toContain("Unknown application");
  });

  it("rejects an incomplete authorization link", async () => {
    const { wrapper, router } = mountWith(OAuthAuthorizePage);
    await router.push("/oauth/authorize?client_id=cid-1");
    await flushPromises();
    expect(oauth.authorizeInfo).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("incomplete");
  });
});

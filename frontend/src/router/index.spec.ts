import { describe, expect, it, vi, beforeEach } from "vitest";
import { router } from "./index";
import { useAuthStore } from "@/stores/auth";

// `auth.restore()` early-returns once `account` is set, so tests
// that preset the account never hit the network; the mock exists
// only as a safety net.
vi.mock("@/api/accounts", () => ({
  verifyCredentials: vi.fn(),
}));

function signInAs(role: "user" | "moderator" | "admin") {
  const auth = useAuthStore();
  auth.setToken("t");
  // A truthy account short-circuits `restore()` — no API call.
  auth.account = { id: "1", acct: "alice" } as never;
  auth.role = role;
}

describe("router role guards", () => {
  beforeEach(() => {
    useAuthStore().setToken(null);
  });

  it("redirects anonymous visitors from admin/moderation to sign-in", async () => {
    await router.push("/admin");
    expect(router.currentRoute.value.name).toBe("sign-in");
    // `/admin` redirects to `/admin/general` before the guard runs.
    expect(router.currentRoute.value.query.redirect).toBe("/admin/general");

    await router.push("/moderation");
    expect(router.currentRoute.value.name).toBe("sign-in");
    expect(router.currentRoute.value.query.redirect).toBe("/moderation");
  });

  it("sends regular users home from both panels", async () => {
    signInAs("user");
    await router.push("/admin");
    expect(router.currentRoute.value.name).toBe("home");
    await router.push("/moderation");
    expect(router.currentRoute.value.name).toBe("home");
  });

  it("admits moderators to /moderation but not /admin", async () => {
    signInAs("moderator");
    await router.push("/moderation");
    expect(router.currentRoute.value.name).toBe("moderation");
    await router.push("/admin");
    expect(router.currentRoute.value.name).toBe("home");
  });

  it("admits admins to both panels", async () => {
    signInAs("admin");
    await router.push("/admin");
    expect(router.currentRoute.value.name).toBe("admin-general");
    await router.push("/moderation");
    expect(router.currentRoute.value.name).toBe("moderation");
  });

  it("leaves public and plain-auth routes untouched", async () => {
    await router.push("/search");
    expect(router.currentRoute.value.name).toBe("search");
    signInAs("user");
    await router.push("/settings");
    expect(router.currentRoute.value.name).toBe("settings");
  });
});

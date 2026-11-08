import { describe, expect, it, vi, beforeEach } from "vitest";
import { router } from "./index";
import { useAuthStore } from "@/stores/auth";
import { getInstance } from "@/api/instance";

// `auth.restore()` early-returns once `account` is set, so tests
// that preset the account never hit the network; the mock exists
// only as a safety net.
vi.mock("@/api/accounts", () => ({
  verifyCredentials: vi.fn(),
}));
vi.mock("@/api/instance", () => ({
  getInstance: vi.fn(),
}));
const mockedGetInstance = vi.mocked(getInstance);

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

  it("fences a restricted session to /restricted", async () => {
    const auth = useAuthStore();
    auth.signInRestricted("t");
    await router.push("/settings");
    expect(router.currentRoute.value.name).toBe("restricted");
    await router.push("/home");
    expect(router.currentRoute.value.name).toBe("restricted");
  });

  it("sends a normal session away from /restricted", async () => {
    signInAs("user");
    // Move off /restricted first — a duplicate navigation would be
    // a no-op and never reach the guard.
    await router.push("/settings");
    await router.push("/restricted");
    expect(router.currentRoute.value.name).toBe("settings");
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

describe("single-user redirect", () => {
  beforeEach(() => {
    useAuthStore().setToken(null);
    mockedGetInstance.mockReset();
  });

  it("sends anonymous visitors to the configured profile", async () => {
    mockedGetInstance.mockResolvedValue({
      yonder: { single_user: "alice" },
    } as never);
    await router.push("/about");
    await router.push("/");
    expect(router.currentRoute.value.name).toBe("profile-acct");
    expect(router.currentRoute.value.path).toBe("/@alice");
  });

  it("keeps the public feed when no single user is configured", async () => {
    mockedGetInstance.mockResolvedValue({
      yonder: { single_user: null },
    } as never);
    await router.push("/about");
    await router.push("/");
    expect(router.currentRoute.value.name).toBe("home");
  });

  it("does not redirect a signed-in session", async () => {
    signInAs("user");
    mockedGetInstance.mockResolvedValue({
      yonder: { single_user: "alice" },
    } as never);
    await router.push("/about");
    await router.push("/");
    expect(router.currentRoute.value.name).toBe("home");
  });
});

describe("feed document route", () => {
  // `*.atom`/`*.rss` URLs resolve to the `feed` route, whose guard
  // bounces to the backend document (`location.assign` is not
  // exercised here — jsdom cannot navigate).
  it("resolves feed paths to the feed route", () => {
    for (const path of [
      "/users/alice.atom",
      "/users/alice.rss",
      "/tags/rust.atom",
      "/lists/1.rss",
    ]) {
      expect(router.resolve(path).name).toBe("feed");
      expect(router.resolve(path).name).not.toBe("not-found");
    }
  });

  it("does not shadow page routes", () => {
    expect(router.resolve("/tags/rust").name).toBe("tag");
    expect(router.resolve("/lists/1").name).toBe("list");
    expect(router.resolve("/@alice").name).toBe("profile-acct");
    expect(router.resolve("/about").name).toBe("about");
  });
});

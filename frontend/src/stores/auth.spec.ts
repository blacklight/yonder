import { describe, expect, it, vi, beforeEach } from "vitest";
import { useAuthStore } from "./auth";

vi.mock("@/api/accounts", () => ({
  verifyCredentials: vi.fn(),
}));

import { verifyCredentials } from "@/api/accounts";

const mockedVerify = vi.mocked(verifyCredentials);

function account(role: string | undefined) {
  return {
    id: "1",
    username: "alice",
    acct: "alice",
    yonder: role === undefined ? {} : { role },
  };
}

describe("auth store", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockedVerify.mockResolvedValue(account("user") as never);
    // settings.fetch()/syncThemeFromServer hit the real client.
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(
        new Response(JSON.stringify({ theme: "system" }), { status: 200 }),
      ),
    );
  });

  it("signInWithToken stores token, account and role", async () => {
    const auth = useAuthStore();
    await auth.signInWithToken("tok");
    expect(auth.token).toBe("tok");
    expect(localStorage.getItem("yonder.token")).toBe("tok");
    expect(auth.account?.username).toBe("alice");
    expect(auth.role).toBe("user");
    expect(auth.isLoggedIn).toBe(true);
    expect(auth.isStaff).toBe(false);
    expect(auth.isAdmin).toBe(false);
  });

  it("exposes staff/admin getters from yonder.role", async () => {
    const auth = useAuthStore();
    mockedVerify.mockResolvedValue(account("moderator") as never);
    await auth.signInWithToken("tok");
    expect(auth.isStaff).toBe(true);
    expect(auth.isAdmin).toBe(false);
    auth.signOut();

    mockedVerify.mockResolvedValue(account("admin") as never);
    await auth.signInWithToken("tok2");
    expect(auth.isStaff).toBe(true);
    expect(auth.isAdmin).toBe(true);
  });

  it("falls back to the top-level role object", async () => {
    mockedVerify.mockResolvedValue({
      ...account(undefined),
      role: { id: "admin", name: "admin", permissions: "", color: "" },
    } as never);
    const auth = useAuthStore();
    await auth.signInWithToken("tok");
    expect(auth.role).toBe("admin");
  });

  it("clears the token and rethrows when verify fails", async () => {
    mockedVerify.mockRejectedValue(new Error("nope"));
    const auth = useAuthStore();
    await expect(auth.signInWithToken("bad")).rejects.toThrow("nope");
    expect(auth.token).toBeNull();
    expect(localStorage.getItem("yonder.token")).toBeNull();
  });

  it("rolls back the token when settings cannot be loaded", async () => {
    // verify_credentials is mocked above; settings.fetch hits the real
    // client — make it fail.
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(
        new Response(JSON.stringify({ error: "down" }), { status: 500 }),
      ),
    );
    const auth = useAuthStore();
    await expect(auth.signInWithToken("tok")).rejects.toBeTruthy();
    expect(auth.token).toBeNull();
    expect(localStorage.getItem("yonder.token")).toBeNull();
  });

  it("restore re-fetches account for a persisted token", async () => {
    localStorage.setItem("yonder.token", "saved");
    const auth = useAuthStore();
    auth.setToken("saved");
    await auth.restore();
    expect(auth.account?.username).toBe("alice");
  });

  it("restore is a no-op without a token or with an account", async () => {
    const auth = useAuthStore();
    await auth.restore();
    expect(mockedVerify).not.toHaveBeenCalled();
    auth.setToken("tok");
    auth.account = { id: "1" } as never;
    await auth.restore();
    expect(mockedVerify).not.toHaveBeenCalled();
  });

  it("signOut clears token, account and role", async () => {
    const auth = useAuthStore();
    await auth.signInWithToken("tok");
    auth.signOut();
    expect(auth.token).toBeNull();
    expect(auth.account).toBeNull();
    expect(auth.role).toBeNull();
    expect(auth.isLoggedIn).toBe(false);
  });
});

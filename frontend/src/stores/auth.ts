import { defineStore } from "pinia";
import type { CredentialAccount } from "@/api/entities";

const TOKEN_KEY = "yonder.token";

export type Role = "user" | "moderator" | "admin";

// Token-entry auth (epic 24) — the full OAuth/login flow replaces
// `signInWithToken` in epic 25 without touching consumers.
// Persisted to localStorage so a reload keeps the session; the
// account/role are re-fetched from `verify_credentials` on boot
// (`restore`), never persisted, so a role change can't go stale.
export const useAuthStore = defineStore("auth", {
  state: () => ({
    token: localStorage.getItem(TOKEN_KEY) as string | null,
    account: null as CredentialAccount | null,
    role: null as Role | null,
  }),
  getters: {
    isLoggedIn: (s) => s.token !== null,
    isStaff: (s) => s.role === "moderator" || s.role === "admin",
    isAdmin: (s) => s.role === "admin",
  },
  actions: {
    setToken(token: string | null) {
      this.token = token;
      if (token) localStorage.setItem(TOKEN_KEY, token);
      else localStorage.removeItem(TOKEN_KEY);
      if (!token) {
        this.account = null;
        this.role = null;
      }
    },

    /**
     * Dev login: store the token, verify it against
     * `verify_credentials`, then pull the user's settings so the
     * theme and compose defaults sync before the UI mounts.
     * Throws the `ApiError` on failure (caller shows the message).
     */
    async signInWithToken(token: string) {
      // Lazy imports: api/accounts → client → this store is a cycle.
      const { verifyCredentials } = await import("@/api/accounts");
      const { useSettingsStore } = await import("./settings");
      this.setToken(token);
      let account;
      try {
        account = await verifyCredentials();
      } catch (e) {
        // A failed verify never keeps the token (401 already cleared
        // it inside the client; other failures clear it here so a
        // retry starts clean).
        this.setToken(null);
        throw e;
      }
      this.account = account;
      const role = account.yonder?.role ?? account.role?.name;
      this.role = role === "moderator" || role === "admin" ? role : "user";
      const settings = useSettingsStore();
      try {
        await settings.fetch();
        await settings.syncThemeFromServer();
      } catch (e) {
        // Settings failures roll back — a half-signed-in session
        // (token persisted, defaults never loaded) is worse than a
        // clean retry.
        this.setToken(null);
        throw e;
      }
    },

    /** Re-fetch the account/role for a persisted token (app boot). */
    async restore() {
      if (!this.token || this.account) return;
      const { verifyCredentials } = await import("@/api/accounts");
      try {
        const account = await verifyCredentials();
        this.account = account;
        const role = account.yonder?.role ?? account.role?.name;
        this.role = role === "moderator" || role === "admin" ? role : "user";
      } catch {
        // A 401 already cleared the token inside the client; other
        // failures (offline, 5xx) keep the session so transient
        // outages don't sign the user out — the UI just stays
        // anonymous-looking until the next restore.
      }
    },

    signOut() {
      this.setToken(null);
    },
  },
});

import { defineStore } from "pinia";
import type { CredentialAccount } from "@/api/entities";
import { storageGet, storageRemove, storageSet } from "@/utils/storage";

const TOKEN_KEY = "yonder.token";
const RESTRICTED_KEY = "yonder.restricted";

export type Role = "user" | "moderator" | "admin";

// Token-entry auth (epic 24) — the full OAuth/login flow replaces
// `signInWithToken` in epic 25 without touching consumers.
// Persisted to localStorage so a reload keeps the session; the
// account/role are re-fetched from `verify_credentials` on boot
// (`restore`), never persisted, so a role change can't go stale.
export const useAuthStore = defineStore("auth", {
  state: () => ({
    token: storageGet(TOKEN_KEY) as string | null,
    account: null as CredentialAccount | null,
    role: null as Role | null,
    // Suspended-account export-only credential (epic 31 §5): the token
    // works on `/yonder/exports*` + `/yonder/restricted/*` only, so the
    // session is flagged at issue time and persisted to keep reloads
    // inside the restricted page.
    restricted: storageGet(RESTRICTED_KEY) === "1",
  }),
  getters: {
    isLoggedIn: (s) => s.token !== null,
    isStaff: (s) => s.role === "moderator" || s.role === "admin",
    isAdmin: (s) => s.role === "admin",
    // A full session — everything a restricted credential forbids.
    isFull: (s) => s.token !== null && !s.restricted,
  },
  actions: {
    setToken(token: string | null) {
      this.token = token;
      if (token) storageSet(TOKEN_KEY, token);
      else storageRemove(TOKEN_KEY);
      if (!token) {
        this.account = null;
        this.role = null;
        this.restricted = false;
        storageRemove(RESTRICTED_KEY);
      }
    },

    /**
     * Suspended-account login: the token response carried
     * `yonder.restricted`, so `verify_credentials` would 403 — mark
     * the restricted class and skip the settings/bootstrap fetches
     * the ordinary session performs.
     */
    signInRestricted(token: string) {
      this.setToken(token);
      this.restricted = true;
      storageSet(RESTRICTED_KEY, "1");
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
      // A normal grant replaces any restricted class left over — the
      // flag must never describe a full session.
      this.restricted = false;
      storageRemove(RESTRICTED_KEY);
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
      if (!this.token || this.restricted) return;
      const { verifyCredentials } = await import("@/api/accounts");
      const { useSettingsStore } = await import("./settings");
      const settings = useSettingsStore();
      try {
        if (!this.account) {
          const account = await verifyCredentials();
          this.account = account;
          const role = account.yonder?.role ?? account.role?.name;
          this.role = role === "moderator" || role === "admin" ? role : "user";
        }
        // Settings carry the compose defaults (content type,
        // visibility, language, sensitive) — a restored session that
        // skips this composes with factory defaults until the
        // settings page happens to be visited. Retried while null so
        // a transient failure at boot doesn't stick for the session.
        if (!settings.settings) await settings.syncThemeFromServer();
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

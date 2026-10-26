import { defineStore } from "pinia";
import { request } from "@/api/client";
import { useAuthStore } from "./auth";
import { resetOnUserChange } from "./perUser";
import { useThemeStore, type ThemeMode } from "./theme";

interface YonderSettings {
  interface_language: string | null;
  theme: ThemeMode;
  accent: string | null;
  frontend: string | null;
  default_visibility?: string | null;
  default_posting_language?: string | null;
  default_sensitive?: boolean;
  cw_always_expand?: boolean;
  show_app_display?: boolean;
  filter_languages?: string[];
  block_stranger_notifications?: boolean;
  email_notifications?: Record<string, boolean>;
  directory_opt_in?: boolean;
  include_public_posts_in_search?: boolean;
  scheduled_deletion_days?: number;
  [key: string]: unknown;
}

// Syncs server-persisted preferences with local stores. Server wins:
// `theme` is always present ("system" when unset); a null `accent`
// means the instance default and clears any local accent.
export const useSettingsStore = defineStore("settings", {
  state: () => {
    const s = { settings: null as YonderSettings | null };
    // Per-user data — drop the previous account's settings on sign-out.
    // The callback must write through the reactive store (`s` is the
    // raw state object; mutating it does not trigger reactivity).
    resetOnUserChange(() => {
      useSettingsStore().settings = null;
    });
    return s;
  },
  actions: {
    async fetch() {
      this.settings = await request<YonderSettings>({
        path: "/yonder/settings",
      });
      return this.settings;
    },
    async patch(body: Record<string, unknown>) {
      this.settings = await request<YonderSettings>({
        path: "/yonder/settings",
        method: "PATCH",
        body,
      });
      return this.settings;
    },
    // Pull the server copy into the local theme store — called after
    // login/session restore (epic 25 wires the login flow).
    async syncThemeFromServer() {
      const auth = useAuthStore();
      if (!auth.isLoggedIn) return;
      const theme = useThemeStore();
      const s = await this.fetch();
      theme.setMode(s.theme ?? "system");
      theme.setAccent(s.accent ?? null);
    },
    async setTheme(mode: ThemeMode, accent: string | null) {
      const theme = useThemeStore();
      theme.setMode(mode);
      theme.setAccent(accent);
      if (useAuthStore().isLoggedIn) {
        await this.patch({ theme: mode, accent });
      }
    },
  },
});

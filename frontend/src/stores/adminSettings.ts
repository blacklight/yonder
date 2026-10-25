import { defineStore } from "pinia";
import {
  listAdminSettings,
  resetAdminSetting,
  setAdminSetting,
  type AdminSetting,
} from "@/api/adminSettings";
import { resetOnUserChange } from "./perUser";

/**
 * Caches the admin settings registry. Each entry is replaced with the
 * fresh description returned by set/reset, so `source`, `value`, and
 * `requires_restart` stay accurate without a full refetch.
 */
export const useAdminSettingsStore = defineStore("adminSettings", {
  state: () => {
    const s = { entries: [] as AdminSetting[] };
    // Admin data must not leak across accounts.
    resetOnUserChange(() => {
      useAdminSettingsStore().entries = [];
    });
    return s;
  },
  getters: {
    bySection: (s) => (section: string) =>
      s.entries.filter((e) => e.toml_path.split(".")[0] === section),
    byPath: (s) => (path: string) =>
      s.entries.find((e) => e.toml_path === path),
  },
  actions: {
    async fetch() {
      this.entries = await listAdminSettings();
    },
    /** Returns the updated entry (also upserted into `entries`). */
    async set(key: string, value: unknown) {
      return this.upsert(await setAdminSetting(key, value));
    },
    async reset(key: string) {
      return this.upsert(await resetAdminSetting(key));
    },
    upsert(entry: AdminSetting) {
      const i = this.entries.findIndex((e) => e.toml_path === entry.toml_path);
      if (i >= 0) this.entries[i] = entry;
      else this.entries.push(entry);
      return entry;
    },
  },
});

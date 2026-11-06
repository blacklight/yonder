import { computed } from "vue";
import { useSettingsStore } from "@/stores/settings";
import { useThemeStore } from "@/stores/theme";

export function useTheme() {
  const store = useThemeStore();
  const settings = useSettingsStore();

  function toggle() {
    const next = store.resolvedMode === "dark" ? "light" : "dark";
    // Route through the settings store so the choice is PATCHed to
    // `/yonder/settings` — a localStorage-only flip is reverted by
    // `syncThemeFromServer` on the next session restore.
    void settings.setTheme(next, store.accent);
  }

  return {
    mode: computed(() => store.mode),
    accent: computed(() => store.accent),
    resolvedMode: computed(() => store.resolvedMode),
    setMode: store.setMode,
    setAccent: store.setAccent,
    toggle,
    apply: store.apply,
  };
}

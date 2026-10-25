import { computed } from "vue";
import { useThemeStore } from "@/stores/theme";

export function useTheme() {
  const store = useThemeStore();

  function toggle() {
    store.setMode(store.resolvedMode === "dark" ? "light" : "dark");
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

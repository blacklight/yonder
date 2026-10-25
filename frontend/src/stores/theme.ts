import { defineStore } from "pinia";

export type ThemeMode = "light" | "dark" | "system";

const MODE_KEY = "yonder.theme";
const ACCENT_KEY = "yonder.accent";

function systemDark(): boolean {
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

// sRGB relative luminance — picks a readable foreground for any
// arbitrary accent the user supplies.
function accentForeground(hex: string): string {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex);
  if (!m) return "#ffffff";
  const n = parseInt(m[1], 16);
  const r = ((n >> 16) & 255) / 255;
  const g = ((n >> 8) & 255) / 255;
  const b = (n & 255) / 255;
  const lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  return lum > 0.45 ? "#1a1a1a" : "#ffffff";
}

export const useThemeStore = defineStore("theme", {
  state: () => ({
    mode: (localStorage.getItem(MODE_KEY) as ThemeMode | null) ?? "system",
    accent: localStorage.getItem(ACCENT_KEY),
  }),
  getters: {
    resolvedMode(state): "light" | "dark" {
      return state.mode === "system"
        ? systemDark()
          ? "dark"
          : "light"
        : state.mode;
    },
  },
  actions: {
    setMode(mode: ThemeMode) {
      this.mode = mode;
      localStorage.setItem(MODE_KEY, mode);
      this.apply();
    },
    setAccent(accent: string | null) {
      this.accent = accent;
      if (accent) localStorage.setItem(ACCENT_KEY, accent);
      else localStorage.removeItem(ACCENT_KEY);
      this.apply();
    },
    // Applies the resolved mode to <html data-theme> and the accent
    // trio. `data-theme` is always "light" or "dark" — "system" is a
    // preference, not a theme.
    apply() {
      const resolved = this.resolvedMode;
      document.documentElement.dataset.theme = resolved;
      const meta = document.querySelector<HTMLMetaElement>(
        'meta[name="theme-color"]:not([media])',
      );
      const light = getComputedStyle(document.documentElement);
      const color = light.getPropertyValue("--bg").trim();
      if (meta && color) meta.content = color;
      const root = document.documentElement.style;
      if (this.accent) {
        root.setProperty("--accent", this.accent);
        root.setProperty("--accent-fg", accentForeground(this.accent));
        root.setProperty("--accent-soft", `${this.accent}22`);
      } else {
        root.removeProperty("--accent");
        root.removeProperty("--accent-fg");
        root.removeProperty("--accent-soft");
      }
    },
  },
});

import { defineStore } from "pinia";

export type ThemeMode = "light" | "dark" | "system";

const MODE_KEY = "yonder.theme";
const ACCENT_KEY = "yonder.accent";

function systemDark(): boolean {
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

type Rgb = [number, number, number];

function parseColor(raw: string): Rgb | null {
  const hex = /^#?([0-9a-f]{6})$/i.exec(raw.trim());
  if (hex) {
    const n = parseInt(hex[1], 16);
    return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
  }
  const rgb = /^rgba?\(\s*(\d{1,3})[\s,]+(\d{1,3})[\s,]+(\d{1,3})/i.exec(
    raw.trim(),
  );
  if (rgb) return [Number(rgb[1]), Number(rgb[2]), Number(rgb[3])];
  return null;
}

function toHex([r, g, b]: Rgb): string {
  return `#${[r, g, b]
    .map((v) =>
      Math.round(Math.min(255, Math.max(0, v)))
        .toString(16)
        .padStart(2, "0"),
    )
    .join("")}`;
}

function linearize(v: number): number {
  const c = v / 255;
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
}

// WCAG relative luminance — the sRGB channels must be linearized
// first. Summing the raw values hugely overestimates mid-dark colors
// (#3f8f7f scored ~0.49 and got near-black text).
function luminance([r, g, b]: Rgb): number {
  return 0.2126 * linearize(r) + 0.7152 * linearize(g) + 0.0722 * linearize(b);
}

function contrastRatio(a: number, b: number): number {
  return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
}

// Foreground for text/icons on an accent fill. The 0.375 threshold
// keeps dark ink for genuinely light accents only — the dark-theme
// default #2dd4bf (lum≈0.51) gets dark ink, while the light-theme
// default #24a8a0 (≈0.31) and brand teal #14b8a6 (≈0.37) get white.
export function accentForeground(hex: string): string {
  const rgb = parseColor(hex);
  return rgb && luminance(rgb) > 0.375 ? "#1a1a1a" : "#ffffff";
}

// --accent-link is the accent shifted until it reaches 4.5:1 against
// --bg, so links and accent-colored text stay readable even when the
// accent itself is too light (bright accent on a light theme) or too
// dark (deep accent on a dark theme) to render as text. Mixing
// channel-wise toward black/white keeps the hue. The theme defaults
// are derived the same way and hardcoded in tokens.css.
const LINK_MIN_CONTRAST = 4.5;

export function accentLink(accent: string, bg: string): string {
  const a = parseColor(accent);
  const b = parseColor(bg);
  if (!a || !b) return accent;
  const bgLum = luminance(b);
  if (contrastRatio(luminance(a), bgLum) >= LINK_MIN_CONTRAST) {
    return toHex(a);
  }
  // Mix toward black when the accent is darker than the bg, toward
  // white when lighter — unless that side cannot reach 4.5:1 even at
  // the extreme (no white is bright enough against a light bg).
  const canDarken = (bgLum + 0.05) / LINK_MIN_CONTRAST - 0.05 >= 0;
  const canLighten = (bgLum + 0.05) * LINK_MIN_CONTRAST - 0.05 <= 1;
  const darken = luminance(a) <= bgLum ? canDarken : !canLighten;
  const target: Rgb = darken ? [0, 0, 0] : [255, 255, 255];
  // Channel interpolation toward black/white is monotonic in
  // luminance, so bisection finds the mix closest to the accent.
  let lo = 0;
  let hi = 1;
  let best = target;
  for (let i = 0; i < 16; i++) {
    const t = (lo + hi) / 2;
    const mixed = a.map((c, j) => Math.round(c + (target[j] - c) * t)) as Rgb;
    if (contrastRatio(luminance(mixed), bgLum) >= LINK_MIN_CONTRAST) {
      best = mixed;
      hi = t;
    } else {
      lo = t;
    }
  }
  return toHex(best);
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
      const root = document.documentElement.style;
      // --accent must settle before the computed read below so a
      // just-cleared override doesn't leak into the derived trio.
      if (this.accent) root.setProperty("--accent", this.accent);
      else root.removeProperty("--accent");
      const light = getComputedStyle(document.documentElement);
      const color = light.getPropertyValue("--bg").trim();
      if (meta && color) meta.content = color;
      // The trio follows the *effective* accent — a stored override
      // or the per-theme default — so --accent-fg is right on first
      // paint too, not only after picking a swatch.
      const accent = this.accent ?? light.getPropertyValue("--accent").trim();
      const rgb = parseColor(accent);
      if (rgb || this.accent) {
        const hex = rgb ? toHex(rgb) : accent;
        root.setProperty("--accent-fg", accentForeground(accent));
        root.setProperty("--accent-soft", `${hex}22`);
        root.setProperty("--accent-link", accentLink(accent, color));
      } else {
        root.removeProperty("--accent-fg");
        root.removeProperty("--accent-soft");
        root.removeProperty("--accent-link");
      }
    },
  },
});

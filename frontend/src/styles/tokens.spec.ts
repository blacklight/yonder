import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { accentForeground, accentLink } from "@/stores/theme";

const css = readFileSync(
  resolve(process.cwd(), "src/styles/tokens.css"),
  "utf-8",
);

function hexLuminance(hex: string): number {
  const n = parseInt(hex.replace("#", ""), 16);
  const channel = (v: number) => {
    const c = v / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  return (
    0.2126 * channel((n >> 16) & 255) +
    0.7152 * channel((n >> 8) & 255) +
    0.0722 * channel(n & 255)
  );
}

// WCAG contrast ratio between two #rrggbb colors.
function contrast(a: string, b: string): number {
  const la = hexLuminance(a);
  const lb = hexLuminance(b);
  const [hi, lo] = la > lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

function token(name: string, theme: "light" | "dark"): string {
  const block =
    theme === "dark"
      ? css.match(/\[data-theme="dark"\]\s*{([^}]*)}/s)?.[1]
      : css.match(/:root\s*{([^}]*)}/s)?.[1];
  const value = block?.match(new RegExp(`--${name}:\\s*([^;]+)`))?.[1]?.trim();
  if (!value) throw new Error(`--${name} not defined for ${theme}`);
  return value;
}

describe("design tokens", () => {
  it.each(["light", "dark"] as const)(
    "keeps WCAG AA contrast for text on %s surfaces",
    (theme) => {
      expect(
        contrast(token("text", theme), token("bg", theme)),
      ).toBeGreaterThanOrEqual(4.5);
      expect(
        contrast(token("text-muted", theme), token("bg", theme)),
      ).toBeGreaterThanOrEqual(4.5);
      expect(
        contrast(token("text", theme), token("bg-raised", theme)),
      ).toBeGreaterThanOrEqual(4.5);
    },
  );

  it("defines the full token surface in both themes", () => {
    for (const name of [
      "bg",
      "bg-raised",
      "bg-sunken",
      "text",
      "text-muted",
      "border",
      "accent",
      "accent-fg",
      "accent-link",
      "success",
      "warning",
      "danger",
      "info",
    ]) {
      token(name, "light");
      token(name, "dark");
    }
  });

  it.each(["light", "dark"] as const)(
    "keeps WCAG AA contrast for --accent-link on %s bg",
    (theme) => {
      expect(
        contrast(token("accent-link", theme), token("bg", theme)),
      ).toBeGreaterThanOrEqual(4.5);
      expect(
        contrast(token("accent-link", theme), token("bg-raised", theme)),
      ).toBeGreaterThanOrEqual(4.5);
    },
  );

  it.each(["light", "dark"] as const)(
    "keeps the derived accent trio in sync with theme.ts on %s",
    (theme) => {
      // apply() re-derives the trio at runtime — the CSS values only
      // guard first paint, but drift shows up as a text-color flip on
      // load. --accent-fg/#ffffff on the light accent is ~2.9:1
      // (below AA) — the ink pair accentForeground() picks, not a
      // contrast guarantee.
      const accent = token("accent", theme);
      expect(token("accent-fg", theme)).toBe(accentForeground(accent));
      expect(token("accent-soft", theme)).toBe(`${accent}22`);
      expect(token("accent-link", theme)).toBe(
        accentLink(accent, token("bg", theme)),
      );
    },
  );

  it("ships focus ring, motion and reduced-motion tokens", () => {
    expect(css).toContain("--focus-ring");
    expect(css).toContain("--duration-fast");
    expect(css).toContain("prefers-reduced-motion");
  });
});

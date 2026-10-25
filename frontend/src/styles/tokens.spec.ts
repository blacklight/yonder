import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const css = readFileSync(
  resolve(process.cwd(), "src/styles/tokens.css"),
  "utf-8",
);

function hexLuminance(hex: string): number {
  const n = parseInt(hex.replace("#", ""), 16);
  const r = ((n >> 16) & 255) / 255;
  const g = ((n >> 8) & 255) / 255;
  const b = (n & 255) / 255;
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
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
      "success",
      "warning",
      "danger",
      "info",
    ]) {
      token(name, "light");
      token(name, "dark");
    }
  });

  it("ships focus ring, motion and reduced-motion tokens", () => {
    expect(css).toContain("--focus-ring");
    expect(css).toContain("--duration-fast");
    expect(css).toContain("prefers-reduced-motion");
  });
});

import { beforeEach, describe, expect, it, vi } from "vitest";
import { useThemeStore } from "./theme";

function mockSystemDark(matches: boolean) {
  (window.matchMedia as ReturnType<typeof vi.fn>).mockImplementation(
    (query: string) => ({
      matches,
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    }),
  );
}

describe("theme store", () => {
  beforeEach(() => {
    document.documentElement.removeAttribute("data-theme");
    mockSystemDark(false);
  });

  it("defaults to system and resolves via prefers-color-scheme", () => {
    const store = useThemeStore();
    expect(store.mode).toBe("system");
    // Pinia getters memoize on reactive deps — the OS preference is
    // not one, so mock before reading rather than mid-test.
    mockSystemDark(true);
    expect(store.resolvedMode).toBe("dark");
    mockSystemDark(false);
    store.setMode("light");
    store.setMode("system");
    expect(store.resolvedMode).toBe("light");
  });

  it("persists mode and applies the resolved theme attribute", () => {
    const store = useThemeStore();
    store.setMode("dark");
    expect(localStorage.getItem("yonder.theme")).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");
    store.setMode("system");
    expect(document.documentElement.dataset.theme).toBe("light");
  });

  it("applies and clears the accent vars", () => {
    const store = useThemeStore();
    store.setAccent("#ffffff");
    const style = document.documentElement.style;
    expect(style.getPropertyValue("--accent")).toBe("#ffffff");
    expect(style.getPropertyValue("--accent-fg")).toBe("#1a1a1a");
    store.setAccent(null);
    expect(style.getPropertyValue("--accent")).toBe("");
    expect(style.getPropertyValue("--accent-link")).toBe("");
    expect(localStorage.getItem("yonder.accent")).toBeNull();
  });

  it.each([
    // Mid-dark accents get light text — #3f8f7f previously scored
    // "light" under raw sRGB summation.
    ["#3f8f7f", "#ffffff"],
    ["#1565c0", "#ffffff"],
    ["#7c3aed", "#ffffff"],
    ["#808080", "#ffffff"],
    // Genuinely light accents get dark ink — #14b8a6 and #2dd4bf are
    // the light/dark theme defaults and must keep dark-text pairing.
    ["#14b8a6", "#1a1a1a"],
    ["#2dd4bf", "#1a1a1a"],
    ["#ffc107", "#1a1a1a"],
    ["#ffffff", "#1a1a1a"],
  ])("picks readable text for accent %s", (hex, expected) => {
    const store = useThemeStore();
    store.setAccent(hex);
    expect(document.documentElement.style.getPropertyValue("--accent-fg")).toBe(
      expected,
    );
  });

  it("derives --accent-link toward readable contrast against --bg", () => {
    document.documentElement.style.setProperty("--bg", "#f9f8f7");
    const store = useThemeStore();
    const style = document.documentElement.style;
    // Bright accent on a light bg gets darkened for text use.
    store.setAccent("#14b8a6");
    const link = style.getPropertyValue("--accent-link");
    expect(link).toMatch(/^#[0-9a-f]{6}$/);
    expect(link).not.toBe("#14b8a6");
    // An accent that already contrasts well is kept as-is.
    store.setAccent("#0e8074");
    expect(style.getPropertyValue("--accent-link")).toBe("#0e8074");
    document.documentElement.style.removeProperty("--bg");
  });

  it("falls back to light text for malformed accents", () => {
    const store = useThemeStore();
    store.setAccent("not-a-color");
    expect(document.documentElement.style.getPropertyValue("--accent-fg")).toBe(
      "#ffffff",
    );
  });
});

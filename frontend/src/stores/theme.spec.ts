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

  it("applies and clears the accent trio", () => {
    const store = useThemeStore();
    store.setAccent("#ffffff");
    const style = document.documentElement.style;
    expect(style.getPropertyValue("--accent")).toBe("#ffffff");
    expect(style.getPropertyValue("--accent-fg")).toBe("#1a1a1a");
    store.setAccent(null);
    expect(style.getPropertyValue("--accent")).toBe("");
    expect(localStorage.getItem("yonder.accent")).toBeNull();
  });
});

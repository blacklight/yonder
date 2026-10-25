import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

// Manifest is configured in vite.config.ts — parse it back out so a
// regression there fails here instead of at release time.
const config = readFileSync(resolve(process.cwd(), "vite.config.ts"), "utf-8");

describe("PWA manifest configuration", () => {
  it("declares name, start_url and standalone display", () => {
    for (const needle of [
      'name: "Yonder"',
      'start_url: "/"',
      'display: "standalone"',
      "theme_color",
      "background_color",
    ]) {
      expect(config).toContain(needle);
    }
  });
  it("ships any and maskable icons", () => {
    expect(config).toContain('purpose: "any"');
    expect(config).toContain('purpose: "maskable"');
    expect(config).toContain("maskable-512.png");
  });
  it("excludes API and federation paths from the SPA fallback", () => {
    expect(config).toContain("navigateFallbackDenylist");
    expect(config).toContain("/^\\/api/");
  });
});

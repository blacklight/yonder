import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

// Manifest is configured in vite.config.ts — parse it back out so a
// regression there fails here instead of at release time.
const config = readFileSync(resolve(process.cwd(), "vite.config.ts"), "utf-8");
// The navigation denylist + runtime caching live in the custom worker
// (injectManifest), not the vite config.
const sw = readFileSync(resolve(process.cwd(), "src/sw.ts"), "utf-8");

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
    expect(sw).toContain("NAV_DENYLIST");
    expect(sw).toContain("/^\\/api/");
  });
  it("excludes feed documents from the SPA fallback", () => {
    // `*.atom`/`*.rss` navigations must reach the backend — serving
    // index.html for them would show the SPA's not-found page.
    expect(sw).toContain("/^\\/(users|tags|lists)\\/.*\\.(atom|rss)$/");
  });
  it("ships a custom worker with push handlers", () => {
    // injectManifest + the notification listeners are what turn
    // backend WebPush deliveries into native notifications.
    expect(config).toContain('strategies: "injectManifest"');
    expect(sw).toContain('addEventListener("push"');
    expect(sw).toContain('addEventListener("notificationclick"');
  });
});

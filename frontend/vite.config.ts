import { defineConfig, loadEnv } from "vite";
import vue from "@vitejs/plugin-vue";
import rtlcss from "postcss-rtlcss";
import { VitePWA } from "vite-plugin-pwa";
import { resolve } from "node:path";

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, import.meta.dirname, "VITE_");

  const allowedHosts = env.VITE_ALLOWED_HOSTS
    ? env.VITE_ALLOWED_HOSTS.split(",")
        .map((h) => h.trim())
        .filter(Boolean)
    : [];

  // Dev proxy target. Defaults to the backend's stock `server.port`;
  // override via VITE_BACKEND_URL (shell or a gitignored .env.local)
  // when the backend binds a non-loopback host or a different port.
  const backend = env.VITE_BACKEND_URL || "http://localhost:3000";
  const to = { target: backend, changeOrigin: true };
  // Proxies only `*.{atom,rss}` documents to the backend — bare page
  // paths under the same prefix bypass so vite serves the SPA.
  const feedOnly = {
    ...to,
    bypass(req: { url?: string }) {
      if (!/\.(atom|rss)(\?|$)/.test(req.url ?? "")) return req.url;
    },
  };

  return {
    plugins: [
      vue(),
      VitePWA({
        // Custom SW source — the generateSW workbox config lives in
        // `src/sw.ts` alongside the WebPush `push`/`notificationclick`
        // handlers a generated worker can't carry.
        strategies: "injectManifest",
        srcDir: "src",
        filename: "sw.ts",
        registerType: "autoUpdate",
        devOptions: { enabled: false },
        manifest: {
          id: "/",
          name: "Yonder",
          short_name: "Yonder",
          description: "A Mastodon-compatible ActivityPub server",
          start_url: "/",
          display: "standalone",
          background_color: "#f9f8f7",
          theme_color: "#f9f8f7",
          icons: [
            {
              src: "/icons/logo-512.png",
              sizes: "512x512",
              type: "image/png",
              purpose: "any",
            },
            {
              src: "/icons/maskable-192.png",
              sizes: "192x192",
              type: "image/png",
              purpose: "maskable",
            },
            {
              src: "/icons/maskable-512.png",
              sizes: "512x512",
              type: "image/png",
              purpose: "maskable",
            },
          ],
        },
        injectManifest: {
          // The sw.ts build itself ships workbox imports — let Rollup
          // resolve them into the worker bundle instead of importScripts.
          rollupFormat: "iife",
        },
        includeAssets: ["favicon.ico", "favicon.svg", "icons/*.png"],
      }),
    ],
    // Mirrors physical directional CSS (left/right, text-align, floats,
    // ...) into [dir=ltr]/[dir=rtl] variants so RTL locales flip.
    css: {
      postcss: {
        plugins: [rtlcss()],
      },
    },
    resolve: {
      alias: {
        "@": resolve(import.meta.dirname, "src"),
      },
    },
    server: {
      host: true,
      allowedHosts,
      proxy: {
        // `ws: true` on /api also proxies the /api/v1/streaming
        // websocket endpoint.
        "/api": { ...to, ws: true },
        "/media": to,
        // `/oauth/*` lives outside /api (token + 2FA exchange endpoints).
        "/oauth": to,
        // Feed documents (`{name}.{atom,rss}`) live on the backend
        // under the same prefixes as SPA pages — only the
        // extension-bearing paths are proxied; bare paths bypass so
        // the history fallback still serves the app.
        "/users": feedOnly,
        "/tags": feedOnly,
        "/lists": feedOnly,
        // Instance-discovery documents the backend owns; the SPA router
        // claims everything else, so /ap and other page prefixes are
        // intentionally NOT proxied (they would shadow SPA routes).
        "/.well-known": to,
        "/nodeinfo": to,
        "/openapi.json": to,
        "/swagger-ui": to,
      },
    },
    build: {
      outDir: "dist",
    },
  };
});

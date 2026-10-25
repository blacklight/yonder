import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import rtlcss from "postcss-rtlcss";
import { VitePWA } from "vite-plugin-pwa";
import { resolve } from "node:path";

const allowedHosts = process.env.VITE_ALLOWED_HOSTS
  ? process.env.VITE_ALLOWED_HOSTS.split(",")
      .map((h) => h.trim())
      .filter(Boolean)
  : [];

export default defineConfig({
  plugins: [
    vue(),
    VitePWA({
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
      workbox: {
        navigateFallback: "/index.html",
        // API and streaming endpoints must never be answered from
        // the cache — they are live federation/notification traffic.
        navigateFallbackDenylist: [
          /^\/api/,
          /^\/\.well-known/,
          /^\/users\/.*\.(atom|rss)$/,
          /^\/media\//,
          /^\/oauth/,
          /^\/swagger-ui/,
          /^\/openapi\.json$/,
        ],
        runtimeCaching: [
          {
            urlPattern: ({ request }) => request.destination === "image",
            handler: "CacheFirst",
            options: {
              cacheName: "yonder-images",
              expiration: { maxEntries: 256, maxAgeSeconds: 7 * 24 * 3600 },
            },
          },
        ],
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
      // Yonder's default server.port is 3000. `ws: true` on /api also
      // proxies the /api/v1/streaming websocket endpoint.
      "/api": { target: "http://localhost:3000", changeOrigin: true, ws: true },
      "/media": { target: "http://localhost:3000", changeOrigin: true },
      // `/oauth/*` lives outside /api (token + 2FA exchange endpoints).
      "/oauth": { target: "http://localhost:3000", changeOrigin: true },
      "/openapi.json": { target: "http://localhost:3000", changeOrigin: true },
      "/swagger-ui": { target: "http://localhost:3000", changeOrigin: true },
    },
  },
  build: {
    outDir: "dist",
  },
});

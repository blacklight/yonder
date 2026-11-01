// Custom service worker — built by vite-plugin-pwa `injectManifest`.
// The Workbox sections mirror the old `generateSW` config (precache +
// SPA navigation fallback + image cache); the `push` /
// `notificationclick` handlers below are the reason this file exists:
// they turn backend WebPush deliveries into native notifications.
import { clientsClaim } from "workbox-core";
import { ExpirationPlugin } from "workbox-expiration";
import {
  cleanupOutdatedCaches,
  createHandlerBoundToURL,
  precacheAndRoute,
} from "workbox-precaching";
import { NavigationRoute, registerRoute } from "workbox-routing";
import { CacheFirst } from "workbox-strategies";

declare let self: ServiceWorkerGlobalScope & {
  __WB_MANIFEST: Array<{ url: string; revision: string | null } | string>;
};

precacheAndRoute(self.__WB_MANIFEST);
cleanupOutdatedCaches();
clientsClaim();

// `registerSW({ immediate: true })` (autoUpdate) posts this message
// once a new worker is installed — honor it so updates activate
// without a tab reload.
self.addEventListener("message", (event) => {
  if (event.data?.type === "SKIP_WAITING") void self.skipWaiting();
});

// Live traffic must never hit the SPA fallback — the same denylist
// the previous generateSW config carried.
const NAV_DENYLIST = [
  /^\/api/,
  /^\/\.well-known/,
  /^\/(users|tags|lists)\/.*\.(atom|rss)$/,
  /^\/media\//,
  /^\/oauth/,
  /^\/swagger-ui/,
  /^\/openapi\.json$/,
];

registerRoute(
  new NavigationRoute(createHandlerBoundToURL("/index.html"), {
    denylist: NAV_DENYLIST,
  }),
);

registerRoute(
  ({ request }) => request.destination === "image",
  new CacheFirst({
    cacheName: "yonder-images",
    plugins: [
      new ExpirationPlugin({ maxEntries: 256, maxAgeSeconds: 7 * 24 * 3600 }),
    ],
  }),
);

// ---------------------------------------------------------------------------
// Web Push
// ---------------------------------------------------------------------------

/** The JSON the backend's `WebPushSend` job encrypts into the push. */
interface PushPayload {
  notification_id?: string;
  notification_type?: string;
  title?: string;
  body?: string;
  icon?: string;
  badge?: string;
  tag?: string;
  data?: { url?: string };
}

self.addEventListener("push", (event) => {
  let payload: PushPayload = {};
  try {
    payload = (event.data?.json() ?? {}) as PushPayload;
  } catch {
    // Unparseable pushes fall back to the default title/body below.
  }
  event.waitUntil(
    self.registration.showNotification(payload.title || "Yonder", {
      body: payload.body || "",
      icon: payload.icon || "/icons/logo-512.png",
      badge: payload.badge || "/icons/maskable-192.png",
      tag: payload.tag || "yonder-notification",
      data: { url: payload.data?.url || "/notifications" },
    }),
  );
});

/** Local paths only — a push payload must never navigate off-origin. */
function safeLocalUrl(input: string | undefined): string {
  try {
    const url = new URL(input || "/", self.location.origin);
    if (url.origin !== self.location.origin) return "/";
    if (url.protocol !== "http:" && url.protocol !== "https:") return "/";
    return url.pathname + url.search + url.hash;
  } catch {
    return "/";
  }
}

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const target = safeLocalUrl(
    (event.notification.data as { url?: string } | undefined)?.url,
  );
  event.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then(async (clientList) => {
        // Reuse an open app window when one exists — focus it and
        // navigate to the notification's target rather than stacking
        // tabs per click.
        const existing = clientList.find((c) =>
          c.url.startsWith(self.location.origin),
        );
        if (existing) {
          await existing.focus();
          return existing.navigate(target);
        }
        if (self.clients.openWindow) return self.clients.openWindow(target);
      }),
  );
});

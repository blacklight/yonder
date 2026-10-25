// Registers the service worker in production builds and guards the
// frontend-switch edge case: when an admin swaps `/` for another
// frontend, the built-in yonder worker must not keep serving its
// cached shell. On boot we compare the served document's
// `x-yonder-frontend` header with this bundle's identity; on a
// mismatch every registration is unregistered, the caches are wiped
// and the page reloads once into the new frontend's document.
const FRONTEND_NAME = "yonder";

// The probe URL must NOT be precached: `/index.html` lives in the
// workbox precache manifest, so a fetch for it is answered by the
// service worker itself and the `x-yonder-frontend` header is never
// seen. Any unmatched path falls through to the server, which stamps
// the header on every frontend response.
const PROBE_URL = "/frontend-check";

export function usePwa() {
  // A failed probe (offline, server restarting) must never break boot
  // or surface as an unhandled rejection — the guard is best-effort.
  async function guardFrontendSwitch() {
    if (!("serviceWorker" in navigator)) return;
    try {
      const registration = await navigator.serviceWorker.getRegistration();
      if (!registration) return;
      const response = await fetch(PROBE_URL, { cache: "no-store" });
      const served = response.headers.get("x-yonder-frontend");
      if (served && served !== FRONTEND_NAME) {
        await registration.unregister();
        const keys = await caches.keys();
        await Promise.all(keys.map((k) => caches.delete(k)));
        location.reload();
      }
    } catch {
      // Best-effort: offline or a restarting server is retried on the next boot.
    }
  }

  function registerServiceWorker() {
    if (import.meta.env.DEV || !("serviceWorker" in navigator)) return;
    void import("virtual:pwa-register").then((m) => {
      m.registerSW({ immediate: true });
    });
    void guardFrontendSwitch();
  }
  return { registerServiceWorker, guardFrontendSwitch };
}

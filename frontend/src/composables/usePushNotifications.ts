import { ref, type Ref } from "vue";
import { ApiError } from "@/api/errors";
import {
  createPushSubscription,
  deletePushSubscription,
  getPushSubscription,
  getVapidPublicKey,
  updatePushSubscription,
  type PushSubscriptionEntity,
} from "@/api/push";

function errorMessage(err: unknown): string {
  return err instanceof ApiError || err instanceof Error
    ? err.message
    : String(err);
}

export type PushNotificationState =
  | "unsupported"
  | "unavailable"
  | "denied"
  | "prompt"
  | "subscribed"
  | "loading";

export interface UsePushNotifications {
  state: Ref<PushNotificationState>;
  error: Ref<string | null>;
  isSupported: Ref<boolean>;
  isSubscribed: Ref<boolean>;
  /** The backend subscription entity, when one exists. */
  subscription: Ref<PushSubscriptionEntity | null>;
  load: () => Promise<void>;
  enable: () => Promise<void>;
  disable: () => Promise<void>;
  /** PATCH alert toggles / policy on the current subscription. */
  update: (alerts?: Record<string, boolean>, policy?: string) => Promise<void>;
}

function urlBase64ToUint8Array(base64url: string): Uint8Array {
  const padding = "=".repeat((4 - (base64url.length % 4)) % 4);
  const base64 = (base64url + padding).replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(base64);
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) {
    bytes[i] = raw.charCodeAt(i);
  }
  return bytes;
}

/**
 * The PWA's service worker registration — registered by
 * `registerSW` at app boot. `.ready` alone would hang forever on
 * builds without a worker (dev mode), hence the timeout race.
 */
async function getRegistration(): Promise<
  ServiceWorkerRegistration | undefined
> {
  if (!("serviceWorker" in navigator)) return undefined;
  try {
    const existing = await navigator.serviceWorker.getRegistration();
    if (existing) return existing;
    return await Promise.race([
      navigator.serviceWorker.ready,
      new Promise<undefined>((resolve) =>
        setTimeout(() => resolve(undefined), 2000),
      ),
    ]);
  } catch {
    return undefined;
  }
}

export function usePushNotifications(): UsePushNotifications {
  const state: Ref<PushNotificationState> = ref("loading");
  const error: Ref<string | null> = ref(null);
  const isSupported = ref(false);
  const isSubscribed = ref(false);
  const subscription: Ref<PushSubscriptionEntity | null> = ref(null);

  let vapidKey: string | null = null;
  let registration: ServiceWorkerRegistration | undefined;

  async function load() {
    error.value = null;
    state.value = "loading";
    subscription.value = null;

    if (
      !("serviceWorker" in navigator) ||
      !("PushManager" in window) ||
      typeof Notification === "undefined"
    ) {
      state.value = "unsupported";
      isSupported.value = false;
      return;
    }
    isSupported.value = true;

    if (Notification.permission === "denied") {
      state.value = "denied";
      isSubscribed.value = false;
      return;
    }

    [vapidKey, registration] = await Promise.all([
      getVapidPublicKey().catch(() => null),
      getRegistration(),
    ]);

    if (!vapidKey || !registration?.pushManager) {
      state.value = "unavailable";
      isSubscribed.value = false;
      return;
    }

    const backend = await getPushSubscription().catch(() => null);
    const browser = await registration.pushManager
      .getSubscription()
      .catch(() => null);
    subscription.value = backend;
    if (backend && browser) {
      // Re-register on every load — a rotated VAPID pair or a
      // lapsed push endpoint heals itself on the next subscribe
      // (the endpoint-keyed upsert refreshes keys/alerts).
      await syncBackend(browser);
      state.value = isSubscribed.value ? "subscribed" : "prompt";
    } else if (browser) {
      // Browser-side subscription the backend forgot — re-register.
      await syncBackend(browser);
      state.value = isSubscribed.value ? "subscribed" : "prompt";
    } else {
      isSubscribed.value = false;
      state.value = "prompt";
    }
  }

  async function syncBackend(sub: PushSubscription) {
    const json = sub.toJSON();
    const keys = json.keys as { p256dh?: string; auth?: string } | undefined;
    if (!json.endpoint || !keys?.p256dh || !keys?.auth) {
      isSubscribed.value = false;
      return;
    }
    try {
      subscription.value = await createPushSubscription(json.endpoint, {
        p256dh: keys.p256dh,
        auth: keys.auth,
      });
      isSubscribed.value = true;
    } catch (err) {
      error.value = errorMessage(err) || "Could not register push subscription";
      isSubscribed.value = false;
    }
  }

  async function enable() {
    error.value = null;
    if (!isSupported.value || !registration?.pushManager || !vapidKey) {
      state.value = isSupported.value ? "unavailable" : "unsupported";
      return;
    }

    state.value = "loading";
    const permission = await Notification.requestPermission();
    if (permission === "denied") {
      state.value = "denied";
      return;
    }

    try {
      const sub = await registration.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: urlBase64ToUint8Array(vapidKey) as BufferSource,
      });
      await syncBackend(sub);
      state.value = isSubscribed.value ? "subscribed" : "prompt";
    } catch (err) {
      isSubscribed.value = false;
      error.value = errorMessage(err) || "Could not enable push notifications";
      state.value = "prompt";
    }
  }

  async function disable() {
    error.value = null;
    state.value = "loading";
    try {
      const sub = await registration?.pushManager.getSubscription();
      if (sub) {
        const endpoint = sub.toJSON().endpoint;
        await sub.unsubscribe();
        if (endpoint) await deletePushSubscription(endpoint);
      } else {
        // No browser-side sub but a stale backend row — delete it too.
        const backend = await getPushSubscription().catch(() => null);
        if (backend?.endpoint) await deletePushSubscription(backend.endpoint);
      }
      subscription.value = null;
      isSubscribed.value = false;
      state.value = "prompt";
    } catch (err) {
      error.value = errorMessage(err) || "Could not disable push notifications";
      state.value = isSubscribed.value ? "subscribed" : "prompt";
    }
  }

  async function update(alerts?: Record<string, boolean>, policy?: string) {
    error.value = null;
    try {
      subscription.value = await updatePushSubscription(alerts, policy);
    } catch (err) {
      error.value = errorMessage(err) || "Could not update push subscription";
    }
  }

  return {
    state,
    error,
    isSupported,
    isSubscribed,
    subscription,
    load,
    enable,
    disable,
    update,
  };
}

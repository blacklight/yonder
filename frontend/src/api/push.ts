import { request } from "./client";
import { ApiError } from "./errors";
import { getInstanceV1 } from "./instance";

/** `GET /api/v1/push/subscription` entity (Mastodon shape). */
export interface PushSubscriptionEntity {
  id: string;
  endpoint: string;
  alerts: Record<string, boolean>;
  policy: "all" | "followed" | "follower" | "none";
  server_key: string | null;
}

/**
 * The instance's VAPID public key — carried on
 * `/api/v1/instance`'s `pleroma.metadata` block (Akkoma-style
 * discovery) so a client can subscribe before any subscription
 * exists. `null` when the admin never generated keys.
 */
export async function getVapidPublicKey(): Promise<string | null> {
  const instance = await getInstanceV1();
  const pleroma = instance.pleroma as
    { metadata?: { vapid_public_key?: string | null } } | undefined;
  return pleroma?.metadata?.vapid_public_key ?? null;
}

/** The caller's current subscription; `null` when none (Mastodon's 404). */
export async function getPushSubscription(): Promise<PushSubscriptionEntity | null> {
  try {
    return await request<PushSubscriptionEntity>({
      path: "/push/subscription",
      silent: true,
    });
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return null;
    throw e;
  }
}

/** Register (or re-register) a browser PushSubscription. */
export function createPushSubscription(
  endpoint: string,
  keys: { p256dh: string; auth: string },
  alerts?: Record<string, boolean>,
): Promise<PushSubscriptionEntity> {
  return request<PushSubscriptionEntity>({
    path: "/push/subscription",
    method: "POST",
    body: {
      subscription: { endpoint, keys },
      ...(alerts ? { data: { alerts } } : {}),
    },
    silent: true,
  });
}

/** Update the current subscription's alert map and/or policy. */
export function updatePushSubscription(
  alerts?: Record<string, boolean>,
  policy?: string,
): Promise<PushSubscriptionEntity> {
  return request<PushSubscriptionEntity>({
    path: "/push/subscription",
    method: "PUT",
    body: {
      ...(alerts ? { data: { alerts } } : {}),
      ...(policy ? { policy } : {}),
    },
    silent: true,
  });
}

/** Drop the caller's subscription for `endpoint`. */
export function deletePushSubscription(endpoint: string): Promise<void> {
  return request<void>({
    path: "/push/subscription",
    method: "DELETE",
    body: { subscription: { endpoint } },
    silent: true,
  });
}

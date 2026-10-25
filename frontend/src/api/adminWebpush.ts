import { request } from "./client";

export interface VapidKeys {
  public_key: string;
  /** The configured `mailto:` subject, for display only. */
  subject: string;
}

/**
 * `POST /api/v1/yonder/admin/webpush/generate_keys` — mints a VAPID
 * P-256 pair, stores both halves as overlays, and rotates existing
 * push subscriptions' encryption. The private key is never returned.
 */
export function generateVapidKeys(): Promise<VapidKeys> {
  return request({
    path: "/yonder/admin/webpush/generate_keys",
    method: "POST",
    body: {},
  });
}

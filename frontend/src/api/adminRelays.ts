import { request } from "./client";

/**
 * `Admin::Relay` — manual-accept federation relay (epic 22). Yonder
 * deliberately skips Mastodon's Follow/Accept handshake: new relays
 * start `pending`, an admin authorizes them, and only `accepted`
 * relays receive public-post fan-out.
 */
export interface AdminRelay {
  id: string;
  inbox_url: string;
  state: "pending" | "accepted" | string;
  created_at: string;
  updated_at: string;
}

const path = (id: string | number) =>
  `/admin/relays/${encodeURIComponent(String(id))}`;

export function listRelays(): Promise<AdminRelay[]> {
  return request({ path: "/admin/relays" });
}

export function addRelay(inbox_url: string): Promise<AdminRelay> {
  return request({
    path: "/admin/relays",
    method: "POST",
    body: { inbox_url },
  });
}

export function authorizeRelay(id: string | number): Promise<AdminRelay> {
  return request({ path: `${path(id)}/authorize`, method: "POST", body: {} });
}

export function deleteRelay(id: string | number): Promise<object> {
  return request({ path: path(id), method: "DELETE" });
}

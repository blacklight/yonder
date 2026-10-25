import { request } from "./client";

/**
 * Where the effective value came from — the backend collapses TOML
 * and compiled-in defaults into `default` (epic 22, docs/admin.md).
 */
export type SettingSource = "overlay" | "env" | "default";

export interface AdminSetting {
  /** Dotted TOML path, e.g. `instance.name`. */
  toml_path: string;
  /** Environment variable that overrides the TOML value. */
  env: string;
  source: SettingSource;
  /** Effective value; `null` on `sensitive` keys (see `set`). */
  value: unknown;
  /** Compiled-in default; `null` on `sensitive` keys. */
  default: unknown;
  /** Present on `sensitive` keys: whether a value exists at all. */
  set?: boolean;
  requires_restart: boolean;
  sensitive: boolean;
  writable: boolean;
}

const path = (key: string) =>
  `/yonder/admin/settings/${encodeURIComponent(key)}`;

/** `GET /api/v1/yonder/admin/settings` — optionally `?section=`. */
export function listAdminSettings(section?: string): Promise<AdminSetting[]> {
  return request({ path: "/yonder/admin/settings", query: { section } });
}

/** `PUT …/{key}` — validated server-side; 422 surfaces in `ApiError`. */
export function setAdminSetting(
  key: string,
  value: unknown,
): Promise<AdminSetting> {
  return request({ path: path(key), method: "PUT", body: { value } });
}

/** `DELETE …/{key}` — idempotent; returns the entry after fallback. */
export function resetAdminSetting(key: string): Promise<AdminSetting> {
  return request({ path: path(key), method: "DELETE" });
}

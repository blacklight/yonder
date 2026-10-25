import { request } from "./client";

export interface AdminFrontend {
  name: string;
  version?: string;
  /** `builtin` | `zip` | `path` | `git` (admin list only). */
  source?: string;
  builtin?: boolean;
}

export interface AdminFrontends {
  default: string;
  frontends: AdminFrontend[];
}

/** `GET /api/v1/yonder/admin/frontends` — installed + source metadata. */
export function listAdminFrontends(): Promise<AdminFrontends> {
  return request({ path: "/yonder/admin/frontends" });
}

/**
 * `GET`/`PUT /api/v1/yonder/admin/frontend` — the default frontend as
 * a regular settings entry (admin.default_frontend).
 */
export function setDefaultFrontend(name: string) {
  return request({
    path: "/yonder/admin/frontend",
    method: "PUT",
    body: { value: name },
  });
}

/** Install from a zip upload (`file` + `name` + optional `version`). */
export function installFrontendZip(body: {
  file: File;
  name: string;
  version?: string;
}): Promise<AdminFrontend> {
  const form = new FormData();
  form.set("file", body.file);
  form.set("name", body.name);
  if (body.version) form.set("version", body.version);
  return request({
    path: "/yonder/admin/frontends",
    method: "POST",
    body: form,
  });
}

export type FrontendInstallBody =
  | { name: string; source: "path"; path: string; version?: string }
  | {
      name: string;
      source: "git";
      url: string;
      ref?: string;
      version?: string;
    };

/** Install from a server path or a git repository. */
export function installFrontend(
  body: FrontendInstallBody,
): Promise<AdminFrontend> {
  return request({
    path: "/yonder/admin/frontends",
    method: "POST",
    body,
  });
}

/** `DELETE …/{name}` — 422 on builtin or the current default. */
export function deleteFrontend(name: string): Promise<object> {
  return request({
    path: `/yonder/admin/frontends/${encodeURIComponent(name)}`,
    method: "DELETE",
  });
}

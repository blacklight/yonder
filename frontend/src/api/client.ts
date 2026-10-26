import { useAuthStore } from "@/stores/auth";
import { useToastsStore } from "@/stores/toasts";
import { i18n } from "@/i18n";
import { API_PREFIX, buildUrl } from "./config";
import { ApiError } from "./errors";

type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export interface RequestOptions {
  /**
   * Path under `/api/v1`, e.g. `/instance` or `/yonder/settings`.
   * A path starting with `/api/` is used verbatim (for `/api/v2`
   * endpoints such as media upload).
   */
  path: string;
  method?: HttpMethod;
  /** JSON body, or a `FormData` for multipart uploads (no JSON header). */
  body?: unknown;
  query?: Record<
    string,
    string | number | boolean | Array<string | number> | undefined | null
  >;
  /** Extra request headers, e.g. `Idempotency-Key` on status create. */
  headers?: Record<string, string>;
  signal?: AbortSignal;
  skipAuth?: boolean;
  /**
   * Best-effort calls (badge refreshes, read markers) set this so a
   * failure doesn't raise an error toast alongside their own catch.
   */
  silent?: boolean;
}

/** Surface a failed call as an error toast unless the caller opted out. */
function failureToast(opts: RequestOptions, message?: string) {
  if (opts.silent) return;
  useToastsStore().push({
    text: message || i18n.global.t("common.error"),
    kind: "error",
    icon: "triangle-exclamation",
  });
}

export interface ApiResponse<T> {
  data: T;
  response: Response;
}

export async function request<T>(opts: RequestOptions): Promise<T> {
  return (await requestWithResponse<T>(opts)).data;
}

// The raw `Response` is returned alongside the data so callers can
// read pagination headers (`Link: <...>; rel="next"`, see
// api/pagination) and other metadata.
export async function requestWithResponse<T>(
  opts: RequestOptions,
): Promise<ApiResponse<T>> {
  const headers = new Headers();
  // The token actually sent, for the 401 eviction below — a 401 on a
  // request that carried a token means it is revoked/expired, so the
  // store forgets it (anonymous requests never clear anything).
  let sentToken: string | null = null;
  if (!opts.skipAuth) {
    sentToken = useAuthStore().token;
    if (sentToken) headers.set("Authorization", `Bearer ${sentToken}`);
  }
  let body: BodyInit | undefined;
  if (opts.body instanceof FormData) {
    // Multipart: the browser sets Content-Type with the boundary.
    body = opts.body;
  } else if (opts.body !== undefined) {
    headers.set("Content-Type", "application/json");
    body = JSON.stringify(opts.body);
  }
  for (const [key, value] of Object.entries(opts.headers ?? {})) {
    headers.set(key, value);
  }
  const fullPath = opts.path.startsWith("/api/")
    ? opts.path
    : `${API_PREFIX}${opts.path}`;
  let response: Response;
  try {
    response = await fetch(buildUrl(fullPath, opts.query), {
      method: opts.method ?? "GET",
      headers,
      body,
      signal: opts.signal ?? null,
    });
  } catch (e) {
    failureToast(opts);
    throw e;
  }
  const text = await response.text();
  let data: unknown = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = text;
    }
  }
  if (!response.ok) {
    // Evict only if the sent token is still the active one — a stale
    // in-flight request must not wipe a newer session's token.
    if (response.status === 401 && sentToken) {
      const auth = useAuthStore();
      if (auth.token === sentToken) auth.setToken(null);
    }
    const error = await ApiError.fromResponse(response, data);
    failureToast(opts, error.message);
    throw error;
  }
  return { data: data as T, response };
}

export interface BlobResult {
  blob: Blob;
  filename: string;
}

/**
 * Bearer-authenticated blob download — exports can't use a bare
 * `<a href>` (no Authorization header). Method/body are supported
 * because the inline CSV/JSON exports come from `POST
 * /api/v1/yonder/exports`, while the archive uses `GET
 * /api/v1/yonder/exports/{id}/download`. Same token source, same
 * `ApiError`, same 401 eviction as `requestWithResponse`. Callers own
 * the object-URL lifecycle (create → click → revoke in `finally`).
 */
export async function requestBlob(opts: RequestOptions): Promise<BlobResult> {
  const headers = new Headers();
  let sentToken: string | null = null;
  if (!opts.skipAuth) {
    sentToken = useAuthStore().token;
    if (sentToken) headers.set("Authorization", `Bearer ${sentToken}`);
  }
  let body: BodyInit | undefined;
  if (opts.body instanceof FormData) {
    body = opts.body;
  } else if (opts.body !== undefined) {
    headers.set("Content-Type", "application/json");
    body = JSON.stringify(opts.body);
  }
  for (const [key, value] of Object.entries(opts.headers ?? {})) {
    headers.set(key, value);
  }
  const fullPath = opts.path.startsWith("/api/")
    ? opts.path
    : `${API_PREFIX}${opts.path}`;
  let response: Response;
  try {
    response = await fetch(buildUrl(fullPath, opts.query), {
      method: opts.method ?? "GET",
      headers,
      body,
      signal: opts.signal ?? null,
    });
  } catch (e) {
    failureToast(opts);
    throw e;
  }
  if (!response.ok) {
    if (response.status === 401 && sentToken) {
      const auth = useAuthStore();
      if (auth.token === sentToken) auth.setToken(null);
    }
    let data: unknown = null;
    const text = await response.text();
    if (text) {
      try {
        data = JSON.parse(text);
      } catch {
        data = text;
      }
    }
    const error = await ApiError.fromResponse(response, data);
    failureToast(opts, error.message);
    throw error;
  }
  return {
    blob: await response.blob(),
    filename: dispositionFilename(response.headers.get("content-disposition")),
  };
}

/** `filename*`/`filename` from Content-Disposition; a bare name, never a path. */
function dispositionFilename(header: string | null): string {
  if (header) {
    const star = header.match(/filename\*\s*=\s*(?:UTF-8|utf-8)''([^;]+)/);
    const raw = star?.[1] ?? header.match(/filename\s*=\s*"?([^";]+)"?/)?.[1];
    if (raw) {
      try {
        const name = decodeURIComponent(raw).split(/[\\/]/).pop() ?? "";
        if (name) return name;
      } catch {
        // Malformed %-escape in the header — fall through to the default.
      }
    }
  }
  return "download";
}

export { ApiError };

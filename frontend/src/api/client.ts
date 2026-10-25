import { useAuthStore } from "@/stores/auth";
import { API_PREFIX, buildUrl } from "./config";
import { ApiError } from "./errors";

type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export interface RequestOptions {
  /** Path under `/api/v1`, e.g. `/instance` or `/yonder/settings`. */
  path: string;
  method?: HttpMethod;
  body?: unknown;
  query?: Record<string, string | number | boolean | undefined | null>;
  signal?: AbortSignal;
  skipAuth?: boolean;
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
  if (!opts.skipAuth) {
    const token = useAuthStore().token;
    if (token) headers.set("Authorization", `Bearer ${token}`);
  }
  let body: BodyInit | undefined;
  if (opts.body !== undefined) {
    headers.set("Content-Type", "application/json");
    body = JSON.stringify(opts.body);
  }
  const response = await fetch(
    buildUrl(`${API_PREFIX}${opts.path}`, opts.query),
    {
      method: opts.method ?? "GET",
      headers,
      body,
      signal: opts.signal ?? null,
    },
  );
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
    throw await ApiError.fromResponse(response, data);
  }
  return { data: data as T, response };
}

export { ApiError };

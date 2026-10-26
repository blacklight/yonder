import { ApiError } from "./errors";

/**
 * `/oauth/*` endpoints — deliberately NOT through `request()`:
 *
 * - they live outside `/api/v1` (the request path convention),
 * - they take `application/x-www-form-urlencoded` bodies, and
 * - errors use the doorkeeper shape `{"error","error_description"}`,
 *   with `mfa_required` carrying `mfa_token`/`methods` at top level.
 *
 * No bearer is sent and there is no 401 eviction — a failed login must
 * never disturb an existing session, and these calls happen before any
 * token exists.
 */
export interface TokenBody {
  access_token: string;
  token_type: string;
  scope: string;
  created_at: number;
  expires_in?: number;
  refresh_token?: string;
}

/** The password grant verified credentials but the account has 2FA. */
export class MfaRequired extends ApiError {
  mfaToken: string;
  methods: string[];

  constructor(mfaToken: string, methods: string[], message: string) {
    super(message, 400);
    this.name = "MfaRequired";
    this.mfaToken = mfaToken;
    this.methods = methods;
  }
}

async function postForm(
  path: string,
  params: Record<string, string>,
): Promise<TokenBody> {
  const response = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams(params),
  });
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
    const body = (data && typeof data === "object" ? data : {}) as Record<
      string,
      unknown
    >;
    const description =
      typeof body.error_description === "string"
        ? body.error_description
        : response.statusText;
    if (body.error === "mfa_required" && typeof body.mfa_token === "string") {
      const methods = Array.isArray(body.methods)
        ? body.methods.filter((m): m is string => typeof m === "string")
        : [];
      throw new MfaRequired(body.mfa_token, methods, description);
    }
    throw new ApiError(description, response.status, data);
  }
  // A 200 that isn't a token entity (captive portal, misconfigured
  // proxy) must fail loudly here — `undefined` flowing into
  // `signInWithToken` would surface as a confusing verify failure.
  const token = data as TokenBody;
  if (typeof token?.access_token !== "string" || !token.access_token) {
    throw new ApiError(
      "The server returned an invalid token response.",
      response.status,
      data,
    );
  }
  return token;
}

/**
 * `POST /oauth/token` password grant — accepts an email or the local
 * username (the backend matches both). Throws {@link MfaRequired} when
 * a second factor is needed; complete via {@link twoFactorVerify}.
 */
export function passwordToken(params: {
  username: string;
  password: string;
}): Promise<TokenBody> {
  return postForm("/oauth/token", {
    grant_type: "password",
    username: params.username,
    password: params.password,
    // No client app — first-party UI. Request the full scope set so
    // admin/moderation endpoints work for staff accounts (the role
    // check still gates them for regular users).
    scope: "read write follow push admin",
  });
}

/** The authorization-request params carried in `/oauth/authorize`'s query. */
export interface AuthorizeQuery {
  client_id: string;
  redirect_uri: string;
  response_type?: string;
  scope?: string;
  state?: string;
  code_challenge?: string;
  code_challenge_method?: string;
}

/** `GET /oauth/authorize` metadata — the client app's identity. */
export interface AuthorizeInfo {
  client_name: string;
  website: string | null;
  /** The app's registered scopes (not the request's). */
  scopes: string[];
  redirect_uri: string;
}

/** `POST /oauth/authorize` result — `redirect_to` carries `code`/`state`. */
export interface AuthorizeResult {
  code: string;
  state: string | null;
  redirect_to: string;
}

function authorizeParams(q: AuthorizeQuery): URLSearchParams {
  const params = new URLSearchParams({ client_id: q.client_id });
  for (const [key, value] of Object.entries({
    redirect_uri: q.redirect_uri,
    response_type: q.response_type ?? "code",
    scope: q.scope,
    state: q.state,
    code_challenge: q.code_challenge,
    code_challenge_method: q.code_challenge_method,
  })) {
    if (value) params.set(key, value);
  }
  return params;
}

async function errorFrom(response: Response): Promise<ApiError> {
  const text = await response.text();
  let message = response.statusText;
  try {
    const body = JSON.parse(text) as Record<string, unknown>;
    if (typeof body.error === "string") message = body.error;
    if (typeof body.error_description === "string")
      message = body.error_description;
  } catch {
    /* non-JSON error body — keep the status text */
  }
  return new ApiError(message, response.status);
}

/**
 * `GET /oauth/authorize` — consent metadata for a client app. Needs
 * `Accept: application/json`: the same URL answers browsers with the
 * SPA shell (the consent page itself).
 */
export async function authorizeInfo(q: AuthorizeQuery): Promise<AuthorizeInfo> {
  const response = await fetch(`/oauth/authorize?${authorizeParams(q)}`, {
    headers: { Accept: "application/json" },
  });
  if (!response.ok) throw await errorFrom(response);
  return (await response.json()) as AuthorizeInfo;
}

/**
 * `POST /oauth/authorize` — consent grant: issues the authorization
 * code the client exchanges at `/oauth/token`. Bearer-auth (the
 * signed-in user consents), form-encoded like the rest of `/oauth/*`;
 * errors use the `{error}` API shape, not doorkeeper's.
 */
export async function authorizeApp(
  q: AuthorizeQuery,
): Promise<AuthorizeResult> {
  const { useAuthStore } = await import("@/stores/auth");
  const token = useAuthStore().token;
  const response = await fetch("/oauth/authorize", {
    method: "POST",
    headers: {
      "Content-Type": "application/x-www-form-urlencoded",
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    body: authorizeParams(q),
  });
  if (!response.ok) throw await errorFrom(response);
  return (await response.json()) as AuthorizeResult;
}

/**
 * Hand the browser to a client app's redirect URI — custom schemes
 * (`moshidon-android-auth://…`) are resolved by the platform's intent
 * filters, http(s) by ordinary navigation. Extracted so specs can
 * stub it (jsdom can't spy on `location.assign`).
 */
export function navigateTo(url: string) {
  window.location.assign(url);
}

/**
 * `POST /oauth/2fa/verify` — finish an `mfa_required` login with a TOTP
 * or backup code. Returns the same token entity as the password grant.
 */
export function twoFactorVerify(params: {
  mfaToken: string;
  code: string;
}): Promise<TokenBody> {
  return postForm("/oauth/2fa/verify", {
    mfa_token: params.mfaToken,
    code: params.code,
  });
}

/**
 * `POST /oauth/2fa/webauthn/assert/begin` — assertion options for the
 * pending login's registered security keys.
 */
export async function webauthnAssertBegin(mfaToken: string): Promise<unknown> {
  const response = await fetch("/oauth/2fa/webauthn/assert/begin", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ mfa_token: mfaToken }),
  });
  if (!response.ok) throw await errorFrom(response);
  return response.json();
}

/**
 * `POST /oauth/2fa/webauthn/assert` — verify the assertion and issue
 * the token pair (`credential` is the serialized PublicKeyCredential).
 */
export function webauthnAssert(params: {
  mfaToken: string;
  credential: unknown;
}): Promise<TokenBody> {
  return postForm("/oauth/2fa/webauthn/assert", {
    mfa_token: params.mfaToken,
    credential: JSON.stringify(params.credential),
  });
}

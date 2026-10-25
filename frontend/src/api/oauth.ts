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
    throw new ApiError("The server returned an invalid token response.", response.status, data);
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
    // No client app — first-party UI. `read write` is the same scope
    // set anonymous registration receives.
    scope: "read write",
  });
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

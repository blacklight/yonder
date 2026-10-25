import { describe, expect, it, vi, beforeEach } from "vitest";
import { ApiError } from "./errors";
import { MfaRequired, passwordToken, twoFactorVerify } from "./oauth";

function mockFetch(status: number, body: unknown) {
  return vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(() =>
      Promise.resolve(new Response(JSON.stringify(body), { status })),
    );
}

describe("oauth api", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("posts the password grant form-encoded to /oauth/token", async () => {
    const spy = mockFetch(200, { access_token: "tok" });
    const res = await passwordToken({ username: "alice", password: "pw" });
    expect(spy.mock.calls[0][0]).toBe("/oauth/token");
    const init = spy.mock.calls[0][1]!;
    expect(init.method).toBe("POST");
    expect(
      (init.headers as Record<string, string>)["Content-Type"],
    ).toBe("application/x-www-form-urlencoded");
    const body = new URLSearchParams(init.body as string);
    expect(body.get("grant_type")).toBe("password");
    expect(body.get("username")).toBe("alice");
    expect(body.get("password")).toBe("pw");
    expect(body.get("scope")).toBe("read write");
    // No bearer — the token endpoint runs before any session exists.
    expect((init.headers as Record<string, string>).Authorization).toBeUndefined();
    expect(res.access_token).toBe("tok");
  });

  it("maps invalid_grant to ApiError with the doorkeeper description", async () => {
    mockFetch(400, {
      error: "invalid_grant",
      error_description: "The provided authorization grant is invalid.",
    });
    const err = (await passwordToken({ username: "a", password: "b" }).catch(
      (e) => e,
    )) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
    expect(err).not.toBeInstanceOf(MfaRequired);
    expect(err.status).toBe(400);
    expect(err.message).toBe(
      "The provided authorization grant is invalid.",
    );
  });

  it("maps mfa_required to MfaRequired carrying the token and methods", async () => {
    mockFetch(400, {
      error: "mfa_required",
      error_description: "Two-factor authentication is required.",
      mfa_token: "mfa-123",
      methods: ["totp", "webauthn"],
    });
    const err = (await passwordToken({ username: "a", password: "b" }).catch(
      (e) => e,
    )) as MfaRequired;
    expect(err).toBeInstanceOf(MfaRequired);
    expect(err.mfaToken).toBe("mfa-123");
    expect(err.methods).toEqual(["totp", "webauthn"]);
  });

  it("rejects a 200 that is not a token entity", async () => {
    mockFetch(200, "<html>captive portal</html>");
    const err = (await passwordToken({ username: "a", password: "b" }).catch(
      (e) => e,
    )) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
    expect(err.message).toContain("invalid token response");
  });

  it("twoFactorVerify posts the pending-login exchange", async () => {
    const spy = mockFetch(200, { access_token: "tok" });
    await twoFactorVerify({ mfaToken: "mfa-123", code: "123456" });
    expect(spy.mock.calls[0][0]).toBe("/oauth/2fa/verify");
    const body = new URLSearchParams(spy.mock.calls[0][1]!.body as string);
    expect(body.get("mfa_token")).toBe("mfa-123");
    expect(body.get("code")).toBe("123456");
  });

  it("a failed second factor is an ordinary grant error", async () => {
    mockFetch(400, {
      error: "invalid_grant",
      error_description: "The provided two-factor credential is invalid.",
    });
    const err = (await twoFactorVerify({
      mfaToken: "m",
      code: "0",
    }).catch((e) => e)) as ApiError;
    expect(err).not.toBeInstanceOf(MfaRequired);
    expect(err.message).toContain("two-factor");
  });
});

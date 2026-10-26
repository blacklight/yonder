/**
 * Minimal browser glue for the server's webauthn-rs ceremonies.
 *
 * `register/begin` returns a `CreationChallengeResponse`
 * (`{publicKey: …}`) whose binary fields are base64url strings; the
 * Web Authentication API wants `ArrayBuffer`s, and the finished
 * `PublicKeyCredential` must be serialized back into the
 * `RegisterPublicKeyCredential` JSON shape the server verifies.
 */

function b64urlToBuffer(value: string): ArrayBuffer {
  const padded = value.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes.buffer;
}

function bufferToB64url(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

interface CredentialDescriptor {
  type: string;
  id: string;
  transports?: string[];
}

interface CreationOptionsWire {
  publicKey: {
    challenge: string;
    user: { id: string; name: string; displayName: string };
    excludeCredentials?: CredentialDescriptor[];
    [key: string]: unknown;
  };
}

/** Security-key ceremonies need `PublicKeyCredential` support. */
export function webauthnSupported(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof window.PublicKeyCredential === "function" &&
    !!navigator.credentials
  );
}

/**
 * Run `navigator.credentials.create` for a `register/begin` response
 * and return the credential in the JSON shape `register` verifies.
 * Throws the DOMException the browser raises on cancel/failure.
 */
export async function createSecurityKey(options: unknown): Promise<unknown> {
  const wire = options as CreationOptionsWire;
  const { publicKey: pk } = wire;
  // `hints` and `extensions` (credProtect/uvm/credProps) are advisory
  // browser hints — the server doesn't verify them, and Firefox-family
  // browsers abort the ceremony with NotAllowedError on the credProtect
  // policy webauthn-rs requests. Pass through everything else verbatim.
  const rest = { ...pk } as Record<string, unknown>;
  delete rest.hints;
  delete rest.extensions;
  const credential = (await navigator.credentials.create({
    publicKey: {
      ...rest,
      challenge: b64urlToBuffer(pk.challenge),
      user: { ...pk.user, id: b64urlToBuffer(pk.user.id) },
      excludeCredentials: (pk.excludeCredentials ?? []).map((c) => ({
        ...c,
        type: "public-key",
        id: b64urlToBuffer(c.id),
      })),
    } as PublicKeyCredentialCreationOptions,
  })) as PublicKeyCredential | null;
  if (!credential) throw new Error("the security key ceremony was cancelled");
  const response = credential.response as AuthenticatorAttestationResponse;
  return {
    id: credential.id,
    rawId: bufferToB64url(credential.rawId),
    type: credential.type,
    response: {
      attestationObject: bufferToB64url(response.attestationObject),
      clientDataJSON: bufferToB64url(response.clientDataJSON),
    },
  };
}

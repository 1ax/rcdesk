// Binds an SDP offer/answer to the OPAQUE session key from login (slice
// 3.2e): defense against a man-in-the-middle at the signaling server, who
// can see and rewrite `offer`/`answer` but (without the password) never
// learns the OPAQUE session key both sides derived during login. The
// browser side of `host/src/dtls_bind.rs` -- see that module's doc comment
// for the full scheme; both sides must produce byte-identical tags or every
// session with a password set would fail to connect.

/** Extracts and normalizes the first `a=fingerprint:<alg> <hex>` line in
 * `sdp` (session level or an m-section, whichever appears first -- SDP
 * lines are read in document order), to `"<alg> <HEX>"` -- algorithm
 * lowercased, hex uppercased, one space between them. `null` when there is
 * no such line, or the line after `a=fingerprint:` doesn't have both an
 * algorithm and a hex value. Mirrors `host/src/dtls_bind.rs::fingerprint_from_sdp`
 * exactly. */
export function fingerprintFromSdp(sdp: string): string | null {
  for (const rawLine of sdp.split(/\r?\n/)) {
    const trimmed = rawLine.trim();
    if (!trimmed.startsWith("a=fingerprint:")) continue;
    const rest = trimmed.slice("a=fingerprint:".length);
    const spaceIdx = rest.indexOf(" ");
    if (spaceIdx === -1) continue;
    const alg = rest.slice(0, spaceIdx).trim();
    const hex = rest.slice(spaceIdx + 1).trim();
    if (!alg || !hex) continue;
    return `${alg.toLowerCase()} ${hex.toUpperCase()}`;
  }
  return null;
}

/** Decodes a base64 URL-safe, no-padding string (the alphabet
 * `@serenity-kit/opaque` and `host/src/access.rs` both use) into raw bytes.
 * Throws on invalid input (via `atob`), same as the host's `base64` crate
 * returning `Err` -- callers that need `false`-on-garbage instead (like
 * `verifyAuthTag`) catch it. */
export function decodeBase64Url(s: string): Uint8Array<ArrayBuffer> {
  const padded = s.replace(/-/g, "+").replace(/_/g, "/");
  const pad = padded.length % 4 === 0 ? "" : "=".repeat(4 - (padded.length % 4));
  const binary = atob(padded + pad);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

/** Encodes raw bytes as base64 URL-safe, no padding -- the inverse of
 * `decodeBase64Url`. */
export function encodeBase64Url(bytes: Uint8Array<ArrayBufferLike>): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** The fixed domain-separation prefix for the HMAC message -- see this
 * module's doc comment and `host/src/dtls_bind.rs::AUTH_TAG_PREFIX`. */
const AUTH_TAG_PREFIX = "rcdesk-dtls-v1";

/** `HMAC-SHA256(keyBytes, "rcdesk-dtls-v1|" + role + "|" + fingerprint)`,
 * base64 (URL-safe, no padding), via WebCrypto. `keyBytes` is the OPAQUE
 * session key's raw bytes (decode `hostAuth.ts`'s `sessionKey` with
 * `decodeBase64Url` first). Mirrors `host/src/dtls_bind.rs::auth_tag`. */
export async function authTag(
  keyBytes: Uint8Array<ArrayBuffer>,
  role: string,
  fingerprint: string,
): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw",
    keyBytes,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const message = new TextEncoder().encode(`${AUTH_TAG_PREFIX}|${role}|${fingerprint}`);
  const signature = await crypto.subtle.sign("HMAC", key, message);
  return encodeBase64Url(new Uint8Array(signature));
}

/** Constant-time byte comparison: checks the length up front (an early exit
 * here leaks nothing an attacker doesn't already know, the tag's length is
 * fixed and public), then compares every byte of the rest without an early
 * exit on the first mismatch. */
function timingSafeEqual(a: Uint8Array<ArrayBufferLike>, b: Uint8Array<ArrayBufferLike>): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) {
    diff |= a[i] ^ b[i];
  }
  return diff === 0;
}

/** Verifies `tag` against `authTag(keyBytes, role, fingerprint)` in constant
 * time (`timingSafeEqual`). Resolves `false` -- never throws to the caller
 * -- on invalid base64 in `tag` or a genuine mismatch: both just mean "not
 * authenticated". Mirrors `host/src/dtls_bind.rs::verify_auth_tag`. */
export async function verifyAuthTag(
  keyBytes: Uint8Array<ArrayBuffer>,
  role: string,
  fingerprint: string,
  tag: string,
): Promise<boolean> {
  let tagBytes: Uint8Array<ArrayBuffer>;
  try {
    tagBytes = decodeBase64Url(tag);
  } catch {
    return false;
  }
  const expected = decodeBase64Url(await authTag(keyBytes, role, fingerprint));
  return timingSafeEqual(tagBytes, expected);
}

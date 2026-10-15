// Pure helpers for passkey login / the account screen (slice 3.7c): the
// WebAuthn JSON <-> browser object conversions in the exact shapes
// `webauthn-rs` 0.5.5 / `webauthn-rs-proto` use on the server side, plus
// display text. No DOM access, so this is unit testable without a browser.
//
// Server -> client (`passkey_register_options` / `passkey_login_options`):
// the `options` string is `CreationChallengeResponse` (`{publicKey}`) resp.
// `RequestChallengeResponse` (`{publicKey, mediation?}`); every
// `Base64UrlSafeData` (challenge, user.id, excludeCredentials[].id,
// allowCredentials[].id) is a base64url string without padding.
// Client -> server (`passkey_register_finish` / `passkey_login_finish`): the
// `credential` string is `RegisterPublicKeyCredential` resp. `PublicKeyCredential`
// serialized by serde (see the doc comments on the two functions below).
//
// The browser's own `toJSON()` / `parse*FromJSON` are deliberately not used:
// support and field names differ between browsers.

import type { BrowserEntry } from "./generated/BrowserEntry";
import type { PasskeyEntry } from "./generated/PasskeyEntry";
import { formatLastSeen } from "./myDevices";

// ---------------------------------------------------------------------------
// base64url

/** Encodes bytes as base64url without padding (`-`/`_` alphabet). */
export function base64urlEncode(data: ArrayBuffer | ArrayBufferView): string {
  const bytes =
    data instanceof ArrayBuffer
      ? new Uint8Array(data)
      : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Decodes base64url (padding optional, standard `+`/`/` also tolerated). */
export function base64urlDecode(text: string): ArrayBuffer {
  const standard = text.replace(/-/g, "+").replace(/_/g, "/");
  const padded = standard + "=".repeat((4 - (standard.length % 4)) % 4);
  const binary = atob(padded);
  const buffer = new ArrayBuffer(binary.length);
  const bytes = new Uint8Array(buffer);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return buffer;
}

// ---------------------------------------------------------------------------
// server options -> browser options

type Json = Record<string, unknown>;

function asObject(value: unknown, what: string): Json {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${what}: expected an object`);
  }
  return value as Json;
}

function asString(value: unknown, what: string): string {
  if (typeof value !== "string") throw new Error(`${what}: expected a string`);
  return value;
}

/** Converts `[{type, id: base64url, transports?}]` descriptors, `id` to bytes. */
function descriptorsFromServer(value: unknown, what: string): unknown[] {
  if (!Array.isArray(value)) throw new Error(`${what}: expected an array`);
  return value.map((item, i) => {
    const obj = asObject(item, `${what}[${i}]`);
    return { ...obj, id: base64urlDecode(asString(obj.id, `${what}[${i}].id`)) };
  });
}

/** `CreationChallengeResponse` JSON (from `passkey_register_options`) ->
 * `PublicKeyCredentialCreationOptions`: `challenge`, `user.id` and
 * `excludeCredentials[].id` become `ArrayBuffer`s; the rest passes through. */
export function creationOptionsFromServer(json: string): PublicKeyCredentialCreationOptions {
  const root = asObject(JSON.parse(json), "creation options");
  const pk = asObject(root.publicKey, "publicKey");
  const user = asObject(pk.user, "user");
  const out: Json = {
    ...pk,
    challenge: base64urlDecode(asString(pk.challenge, "challenge")),
    user: { ...user, id: base64urlDecode(asString(user.id, "user.id")) },
  };
  if (pk.excludeCredentials !== undefined && pk.excludeCredentials !== null) {
    out.excludeCredentials = descriptorsFromServer(pk.excludeCredentials, "excludeCredentials");
  }
  return out as unknown as PublicKeyCredentialCreationOptions;
}

/** `RequestChallengeResponse` JSON (from `passkey_login_options`) ->
 * `PublicKeyCredentialRequestOptions`. The server's `mediation` (conditional
 * UI) is dropped on purpose: login is a button press, so the dialog is modal. */
export function requestOptionsFromServer(json: string): PublicKeyCredentialRequestOptions {
  const root = asObject(JSON.parse(json), "request options");
  const pk = asObject(root.publicKey, "publicKey");
  const out: Json = {
    ...pk,
    challenge: base64urlDecode(asString(pk.challenge, "challenge")),
  };
  if (pk.allowCredentials !== undefined && pk.allowCredentials !== null) {
    out.allowCredentials = descriptorsFromServer(pk.allowCredentials, "allowCredentials");
  }
  return out as unknown as PublicKeyCredentialRequestOptions;
}

// ---------------------------------------------------------------------------
// browser credential -> server JSON

/** The parts of an attestation `PublicKeyCredential` we read (structural, so
 * tests can pass plain objects). */
export interface AttestationCredentialLike {
  id: string;
  rawId: ArrayBuffer;
  type: string;
  response: {
    attestationObject: ArrayBuffer;
    clientDataJSON: ArrayBuffer;
    getTransports?: () => string[];
  };
  getClientExtensionResults?: () => Record<string, unknown>;
}

/** The parts of an assertion `PublicKeyCredential` we read. */
export interface AssertionCredentialLike {
  id: string;
  rawId: ArrayBuffer;
  type: string;
  response: {
    authenticatorData: ArrayBuffer;
    clientDataJSON: ArrayBuffer;
    signature: ArrayBuffer;
    userHandle: ArrayBuffer | null;
  };
  getClientExtensionResults?: () => Record<string, unknown>;
}

/** `RegisterPublicKeyCredential` (webauthn-rs-proto `attest.rs`):
 * `{id, rawId, response: {attestationObject, clientDataJSON, transports?},
 * type, extensions}`. Only the extension outputs the struct models
 * (`appid`, `credProps: {rk}`) are forwarded. */
export function registrationToServer(cred: AttestationCredentialLike): string {
  const response: Json = {
    attestationObject: base64urlEncode(cred.response.attestationObject),
    clientDataJSON: base64urlEncode(cred.response.clientDataJSON),
  };
  const transports = cred.response.getTransports?.();
  if (transports && transports.length > 0) response.transports = transports;

  const results = cred.getClientExtensionResults?.() ?? {};
  const extensions: Json = {};
  if (typeof results.appid === "boolean") extensions.appid = results.appid;
  const credProps = results.credProps;
  if (typeof credProps === "object" && credProps !== null) {
    const rk = (credProps as Json).rk;
    extensions.credProps = typeof rk === "boolean" ? { rk } : {};
  }

  return JSON.stringify({
    id: cred.id,
    rawId: base64urlEncode(cred.rawId),
    response,
    type: cred.type,
    extensions,
  });
}

/** `PublicKeyCredential` (webauthn-rs-proto `auth.rs`): `{id, rawId,
 * response: {authenticatorData, clientDataJSON, signature, userHandle},
 * extensions, type}`. `userHandle` is `null` when the authenticator gave none. */
export function assertionToServer(cred: AssertionCredentialLike): string {
  const results = cred.getClientExtensionResults?.() ?? {};
  const extensions: Json = {};
  if (typeof results.appid === "boolean") extensions.appid = results.appid;

  return JSON.stringify({
    id: cred.id,
    rawId: base64urlEncode(cred.rawId),
    response: {
      authenticatorData: base64urlEncode(cred.response.authenticatorData),
      clientDataJSON: base64urlEncode(cred.response.clientDataJSON),
      signature: base64urlEncode(cred.response.signature),
      userHandle: cred.response.userHandle ? base64urlEncode(cred.response.userHandle) : null,
    },
    extensions,
    type: cred.type,
  });
}

// ---------------------------------------------------------------------------
// display text and screen logic

/** "Chrome · macOS" style label from a User-Agent; "Браузер" when unknown.
 * Order matters: Edge/Opera/Chrome-on-iOS UAs also contain "Chrome" and
 * "Safari", and Chrome's contains "Safari". */
export function browserLabel(userAgent: string): string {
  const ua = userAgent ?? "";
  let browser: string | null = null;
  if (/\bEdg(e|A|iOS)?\//.test(ua)) browser = "Edge";
  else if (/\bOPR\/|\bOpera\b|\bOPT\//.test(ua)) browser = "Opera";
  else if (/\bFirefox\/|\bFxiOS\//.test(ua)) browser = "Firefox";
  else if (/\bChrome\/|\bChromium\/|\bCriOS\//.test(ua)) browser = "Chrome";
  else if (/\bSafari\//.test(ua) && /\bVersion\//.test(ua)) browser = "Safari";
  if (browser === null) return "Браузер";

  let os: string | null = null;
  if (/iPhone|iPad|iPod/.test(ua)) os = "iOS";
  else if (/Android/.test(ua)) os = "Android";
  else if (/Windows/.test(ua)) os = "Windows";
  else if (/CrOS/.test(ua)) os = "ChromeOS";
  else if (/Macintosh|Mac OS X/.test(ua)) os = "macOS";
  else if (/Linux|X11/.test(ua)) os = "Linux";
  return os ? `${browser} · ${os}` : browser;
}

/** Which account row the first screen shows. Hidden until the answers are
 * known: `passkeys` (`null` before `passkeys` answered; not awaited when
 * passkeys are unsupported) and `email` (`undefined` before `account`
 * answered). An account is "has a passkey OR has an email"; otherwise the
 * guest buttons (email ones always, passkey ones only when supported). */
export function accountRowMode(
  supported: boolean,
  passkeys: PasskeyEntry[] | null,
  email: string | null | undefined,
): "hidden" | "guest" | "account" {
  if (email === undefined) return "hidden";
  if (supported && passkeys === null) return "hidden";
  const hasPasskey = (passkeys ?? []).length > 0;
  return hasPasskey || email !== null ? "account" : "guest";
}

/** Two-line text of a passkey row: "создан …" and "последний вход …". */
export function passkeyRowText(
  entry: PasskeyEntry,
  nowSecs: number,
): { title: string; status: string } {
  return {
    title: browserLabel(entry.user_agent),
    status: `создан ${formatLastSeen(Number(entry.created_at), nowSecs)} · последний вход ${formatLastSeen(Number(entry.last_used_at), nowSecs)}`,
  };
}

/** Two-line text of a browser row: "этот браузер" for the current one, else "был …". */
export function browserRowText(
  entry: BrowserEntry,
  nowSecs: number,
): { title: string; status: string } {
  return {
    title: browserLabel(entry.user_agent),
    status: entry.current
      ? "этот браузер"
      : `был ${formatLastSeen(Number(entry.last_used_at), nowSecs)}`,
  };
}

/** The user dismissing the system passkey dialog -- not an error to show. */
export function isCancelError(err: unknown): boolean {
  const name = (err as { name?: unknown } | null)?.name;
  return name === "NotAllowedError" || name === "AbortError";
}

export type PasskeyFlow = "login" | "register";

/** Text shown next to the button when a passkey flow failed. */
export function passkeyFailText(flow: PasskeyFlow): string {
  return flow === "login" ? "Не удалось войти с passkey" : "Не удалось добавить passkey";
}

/** Whether a server `error` means "this browser's token was revoked": the
 * server answers `not authenticated` to owner requests of a deleted token,
 * and the client only sends those once it holds a saved token. */
export function isRevokedTokenError(message: string, savedToken: string | null): boolean {
  return message === "not authenticated" && savedToken !== null;
}

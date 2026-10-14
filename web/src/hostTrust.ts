// Remembers, per device, that the host requires a password (slice E2).
// Before this slice a signaling-server-in-the-middle that swallowed
// `auth_required` and sent an `offer` with `auth: null` bypassed the DTLS
// binding: the client verified the tag only if OPAQUE had succeeded in this
// very session. Now the client persists by `device_id` that the host once
// demanded a password and rejects any later `offer` that arrives without a
// completed login. Pure module, no DOM (same style as `qualityPreset.ts`).

/** localStorage key for the "host requires a password" flag of `deviceId`. */
export function authRequiredStorageKey(deviceId: string): string {
  return "rcdesk.authRequired." + deviceId;
}

/** `true` only when the flag was saved for `deviceId`; a missing key or a
 * throwing `storage` (e.g. Safari private browsing) yields `false`. */
export function loadAuthRequired(storage: Pick<Storage, "getItem">, deviceId: string): boolean {
  try {
    return storage.getItem(authRequiredStorageKey(deviceId)) === "1";
  } catch {
    return false;
  }
}

/** Saves the flag; silently does nothing if `storage` throws. */
export function saveAuthRequired(storage: Pick<Storage, "setItem">, deviceId: string): void {
  try {
    storage.setItem(authRequiredStorageKey(deviceId), "1");
  } catch {
    // Ignored: the flag simply won't survive a reload.
  }
}

/** Clears the flag (device forgotten); silently ignores a throwing `storage`. */
export function clearAuthRequired(storage: Pick<Storage, "removeItem">, deviceId: string): void {
  try {
    storage.removeItem(authRequiredStorageKey(deviceId));
  } catch {
    // Ignored, same reasoning as `saveAuthRequired`.
  }
}

/** `true` exactly when the host is remembered as requiring a password but
 * this session has no OPAQUE session key -- such an `offer` must be refused. */
export function offerRejectedByTrust(
  authRequiredRemembered: boolean,
  sessionKeyPresent: boolean,
): boolean {
  return authRequiredRemembered && !sessionKeyPresent;
}

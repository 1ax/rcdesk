// Pure functions backing the client's quality-preset picker (slice 3.5e):
// the localStorage key/parsing for the owner's per-device choice, and the
// Russian labels shown in the `<select>`. No DOM access here so this stays
// unit testable without a browser (see `displays.ts` for the same reasoning
// applied elsewhere in this codebase).

import type { QualityPreset } from "./generated/QualityPreset";

/** Russian labels for the `<select>`'s options, keyed by the wire value
 * (see `proto::control::QualityPreset`). */
export const QUALITY_PRESET_LABELS: Record<QualityPreset, string> = {
  auto: "Авто",
  sharp: "Чёткость",
  smooth: "Плавность",
};

/** localStorage key for the owner's chosen quality preset on device
 * `deviceId` -- one key per device (a per-device preference, unlike
 * `myDevices.ts`'s single, global `OWNER_TOKEN_KEY`). */
export function qualityPresetStorageKey(deviceId: string): string {
  return `rcdesk.quality-preset.${deviceId}`;
}

/** Parses a stored value back into a `QualityPreset`, falling back to
 * `"auto"` for anything else -- a missing key, a corrupted value, or a
 * preset name from a future version this client doesn't know. */
export function parseQualityPreset(value: string | null): QualityPreset {
  if (value === "sharp" || value === "smooth") return value;
  return "auto";
}

/** Reads the saved preset for `deviceId`, or `"auto"` if there is none, it
 * fails to parse, or `storage` throws (e.g. Safari private browsing throws
 * on any `localStorage` access, same reasoning as `myDevices.loadOwnerToken`).
 * `deviceId` of `null` (no persistent device id, e.g. a pre-3.5b host) never
 * has anything to read -- there is no key to look up, and nothing was ever
 * saved for it either (see `saveQualityPreset`). */
export function loadQualityPreset(
  storage: Pick<Storage, "getItem">,
  deviceId: string | null,
): QualityPreset {
  if (deviceId === null) return "auto";
  try {
    return parseQualityPreset(storage.getItem(qualityPresetStorageKey(deviceId)));
  } catch {
    return "auto";
  }
}

/** Saves `preset` for `deviceId`, silently doing nothing if `storage` throws
 * (same reasoning as `loadQualityPreset`) or `deviceId` is `null` (nothing to
 * key the entry by, so there's nothing to save). */
export function saveQualityPreset(
  storage: Pick<Storage, "setItem">,
  deviceId: string | null,
  preset: QualityPreset,
): void {
  if (deviceId === null) return;
  try {
    storage.setItem(qualityPresetStorageKey(deviceId), preset);
  } catch {
    // Ignored: e.g. Safari private browsing. The choice simply won't
    // survive a reload for this device.
  }
}

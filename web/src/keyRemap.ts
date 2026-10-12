// Pure functions backing the client's "Cmd как Ctrl" setting (slice 3.5f); 4.6e adds the mirrored "Ctrl как Cmd" mode:
// deciding whether it applies to this session, remapping a key `code`, and
// the localStorage load/save for the owner's choice. No DOM access here so
// this stays unit testable without a browser (see `displays.ts`/`fullscreen.ts`
// for the same reasoning applied elsewhere in this codebase).
//
// Why this exists: a Mac keyboard's Cmd key reports as `MetaLeft`/`MetaRight`
// (`KeyboardEvent.code`), which `host/src/input/keymap.rs` maps straight to
// the Windows key on a Windows host -- so Cmd+C/V/Z don't behave like their
// Mac equivalents, and a bare Cmd press opens the Start menu. Remapping
// `MetaLeft`/`MetaRight` to `ControlLeft`/`ControlRight` on the wire (only
// for a Mac client talking to a Windows host -- everywhere else the host
// already interprets the physical key correctly) fixes both.

import type { HostOs } from "./generated/HostOs";

/** The subset of `Navigator` this module needs to tell a Mac client apart
 * from any other -- a real `navigator` satisfies this structurally (see
 * `fullscreen.ts`'s `NavigatorWithKeyboard` for the same pattern applied to
 * a different not-yet-standard-in-lib.dom API). `userAgentData` (the
 * Client Hints replacement for `navigator.platform`) isn't in lib.dom
 * either, hence its own optional, narrowly-typed field here. */
export interface NavigatorLike {
  userAgentData?: { platform?: string };
  platform?: string;
  userAgent?: string;
}

/** Whether `nav` looks like a Mac (Safari or Chrome, on macOS -- not iOS/iPadOS
 * in their default UA, which report `iPhone`/`iPad`). Prefers
 * `userAgentData.platform` (Chrome/Edge), falling back to the older
 * `navigator.platform` (Safari, Firefox), and finally to a substring check on
 * `navigator.userAgent` if neither is available. */
export function isMacClient(nav: NavigatorLike): boolean {
  const uaDataPlatform = nav.userAgentData?.platform;
  if (typeof uaDataPlatform === "string" && uaDataPlatform.length > 0) {
    return /mac/i.test(uaDataPlatform);
  }
  if (typeof nav.platform === "string" && nav.platform.length > 0) {
    return /mac/i.test(nav.platform);
  }
  return /mac/i.test(nav.userAgent ?? "");
}

/** Whether the "Cmd как Ctrl" setting has any effect for this session: only
 * a Mac client talking to a Windows host needs it -- a Windows/Linux client
 * has no Cmd key to begin with, and a Mac host already expects Cmd. `hostOs`
 * is `null` before the host's `ControlMessage::HostInfo` has arrived
 * (`app.ts` hides the toggle until then). */
export function cmdAsCtrlApplies(clientIsMac: boolean, hostOs: HostOs | null): boolean {
  return clientIsMac && hostOs === "windows";
}

/** How modifier codes are rewritten on the wire (slice 4.6e generalises 3.5f):
 * - `none`: pass-through;
 * - `cmd-as-ctrl`: Mac client -> Windows host, Meta becomes Control;
 * - `ctrl-as-cmd`: non-Mac client -> Mac host, Control and Meta are SWAPPED
 *   (Ctrl acts as Cmd, the Win key acts as Ctrl, so a real Mac Ctrl stays
 *   reachable for Ctrl+Space, terminal Ctrl+C and the like). */
export type KeyRemapMode = "none" | "cmd-as-ctrl" | "ctrl-as-cmd";

/** Picks the remap mode for this session. `enabled` is the single shared
 * "substitute the modifier for the host OS" checkbox. */
export function keyRemapMode(
  clientIsMac: boolean,
  hostOs: HostOs | null,
  enabled: boolean,
): KeyRemapMode {
  if (!enabled) return "none";
  if (clientIsMac && hostOs === "windows") return "cmd-as-ctrl";
  if (!clientIsMac && hostOs === "macos") return "ctrl-as-cmd";
  return "none";
}

/** Remaps a `KeyboardEvent.code` for the wire according to `mode`, side
 * preserved; every other code passes through unchanged. Applied identically
 * to keydown and keyup (see `input.ts`'s `keyToMessage`) so a held key can't
 * desync into "Meta down, Ctrl up" on the host. */
export function remapCode(code: string, mode: KeyRemapMode): string {
  if (mode === "cmd-as-ctrl") {
    if (code === "MetaLeft") return "ControlLeft";
    if (code === "MetaRight") return "ControlRight";
  } else if (mode === "ctrl-as-cmd") {
    if (code === "ControlLeft") return "MetaLeft";
    if (code === "ControlRight") return "MetaRight";
    if (code === "MetaLeft") return "ControlLeft";
    if (code === "MetaRight") return "ControlRight";
  }
  return code;
}

/** localStorage key for the owner's "Cmd как Ctrl" choice -- global, unlike
 * `qualityPreset.ts`'s per-device key: the physical keyboard this affects
 * belongs to whichever Mac the owner is sitting at, not to the remote
 * device, so there's nothing meaningful to key it by. */
export const CMD_AS_CTRL_STORAGE_KEY = "rcdesk.cmd-as-ctrl";

/** Reads the saved setting, defaulting to `true` (on) when there's no stored
 * value yet, or `storage` throws (e.g. Safari private browsing -- same
 * reasoning as `myDevices.loadOwnerToken`/`qualityPreset.loadQualityPreset`). */
export function loadCmdAsCtrlSetting(storage: Pick<Storage, "getItem">): boolean {
  try {
    const value = storage.getItem(CMD_AS_CTRL_STORAGE_KEY);
    return value === null ? true : value === "1";
  } catch {
    return true;
  }
}

/** Saves `enabled`, silently doing nothing if `storage` throws (same
 * reasoning as `loadCmdAsCtrlSetting`). */
export function saveCmdAsCtrlSetting(storage: Pick<Storage, "setItem">, enabled: boolean): void {
  try {
    storage.setItem(CMD_AS_CTRL_STORAGE_KEY, enabled ? "1" : "0");
  } catch {
    // Ignored: e.g. Safari private browsing. The choice simply won't
    // survive a reload.
  }
}

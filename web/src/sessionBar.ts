// Pure logic backing the session screen's top bar (slice 3.5g): whether it's
// collapsed to a thin handle strip and whether the stats overlay's text is
// shown at all. No DOM access here -- same reasoning as
// `keyRemap.ts`/`fullscreen.ts`: `app.ts` is the only place that touches the
// actual elements (`#session-bar`, `#session-bar-handle`, `#stats-checkbox`,
// `#stats-overlay`).
//
// Why this exists: before this slice, `.controls` and `#stats-overlay` were
// absolutely positioned over the video, and `.controls` intercepted clicks --
// unreachable on a remote Windows window's close button or a Mac window's
// traffic-light buttons underneath it. The owner's decision was to make the
// bar a layout element above the video (never covering a pixel of the remote
// screen), collapsible to a thin strip that's itself a layout element, and to
// hide the stats text behind a checkbox -- both choices remembered across
// reloads.

/** localStorage key for whether the session bar is collapsed to its handle
 * strip -- global (not per-device), like `keyRemap.ts`'s
 * `CMD_AS_CTRL_STORAGE_KEY`: it's a property of the owner's screen/window,
 * not of whichever remote device they're connected to. */
export const SESSION_BAR_COLLAPSED_STORAGE_KEY = "rcdesk.session-bar-collapsed";

/** localStorage key for whether the stats overlay's text is shown -- global
 * for the same reason as `SESSION_BAR_COLLAPSED_STORAGE_KEY` above. */
export const STATS_VISIBLE_STORAGE_KEY = "rcdesk.stats-visible";

/** Reads the saved "session bar collapsed" choice, defaulting to `false`
 * (expanded) when there's no stored value yet, or `storage` throws (e.g.
 * Safari private browsing -- same reasoning as `keyRemap.loadCmdAsCtrlSetting`). */
export function loadSessionBarCollapsed(storage: Pick<Storage, "getItem">): boolean {
  try {
    return storage.getItem(SESSION_BAR_COLLAPSED_STORAGE_KEY) === "1";
  } catch {
    return false;
  }
}

/** Saves `collapsed`, silently doing nothing if `storage` throws (same
 * reasoning as `keyRemap.saveCmdAsCtrlSetting`). */
export function saveSessionBarCollapsed(
  storage: Pick<Storage, "setItem">,
  collapsed: boolean,
): void {
  try {
    storage.setItem(SESSION_BAR_COLLAPSED_STORAGE_KEY, collapsed ? "1" : "0");
  } catch {
    // Ignored: e.g. Safari private browsing. The choice simply won't
    // survive a reload.
  }
}

/** Reads the saved "show stats" choice, defaulting to `false` (hidden) when
 * there's no stored value yet, or `storage` throws. */
export function loadStatsVisible(storage: Pick<Storage, "getItem">): boolean {
  try {
    return storage.getItem(STATS_VISIBLE_STORAGE_KEY) === "1";
  } catch {
    return false;
  }
}

/** Saves `visible`, silently doing nothing if `storage` throws (same
 * reasoning as `saveSessionBarCollapsed`). */
export function saveStatsVisible(storage: Pick<Storage, "setItem">, visible: boolean): void {
  try {
    storage.setItem(STATS_VISIBLE_STORAGE_KEY, visible ? "1" : "0");
  } catch {
    // Ignored: e.g. Safari private browsing. The choice simply won't
    // survive a reload.
  }
}

/** `#bar-collapse-btn`/`#session-bar-handle`'s title/aria-label for the
 * current collapsed state. */
export function collapseButtonTitle(collapsed: boolean): string {
  return collapsed ? "Показать панель" : "Свернуть панель";
}

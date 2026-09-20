// Pure functions backing the client's "Мои компьютеры" device list (slice
// 3.1e): turning a `DeviceEntry` into display text, sorting the list for
// rendering, and reading/writing the owner token in `localStorage`. No DOM
// access here so this stays unit testable without a browser (see
// `displays.ts`/`inputStatus.ts` for the same reasoning applied elsewhere in
// this codebase).

import type { DeviceEntry } from "./generated/DeviceEntry";

/** localStorage key for the owner token (see `loadOwnerToken`/`saveOwnerToken`). */
const OWNER_TOKEN_KEY = "rcdesk.owner-token";

/** The label shown for a device: its owner-chosen `alias` when set (and not
 * blank after trimming), otherwise the name the host itself reported. */
export function deviceLabel(entry: DeviceEntry): string {
  const alias = entry.alias?.trim();
  return alias ? alias : entry.name;
}

/** Whether `entry` can be connected to right now without a PIN -- online and
 * not already busy with another client. */
export function canConnect(entry: DeviceEntry): boolean {
  return entry.online && !entry.busy;
}

/** Correctly declines a Russian noun after a count, choosing among the three
 * plural forms (e.g. `минуту`/`минуты`/`минут`) the way Russian grammar
 * requires -- ordinary `n === 1` English-style pluralization doesn't work
 * for this language (see the callers in `formatLastSeen`, tested directly in
 * `myDevices.test.ts` since the rule has real edge cases like 11-14). */
export function ruPlural(n: number, one: string, few: string, many: string): string {
  const abs = Math.abs(n);
  const mod10 = abs % 10;
  const mod100 = abs % 100;
  if (mod10 === 1 && mod100 !== 11) return one;
  if (mod10 >= 2 && mod10 <= 4 && (mod100 < 12 || mod100 > 14)) return few;
  return many;
}

/** Formats how long ago `lastSeenAtSecs` was, relative to `nowSecs` (both
 * Unix seconds) -- "только что" under a minute, then minutes/hours/days,
 * each correctly declined via `ruPlural`. */
export function formatLastSeen(lastSeenAtSecs: number, nowSecs: number): string {
  const diff = Math.max(0, nowSecs - lastSeenAtSecs);
  if (diff < 60) return "только что";
  if (diff < 3600) {
    const minutes = Math.floor(diff / 60);
    return `${minutes} ${ruPlural(minutes, "минуту", "минуты", "минут")} назад`;
  }
  if (diff < 86400) {
    const hours = Math.floor(diff / 3600);
    return `${hours} ${ruPlural(hours, "час", "часа", "часов")} назад`;
  }
  const days = Math.floor(diff / 86400);
  return `${days} ${ruPlural(days, "день", "дня", "дней")} назад`;
}

/** The status text shown next to a device in the list. `nowSecs` is Unix
 * seconds (injected rather than read from `Date.now()` here so this stays a
 * pure function of its inputs, same reasoning as the rest of this module). */
export function deviceStatusLabel(entry: DeviceEntry, nowSecs: number): string {
  if (entry.online) return entry.busy ? "Занят" : "В сети";
  return `Не в сети (${formatLastSeen(Number(entry.last_seen_at), nowSecs)})`;
}

/** Sorts devices for display: connectable ones first (online and not busy),
 * then the rest that are online (busy), then offline -- alphabetically by
 * `deviceLabel` within each group, using the Russian locale so it declines
 * accented/Cyrillic letters correctly. Does not mutate `devices`. */
export function sortDevices(devices: DeviceEntry[]): DeviceEntry[] {
  function rank(entry: DeviceEntry): number {
    if (canConnect(entry)) return 0;
    if (entry.online) return 1;
    return 2;
  }
  return [...devices].sort((a, b) => {
    const rankDiff = rank(a) - rank(b);
    if (rankDiff !== 0) return rankDiff;
    return deviceLabel(a).localeCompare(deviceLabel(b), "ru");
  });
}

/** Reads the saved owner token, or `null` if there is none or `storage`
 * throws (e.g. Safari private browsing throws on any `localStorage`
 * access). */
export function loadOwnerToken(storage: Pick<Storage, "getItem">): string | null {
  try {
    return storage.getItem(OWNER_TOKEN_KEY);
  } catch {
    return null;
  }
}

/** Saves the owner token, silently doing nothing if `storage` throws (same
 * reasoning as `loadOwnerToken`). */
export function saveOwnerToken(storage: Pick<Storage, "setItem">, token: string): void {
  try {
    storage.setItem(OWNER_TOKEN_KEY, token);
  } catch {
    // Ignored: e.g. Safari private browsing. The token simply won't
    // survive a reload; the user falls back to entering a PIN.
  }
}

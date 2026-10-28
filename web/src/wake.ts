// Pure logic for "Включить" (Wake-on-LAN, slice W): what the device list shows
// for an offline device and how the client tracks an in-flight wake. No DOM
// access, same reasoning as `myDevices.ts`; `app.ts` wires this to the UI.

import type { DeviceEntry } from "./generated/DeviceEntry";

/** How long the client waits for a woken device to come online. */
export const WAKE_TIMEOUT_MS = 180_000;

/** One device being woken: when the click happened. */
export interface Waking {
  startedMs: number;
}

export interface WakeView {
  showButton: boolean;
  buttonDisabled: boolean;
  buttonLabel: string;
  /** Replacement for the usual status text, or `null` to keep the usual one. */
  status: string | null;
}

/** `m:ss` for an elapsed time (negative clamps to 0). */
export function formatElapsed(ms: number): string {
  const total = Math.floor(Math.max(0, ms) / 1000);
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

const TIMEOUT_STATUS =
  "Не включился за 3 мин. Нужен кабель (не Wi-Fi) и Wake-on-LAN в BIOS и драйвере сетевой карты; Mac из выключенного состояния по сети не включается";

/** What the device row shows for `entry` given its wake state (if any). */
export function wakeView(
  entry: DeviceEntry,
  waking: Waking | undefined,
  nowMs: number,
): WakeView {
  if (entry.online) {
    return { showButton: false, buttonDisabled: false, buttonLabel: "Включить", status: null };
  }
  if (waking !== undefined) {
    const elapsed = nowMs - waking.startedMs;
    if (elapsed < WAKE_TIMEOUT_MS) {
      return {
        showButton: true,
        buttonDisabled: true,
        buttonLabel: "Включаю…",
        status: `Включаю… ${formatElapsed(elapsed)} (обычно 1–2 мин)`,
      };
    }
    return {
      showButton: entry.can_wake,
      buttonDisabled: false,
      buttonLabel: "Включить",
      status: TIMEOUT_STATUS,
    };
  }
  return {
    showButton: entry.can_wake,
    buttonDisabled: false,
    buttonLabel: "Включить",
    status: null,
  };
}

/** Text for a server error to a `wake_device`, or `null` when it is not ours
 * (or deliberately silent: `device online` -- the list refreshes itself). */
export function wakeErrorLabel(message: string): string | null {
  switch (message) {
    case "no waker online":
      return "Сейчас некому включить: нужен включённый компьютер с rcdesk в той же сети";
    case "too many attempts":
      return "Подождите несколько секунд и попробуйте снова";
    case "device not linked":
      return "Устройство не привязано";
    default:
      return null;
  }
}

/** Whether a server error message is one `wake_device` can produce. */
export function isWakeError(message: string): boolean {
  return message === "device online" || wakeErrorLabel(message) !== null;
}

/** Client-side wake bookkeeping for the whole device list. */
export interface WakeState {
  waiting: Map<string, Waking>;
  /** The device of a `wake_device` sent but not yet answered. */
  pendingId: string | null;
  /** Last error text per device, shown until the next click/refresh. */
  errors: Map<string, string>;
}

export function createWakeState(): WakeState {
  return { waiting: new Map(), pendingId: null, errors: new Map() };
}

/** Click on "Включить": begin waiting; the caller sends `wake_device`. */
export function startWake(state: WakeState, deviceId: string, nowMs: number): void {
  state.waiting.set(deviceId, { startedMs: nowMs });
  state.errors.delete(deviceId);
  state.pendingId = deviceId;
}

/** A new `devices` list: online devices are no longer being woken. */
export function applyDevices(state: WakeState, devices: DeviceEntry[]): void {
  for (const d of devices) {
    if (d.online) {
      state.waiting.delete(d.device_id);
      state.errors.delete(d.device_id);
      if (state.pendingId === d.device_id) state.pendingId = null;
    }
  }
}

/** `wake_sent`: the request was accepted; waiting stays. */
export function applyWakeSent(state: WakeState): void {
  state.pendingId = null;
}

/** A server `error`. Returns `true` when it was consumed as the answer to a
 * pending `wake_device` (the caller must then skip its session logic). */
export function applyWakeError(state: WakeState, message: string): boolean {
  if (state.pendingId === null || !isWakeError(message)) return false;
  const id = state.pendingId;
  state.pendingId = null;
  state.waiting.delete(id);
  const label = wakeErrorLabel(message);
  if (label !== null) state.errors.set(id, label);
  return true;
}

/** Status text for a row: a remembered error wins over the wake status. */
export function wakeStatusFor(
  state: WakeState,
  entry: DeviceEntry,
  nowMs: number,
): { view: WakeView; status: string | null } {
  const view = wakeView(entry, state.waiting.get(entry.device_id), nowMs);
  const err = entry.online ? undefined : state.errors.get(entry.device_id);
  return { view, status: err ?? view.status };
}

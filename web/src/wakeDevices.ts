// Pure logic for the "add device by MAC" / "add router-waker" forms (D84/D85).
// No DOM access; `app.ts` wires this to the UI.

import type { WakeNetwork } from "./generated/WakeNetwork";

/** Text for a server error that answers `add_manual_device` /
 * `add_router_waker`, or `null` when it is not one of ours. */
export function addDeviceErrorLabel(message: string): string | null {
  switch (message) {
    case "invalid name":
      return "Укажите имя";
    case "invalid mac":
      return "MAC в виде aa:bb:cc:dd:ee:ff";
    case "unknown network":
      return "Сеть не найдена — обновите страницу";
    case "duplicate mac":
      return "Устройство с таким MAC уже есть";
    case "too many devices":
      return "Достигнут предел — 32 устройства";
    default:
      return null;
  }
}

/** Option caption for the network `<select>`. */
export function networkLabel(n: WakeNetwork): string {
  return `${n.network}/${n.prefix} — ${n.label}`;
}

/** `<option value>` for a network: `network/prefix`. */
export function networkValue(n: WakeNetwork): string {
  return `${n.network}/${n.prefix}`;
}

/** Finds the network a `<select>` value refers to. */
export function findNetwork(networks: WakeNetwork[], value: string): WakeNetwork | undefined {
  return networks.find((n) => networkValue(n) === value);
}

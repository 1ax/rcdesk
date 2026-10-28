// Pure helpers behind the "Ctrl+Alt+Del" session-bar button (slice D46b).
// The host advertises `ctrl_alt_del_available: true` only for a Windows host
// in full mode; an older host omits the field entirely, so anything other
// than a literal `true` keeps the button hidden. No DOM access here, same
// reasoning as `inputBlocked.ts`.

import type { ControlMessage } from "./generated/ControlMessage";

/** Whether the button is shown for a `host_info` payload's flag
 * (`undefined` = old host without the field). */
export function ctrlAltDelVisible(available: boolean | undefined): boolean {
  return available === true;
}

/** The control-channel message the button sends. */
export function ctrlAltDelMessage(): ControlMessage {
  return { type: "send_ctrl_alt_del" };
}

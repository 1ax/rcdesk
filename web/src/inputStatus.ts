// Pure function backing the client's view-only indicator (slice 2.5a,
// closing debt D26): turns the host's `ControlMessage::InputStatus` payload
// into the label shown next to the session status, or `null` when input is
// available and nothing needs to be shown. No DOM access here so this stays
// unit testable without a browser (see `displays.ts` for the same reasoning
// applied elsewhere in this codebase).

/** The two fields of `ControlMessage::InputStatus` that `viewOnlyLabel`
 * needs -- an inline shape (rather than importing the generated
 * `ControlMessage` union) so this stays a plain, easily testable function of
 * its inputs. */
export interface InputStatus {
  available: boolean;
  reason: string | null;
}

/** Returns the view-only banner text for `msg`, or `null` when input is
 * available and no banner should be shown. */
export function viewOnlyLabel(msg: InputStatus): string | null {
  if (msg.available) return null;
  return msg.reason ? `Только просмотр: ${msg.reason}` : "Только просмотр";
}

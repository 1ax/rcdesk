// Pure function backing the client's "admin window" warning banner (slice
// 2.6e): turns the host's `ControlMessage::InputBlocked` payload into the
// banner text, or `null` when nothing should be shown. Unlike
// `inputStatus.ts`'s `viewOnlyLabel`, `blocked: true` here does *not* mean
// input stops flowing -- UIPI silently drops it on the host's side
// regardless of what the client does, so the client keeps sending
// clicks/keys as usual (the user must still be able to click away from the
// blocking window) and only shows this banner. See
// `proto::control::ControlMessage::InputBlocked`'s doc comment for the full
// distinction from `InputStatus`. No DOM access here, same reasoning as
// `inputStatus.ts`/`displays.ts`.

/** The two fields of `ControlMessage::InputBlocked` that `inputBlockedLabel`
 * needs -- an inline shape (rather than importing the generated
 * `ControlMessage` union), same reasoning as `inputStatus.ts`'s
 * `InputStatus`. */
export interface InputBlocked {
  blocked: boolean;
  reason: string | null;
}

/** Returns the warning banner text for `msg`, or `null` when
 * `blocked: false` and nothing should be shown. */
export function inputBlockedLabel(msg: InputBlocked): string | null {
  if (!msg.blocked) return null;
  const base = "Admin window: input is blocked by Windows";
  return msg.reason ? `${base} (${msg.reason})` : base;
}

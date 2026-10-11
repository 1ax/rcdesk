// Pure state logic behind the "Шторка" (privacy curtain) button (slice
// 4.2c3). No DOM access here so it stays unit testable without a browser
// (same approach as `inputStatus.ts`); `app.ts` renders `curtainView`.

export interface CurtainState {
  /** `host_info.curtain_available`; false until `host_info` arrives. */
  available: boolean;
  /** Last `curtain_status.on` (the host starts every session without it). */
  on: boolean;
  /** A `set_curtain` was sent and no `curtain_status` came back yet. */
  pending: boolean;
  /** `curtain_status.reason` of the last answer, if any. */
  reason: string | null;
}

export interface CurtainView {
  hidden: boolean;
  disabled: boolean;
  pressed: boolean;
  text: string;
  /** Text for `#curtain-status`, or `null` to hide it. */
  statusText: string | null;
}

export function initialCurtainState(): CurtainState {
  return { available: false, on: false, pending: false, reason: null };
}

export function curtainOnHostInfo(s: CurtainState, available: boolean): CurtainState {
  return { ...s, available };
}

/** A click: returns the new state and the `on` value to send, or `null`
 * when nothing should be sent (unavailable, or an answer is still pending). */
export function curtainOnClick(s: CurtainState): { state: CurtainState; send: boolean | null } {
  if (!s.available || s.pending) return { state: s, send: null };
  return { state: { ...s, pending: true }, send: !s.on };
}

export function curtainOnStatus(
  s: CurtainState,
  msg: { on: boolean; reason: string | null },
): CurtainState {
  return { ...s, on: msg.on, pending: false, reason: msg.reason };
}

export function curtainView(s: CurtainState): CurtainView {
  return {
    hidden: !s.available,
    disabled: s.pending,
    pressed: s.on,
    text: s.on ? "Шторка вкл" : "Шторка",
    statusText: s.reason ? `Шторка: ${s.reason}` : null,
  };
}

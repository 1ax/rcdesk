// Pure decision logic backing the client's automatic session recovery
// (slice 3.5b): what a WebRTC `connectionState` change means for the
// session (mirrors `host/src/signaling/mod.rs`'s `DisconnectGrace` -- see
// its doc comment for why `disconnected` alone must not end the session),
// the reconnect-attempt backoff schedule, whether a lost session is worth
// auto-recovering at all, and the label shown on the video overlay (D32).
// No DOM/timer access here, same reasoning as `displays.ts`/`reconnectBackoff.ts`:
// `app.ts` is the only place that touches an actual `setTimeout`/`RTCPeerConnection`.

/** How long a `disconnected` `RTCPeerConnectionState` is tolerated before the
 * session is treated as lost -- matches `host/src/signaling/mod.rs`'s
 * `DISCONNECT_GRACE`. ICE can flap through `disconnected` on a brief network
 * hiccup and recover to `connected` on its own; only `failed`/`closed`, or
 * this window expiring without a recovery, end the session. */
export const DISCONNECT_GRACE_MS = 15000;

/** Delay before each automatic reconnect attempt (1-based: `attempt` 1 is
 * the first try after the session is lost), and how many attempts are made
 * in total before giving up. */
export const RECOVERY_DELAYS_MS = [1000, 2000, 4000, 8000, 16000];
export const MAX_RECOVERY_ATTEMPTS = RECOVERY_DELAYS_MS.length;

/** Delay before recovery attempt number `attempt`. Values outside
 * `1..=MAX_RECOVERY_ATTEMPTS` are clamped -- there is no attempt below 1,
 * and nothing schedules an attempt past `MAX_RECOVERY_ATTEMPTS` (see
 * `recoveryExhausted`), but clamping keeps this total instead of throwing. */
export function recoveryDelayMs(attempt: number): number {
  const n = Math.min(Math.max(1, Math.floor(attempt)), MAX_RECOVERY_ATTEMPTS);
  return RECOVERY_DELAYS_MS[n - 1];
}

/** Whether attempt number `attempt` is past the last one that should run --
 * `app.ts` calls this right after incrementing its attempt counter to
 * decide between scheduling that attempt and giving up. */
export function recoveryExhausted(attempt: number): boolean {
  return attempt > MAX_RECOVERY_ATTEMPTS;
}

/** Whether a lost session is worth automatically reconnecting at all --
 * only when the persistent device id from `Joined.device_id` (slice 3.5b)
 * is known, i.e. there is something to send `connect_device` for. `null`
 * covers both "no `joined` yet" and a signaling server old enough to predate
 * 3.5b (`Joined.device_id` is `#[serde(default)]`). */
export function shouldAttemptRecovery(deviceId: string | null): boolean {
  return deviceId !== null;
}

/** The four ways `handle_session_event`/`app.ts` can decide a `PeerSession`
 * has reached its end for a given `RTCPeerConnectionState` change, given
 * whether a `DISCONNECT_GRACE_MS` window is already running:
 * - `"connected"`: the connection is (still, or newly) up -- cancel any
 *   grace window and report the session connected.
 * - `"start-disconnect-grace"`: a *fresh* `disconnected` -- arm the window.
 * - `"already-disconnecting"`: `disconnected` again while a window is
 *   already running -- nothing to (re)arm.
 * - `"session-lost"`: `failed`/`closed` -- end the session right away, no
 *   grace period (mirrors the host's `ConnectionState` handling).
 * - `"ignore"`: any other transient state (`new`, `connecting`).
 */
export type ConnectionStateOutcome =
  | "connected"
  | "start-disconnect-grace"
  | "already-disconnecting"
  | "session-lost"
  | "ignore";

export function connectionStateOutcome(
  state: RTCPeerConnectionState,
  graceWindowActive: boolean,
): ConnectionStateOutcome {
  if (state === "connected") return "connected";
  if (state === "disconnected") {
    return graceWindowActive ? "already-disconnecting" : "start-disconnect-grace";
  }
  if (state === "failed" || state === "closed") return "session-lost";
  return "ignore";
}

/** What the D32 video-overlay banner should say right now, as one of:
 * - `{ kind: "connecting" }`: from `joined` until the first video frame.
 * - `{ kind: "disconnecting" }`: a `DISCONNECT_GRACE_MS` window is running.
 * - `{ kind: "reconnecting", attempt }`: an automatic recovery attempt is in
 *   flight or about to be (1-based `attempt`, out of `MAX_RECOVERY_ATTEMPTS`).
 * - `{ kind: "streaming" }`: video is flowing, nothing to show. */
export type ConnectionBannerPhase =
  | { kind: "connecting" }
  | { kind: "disconnecting" }
  | { kind: "reconnecting"; attempt: number }
  | { kind: "streaming" };

/** The banner text for `phase`, or `null` for `"streaming"` (hide it). */
export function connectionBannerLabel(phase: ConnectionBannerPhase): string | null {
  switch (phase.kind) {
    case "connecting":
      return "Подключение к хосту…";
    case "disconnecting":
      return "Связь прерывается…";
    case "reconnecting":
      return `Переподключение… (${phase.attempt} из ${MAX_RECOVERY_ATTEMPTS})`;
    case "streaming":
      return null;
  }
}

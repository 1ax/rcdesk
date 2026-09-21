// Pure backoff-delay function for the signaling client's automatic
// reconnect after losing the WebSocket (slice 3.5a, see `app.ts`'s
// `scheduleReconnect`). No DOM/timer access here, same reasoning as
// `displays.ts`/`inputStatus.ts`: easy to unit test, and `app.ts` is the only
// place that needs to touch an actual `setTimeout`.

/** Delay before reconnect attempt number `attempt` (1-based: the first retry
 * after a drop is attempt 1). Doubles from 1s, capped at 30s: 1, 2, 4, 8, 16,
 * 30, 30, ... Values below 1 are treated as 1 -- there is no "attempt 0". */
export function reconnectBackoffMs(attempt: number): number {
  const n = Math.max(1, Math.floor(attempt));
  return Math.min(1000 * 2 ** (n - 1), 30000);
}

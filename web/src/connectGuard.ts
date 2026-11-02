/** Tracks one outstanding `join`/`connect_device` request and validates the
 * `joined` that answers it (audit W-1, W-5). Pure, no DOM. */
export type JoinedVerdict = "accept" | "mismatch" | "unexpected";

export class ConnectGuard {
  private waiting = false;
  private expected: string | null = null;

  /** A `joined` is now awaited. `null` = PIN join (device unknown upfront). */
  request(expectedDeviceId: string | null): void {
    this.waiting = true;
    this.expected = expectedDeviceId;
  }

  /** The user gave up (or the request failed): no `joined` is awaited. */
  cancel(): void {
    this.waiting = false;
    this.expected = null;
  }

  /** Judges an incoming `joined`; always consumes the request. */
  onJoined(deviceId: string | null): JoinedVerdict {
    const wasWaiting = this.waiting;
    const expected = this.expected;
    this.cancel();
    if (!wasWaiting) return "unexpected";
    if (expected !== null && expected !== deviceId) return "mismatch";
    return "accept";
  }

  get pending(): boolean {
    return this.waiting;
  }
}

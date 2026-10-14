// Screen Wake Lock for the session (slice H4a): keeps a phone/tablet screen
// from dimming while a remote desktop is being watched. Pure logic, no DOM --
// `app.ts` passes `navigator` and a visibility probe.

/** The sentinel returned by `navigator.wakeLock.request`. */
export interface WakeLockSentinelLike {
  release(): Promise<void>;
  addEventListener?: (type: "release", listener: () => void) => void;
}

/** Narrow shape of `navigator.wakeLock`. */
export interface WakeLockLike {
  request(type: "screen"): Promise<WakeLockSentinelLike>;
}

export interface NavigatorWithWakeLock {
  wakeLock?: WakeLockLike;
}

export class ScreenWakeLock {
  private wanted = false;
  private sentinel: WakeLockSentinelLike | null = null;
  private requesting = false;

  private readonly nav: NavigatorWithWakeLock;
  private readonly isVisible: () => boolean;

  constructor(nav: NavigatorWithWakeLock, isVisible: () => boolean) {
    this.nav = nav;
    this.isVisible = isVisible;
  }

  enable(): void {
    this.wanted = true;
    this.acquire();
  }

  disable(): void {
    this.wanted = false;
    const s = this.sentinel;
    this.sentinel = null;
    if (s) s.release().catch(() => {});
  }

  /** The browser drops the lock when the tab goes to background; re-take it on return. */
  onVisibilityChange(): void {
    if (this.wanted && this.isVisible()) this.acquire();
  }

  private acquire(): void {
    const wl = this.nav.wakeLock;
    if (!this.wanted || !wl || !this.isVisible() || this.sentinel || this.requesting) return;
    this.requesting = true;
    wl.request("screen").then(
      (s) => {
        this.requesting = false;
        if (!this.wanted) {
          s.release().catch(() => {});
          return;
        }
        this.sentinel = s;
        s.addEventListener?.("release", () => {
          if (this.sentinel === s) this.sentinel = null;
        });
      },
      (e: unknown) => {
        this.requesting = false;
        // Refusal (e.g. battery saver) is normal.
        console.debug("wake lock request refused", e);
      },
    );
  }
}

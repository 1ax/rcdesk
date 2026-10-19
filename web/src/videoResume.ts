// D67a: the OS sleeping/locking (Windows especially) makes the browser pause
// the remote `<video>` while frames keep arriving and decoding -- the picture
// just freezes. This un-pauses it for as long as a session is live. Pure
// logic, no globals -- `app.ts` passes the element, a session probe and a clock.

/** Narrow shape of the `<video>` element this needs. */
export interface ResumableVideo {
  paused: boolean;
  srcObject: unknown;
  play(): Promise<void>;
  addEventListener(type: "pause", listener: () => void): void;
  removeEventListener(type: "pause", listener: () => void): void;
}

/** Narrow shape of `document`. */
export interface VisibilityDoc {
  visibilityState: string;
  addEventListener(type: "visibilitychange", listener: () => void): void;
  removeEventListener(type: "visibilitychange", listener: () => void): void;
}

/** Minimum gap between two resume attempts, ms. */
export const RESUME_THROTTLE_MS = 1000;

export class VideoResume {
  private lastAttempt = Number.NEGATIVE_INFINITY;

  private readonly video: ResumableVideo;
  private readonly isActive: () => boolean;
  private readonly now: () => number;

  constructor(video: ResumableVideo, isActive: () => boolean, now: () => number) {
    this.video = video;
    this.isActive = isActive;
    this.now = now;
  }

  check(reason: string): void {
    const video = this.video;
    if (!this.isActive() || !video.srcObject || !video.paused) return;
    const t = this.now();
    if (t - this.lastAttempt < RESUME_THROTTLE_MS) return;
    this.lastAttempt = t;
    const stream = video.srcObject;
    console.info("[rcdesk] video paused, resuming", { reason, step: 1 });
    this.play().then((ok) => {
      if (ok && !video.paused) return;
      if (!this.isActive() || video.srcObject !== stream) return;
      console.info("[rcdesk] video paused, resuming", { reason, step: 2 });
      video.srcObject = null;
      video.srcObject = stream;
      void this.play();
    });
  }

  /** Resolves true when `play()` fulfilled, false when it threw or rejected. Never rejects. */
  private play(): Promise<boolean> {
    try {
      return this.video.play().then(
        () => true,
        () => false,
      );
    } catch {
      return Promise.resolve(false);
    }
  }

  attach(doc: VisibilityDoc): () => void {
    const onPause = () => this.check("pause");
    const onVisible = () => {
      if (doc.visibilityState === "visible") this.check("visible");
    };
    this.video.addEventListener("pause", onPause);
    doc.addEventListener("visibilitychange", onVisible);
    return () => {
      this.video.removeEventListener("pause", onPause);
      doc.removeEventListener("visibilitychange", onVisible);
    };
  }
}

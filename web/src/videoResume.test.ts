import { afterEach, describe, expect, it, vi } from "vitest";
import { VideoResume } from "./videoResume";

const flush = () => new Promise((r) => setTimeout(r, 0));

function makeVideo(opts: { paused?: boolean; srcObject?: unknown } = {}) {
  const listeners = new Map<string, Set<() => void>>();
  const assigned: unknown[] = [];
  let src: unknown = "srcObject" in opts ? opts.srcObject : { id: "stream" };
  const video = {
    paused: opts.paused ?? true,
    get srcObject() {
      return src;
    },
    set srcObject(v: unknown) {
      assigned.push(v);
      src = v;
    },
    play: vi.fn(async () => {}),
    addEventListener: (t: "pause", l: () => void) => {
      if (!listeners.has(t)) listeners.set(t, new Set());
      listeners.get(t)!.add(l);
    },
    removeEventListener: (t: "pause", l: () => void) => {
      listeners.get(t)?.delete(l);
    },
  };
  return { video, listeners, assigned, fire: (t: string) => listeners.get(t)?.forEach((l) => l()) };
}

function makeDoc() {
  const listeners = new Set<() => void>();
  const doc = {
    visibilityState: "visible",
    addEventListener: (_t: "visibilitychange", l: () => void) => listeners.add(l),
    removeEventListener: (_t: "visibilitychange", l: () => void) => listeners.delete(l),
  };
  return { doc, listeners, fire: () => listeners.forEach((l) => l()) };
}

afterEach(() => vi.restoreAllMocks());

describe("VideoResume", () => {
  it("pause during an active session with a stream calls play()", () => {
    vi.spyOn(console, "info").mockImplementation(() => {});
    const v = makeVideo();
    const d = makeDoc();
    new VideoResume(v.video, () => true, () => 0).attach(d.doc);
    v.fire("pause");
    expect(v.video.play).toHaveBeenCalledTimes(1);
  });

  it("does nothing without a session, without srcObject, or when not paused", () => {
    vi.spyOn(console, "info").mockImplementation(() => {});
    const a = makeVideo();
    new VideoResume(a.video, () => false, () => 0).check("x");
    expect(a.video.play).not.toHaveBeenCalled();
    const b = makeVideo({ srcObject: null });
    new VideoResume(b.video, () => true, () => 0).check("x");
    expect(b.video.play).not.toHaveBeenCalled();
    const c = makeVideo({ paused: false });
    new VideoResume(c.video, () => true, () => 0).check("x");
    expect(c.video.play).not.toHaveBeenCalled();
  });

  it("visibilitychange: visible resumes, hidden does not", () => {
    vi.spyOn(console, "info").mockImplementation(() => {});
    const v = makeVideo();
    const d = makeDoc();
    let t = 0;
    new VideoResume(v.video, () => true, () => (t += 5000)).attach(d.doc);
    d.doc.visibilityState = "hidden";
    d.fire();
    expect(v.video.play).not.toHaveBeenCalled();
    d.doc.visibilityState = "visible";
    d.fire();
    expect(v.video.play).toHaveBeenCalledTimes(1);
  });

  it("throttles attempts to one per 1000 ms", () => {
    vi.spyOn(console, "info").mockImplementation(() => {});
    const v = makeVideo();
    let t = 10_000;
    const r = new VideoResume(v.video, () => true, () => t);
    r.check("a");
    t += 999;
    r.check("b");
    expect(v.video.play).toHaveBeenCalledTimes(1);
    t += 1;
    r.check("c");
    expect(v.video.play).toHaveBeenCalledTimes(2);
  });

  it("play() resolving while still paused reconnects srcObject and plays again", async () => {
    vi.spyOn(console, "info").mockImplementation(() => {});
    const v = makeVideo();
    const stream = v.video.srcObject;
    new VideoResume(v.video, () => true, () => 0).check("x");
    await flush();
    expect(v.assigned).toEqual([null, stream]);
    expect(v.video.play).toHaveBeenCalledTimes(2);
  });

  it("does not reconnect when play() resolved and the video is running", async () => {
    vi.spyOn(console, "info").mockImplementation(() => {});
    const v = makeVideo();
    v.video.play.mockImplementation(async () => {
      v.video.paused = false;
    });
    new VideoResume(v.video, () => true, () => 0).check("x");
    await flush();
    expect(v.assigned).toEqual([]);
    expect(v.video.play).toHaveBeenCalledTimes(1);
  });

  it("play() rejecting reconnects and plays again; second reject is swallowed (vitest fails on unhandled rejections)", async () => {
    vi.spyOn(console, "info").mockImplementation(() => {});
    const v = makeVideo();
    const stream = v.video.srcObject;
    v.video.play.mockImplementation(async () => {
      throw new Error("NotAllowedError");
    });
    new VideoResume(v.video, () => true, () => 0).check("x");
    await flush();
    await flush();
    expect(v.assigned).toEqual([null, stream]);
    expect(v.video.play).toHaveBeenCalledTimes(2);
  });

  it("no reconnect if the session ended before step 2", async () => {
    vi.spyOn(console, "info").mockImplementation(() => {});
    const v = makeVideo();
    let active = true;
    new VideoResume(v.video, () => active, () => 0).check("x");
    active = false;
    await flush();
    expect(v.assigned).toEqual([]);
    expect(v.video.play).toHaveBeenCalledTimes(1);
  });

  it("the detach function removes both listeners", () => {
    const v = makeVideo();
    const d = makeDoc();
    const detach = new VideoResume(v.video, () => true, () => 0).attach(d.doc);
    expect(v.listeners.get("pause")?.size).toBe(1);
    expect(d.listeners.size).toBe(1);
    detach();
    expect(v.listeners.get("pause")?.size).toBe(0);
    expect(d.listeners.size).toBe(0);
  });
});

import { describe, expect, it } from "vitest";
import { summarizeStats, takeSnapshot } from "./stats";
import type { RTCStatsLike } from "./stats";

const CODEC: RTCStatsLike = {
  id: "codec-1",
  type: "codec",
  mimeType: "video/H264",
};

function inboundRtp(overrides: Partial<RTCStatsLike> = {}): RTCStatsLike {
  return {
    id: "inbound-1",
    type: "inbound-rtp",
    kind: "video",
    codecId: "codec-1",
    frameWidth: 1920,
    frameHeight: 1080,
    packetsLost: 0,
    jitter: 0.003,
    bytesReceived: 100_000,
    framesDecoded: 300,
    ...overrides,
  };
}

function candidatePair(overrides: Partial<RTCStatsLike> = {}): RTCStatsLike {
  return {
    id: "pair-1",
    type: "candidate-pair",
    state: "succeeded",
    nominated: true,
    currentRoundTripTime: 0.003,
    ...overrides,
  };
}

describe("summarizeStats", () => {
  it("computes kbps and fps from the delta between two snapshots", () => {
    const t0 = 1_000;
    const first = [inboundRtp({ bytesReceived: 100_000, framesDecoded: 300 }), CODEC, candidatePair()];
    const snapshot = takeSnapshot(first, t0);

    const t1 = t0 + 1000; // 1 second later
    const second = [
      inboundRtp({ bytesReceived: 100_000 + 125_000, framesDecoded: 300 + 30 }),
      CODEC,
      candidatePair(),
    ];

    const summary = summarizeStats(second, snapshot, t1);

    // 125_000 bytes/s * 8 / 1000 = 1000 kbit/s
    expect(summary.kbps).toBeCloseTo(1000, 5);
    expect(summary.fps).toBeCloseTo(30, 5);
  });

  it("prefers the reported framesPerSecond over the computed delta when present", () => {
    const report = [inboundRtp({ framesPerSecond: 59.9 }), CODEC, candidatePair()];
    const summary = summarizeStats(report, undefined, 2000);
    expect(summary.fps).toBe(59.9);
  });

  it("reports rtt in milliseconds from the nominated/succeeded candidate pair", () => {
    const report = [inboundRtp(), CODEC, candidatePair({ currentRoundTripTime: 0.0125 })];
    const summary = summarizeStats(report, undefined, 1000);
    expect(summary.rttMs).toBeCloseTo(12.5, 5);
  });

  it("ignores a candidate pair that is neither nominated nor succeeded", () => {
    const report = [
      inboundRtp(),
      CODEC,
      candidatePair({ nominated: false, state: "waiting", currentRoundTripTime: 0.05 }),
    ];
    const summary = summarizeStats(report, undefined, 1000);
    expect(summary.rttMs).toBeUndefined();
  });

  it("resolves codec mimeType via the inbound-rtp's codecId", () => {
    const report = [inboundRtp(), CODEC, candidatePair()];
    const summary = summarizeStats(report, undefined, 1000);
    expect(summary.codec).toBe("video/H264");
  });

  it("reports jitter in milliseconds and passes through packetsLost/resolution", () => {
    const report = [
      inboundRtp({ jitter: 0.004, packetsLost: 2, frameWidth: 1280, frameHeight: 720 }),
      CODEC,
      candidatePair(),
    ];
    const summary = summarizeStats(report, undefined, 1000);
    expect(summary.jitterMs).toBeCloseTo(4, 5);
    expect(summary.packetsLost).toBe(2);
    expect(summary.width).toBe(1280);
    expect(summary.height).toBe(720);
  });

  it("returns undefined fields, without throwing, when the report is empty", () => {
    expect(() => summarizeStats([], undefined, 1000)).not.toThrow();
    const summary = summarizeStats([], undefined, 1000);
    expect(summary).toEqual({
      width: undefined,
      height: undefined,
      packetsLost: undefined,
      jitterMs: undefined,
      framesDecoded: undefined,
      codec: undefined,
      rttMs: undefined,
      fps: undefined,
      kbps: undefined,
    });
  });

  it("does not compute fps/kbps deltas when no previous snapshot is given", () => {
    const report = [inboundRtp({ framesPerSecond: undefined }), CODEC, candidatePair()];
    const summary = summarizeStats(report, undefined, 1000);
    expect(summary.fps).toBeUndefined();
    expect(summary.kbps).toBeUndefined();
  });
});

describe("takeSnapshot", () => {
  it("extracts bytesReceived/framesDecoded/timestampMs from the inbound-rtp entry", () => {
    const report = [inboundRtp({ bytesReceived: 42, framesDecoded: 7 }), CODEC, candidatePair()];
    const snapshot = takeSnapshot(report, 5000);
    expect(snapshot).toEqual({ timestampMs: 5000, bytesReceived: 42, framesDecoded: 7 });
  });

  it("returns an otherwise-empty snapshot when there is no inbound-rtp video entry", () => {
    const snapshot = takeSnapshot([CODEC], 5000);
    expect(snapshot).toEqual({ timestampMs: 5000, bytesReceived: undefined, framesDecoded: undefined });
  });
});

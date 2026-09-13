// Pure functions for summarizing `RTCPeerConnection.getStats()` output into
// the numbers the session overlay shows (fps, bitrate, resolution, RTT,
// packet loss, jitter, codec). No DOM access here so this stays unit
// testable without a browser.

/** The subset of `RTCStats` fields these functions read, across the
 * `inbound-rtp` (video), `candidate-pair` and `codec` stat types. Loosely
 * typed (rather than the real `RTCInboundRtpStreamStats` etc. unions)
 * because a real `RTCStatsReport` yields a broader variety of stat shapes
 * than any single interface captures cleanly, and tests build fixtures by
 * hand. */
export interface RTCStatsLike {
  id?: string;
  type: string;
  kind?: string;
  framesPerSecond?: number;
  framesDecoded?: number;
  bytesReceived?: number;
  frameWidth?: number;
  frameHeight?: number;
  packetsLost?: number;
  jitter?: number;
  nominated?: boolean;
  state?: string;
  currentRoundTripTime?: number;
  mimeType?: string;
  codecId?: string;
  /** Cumulative seconds all emitted frames spent in the jitter buffer
   * (`RTCInboundRtpStreamStats.jitterBufferDelay`). Paired with
   * `jitterBufferEmittedCount` to compute an average per-frame delay over
   * an interval -- see `jitterBufferMs` below. */
  jitterBufferDelay?: number;
  /** Cumulative count of frames emitted from the jitter buffer
   * (`RTCInboundRtpStreamStats.jitterBufferEmittedCount`). */
  jitterBufferEmittedCount?: number;
}

/** The minimal state carried from one `summarizeStats` call to the next, so
 * fps/kbps can be computed from deltas when the browser doesn't report
 * `framesPerSecond` directly. Build one with `takeSnapshot` after each call
 * and pass it back in as `prev` on the next. */
export interface Snapshot {
  timestampMs: number;
  bytesReceived?: number;
  framesDecoded?: number;
  jitterBufferDelay?: number;
  jitterBufferEmittedCount?: number;
}

export interface StatsSummary {
  fps?: number;
  kbps?: number;
  width?: number;
  height?: number;
  packetsLost?: number;
  jitterMs?: number;
  /** Average time each frame spent in the client's jitter buffer since the
   * previous `summarizeStats` call, in milliseconds -- distinct from
   * `jitterMs` (the RTP-level packet arrival jitter estimate). Computed
   * from the delta of two cumulative counters
   * (`jitterBufferDelay`/`jitterBufferEmittedCount`), so it needs `prev`;
   * `undefined` on the first call or when the browser doesn't report those
   * fields. */
  jitterBufferMs?: number;
  rttMs?: number;
  codec?: string;
  framesDecoded?: number;
}

function findInboundVideo(entries: RTCStatsLike[]): RTCStatsLike | undefined {
  return entries.find((e) => e.type === "inbound-rtp" && e.kind === "video");
}

/** Extracts the `{ timestampMs, bytesReceived, framesDecoded }` snapshot from
 * a stats report, to pass as `prev` on the next `summarizeStats` call. */
export function takeSnapshot(report: Iterable<RTCStatsLike>, nowMs: number): Snapshot {
  const inbound = findInboundVideo(Array.from(report));
  return {
    timestampMs: nowMs,
    bytesReceived: inbound?.bytesReceived,
    framesDecoded: inbound?.framesDecoded,
    jitterBufferDelay: inbound?.jitterBufferDelay,
    jitterBufferEmittedCount: inbound?.jitterBufferEmittedCount,
  };
}

/** Summarizes one `getStats()` report. `prev`/`nowMs` let fps and kbps be
 * computed as deltas over wall-clock time when the report has no
 * `framesPerSecond` field (not all browsers populate it). Missing fields
 * anywhere in the report simply leave the corresponding summary field
 * `undefined` -- never throws. */
export function summarizeStats(
  report: Iterable<RTCStatsLike>,
  prev: Snapshot | undefined,
  nowMs: number,
): StatsSummary {
  const entries = Array.from(report);
  const inbound = findInboundVideo(entries);
  const pair = entries.find(
    (e) => e.type === "candidate-pair" && (e.nominated === true || e.state === "succeeded"),
  );
  const codecEntry = inbound?.codecId
    ? entries.find((e) => e.type === "codec" && e.id === inbound.codecId)
    : entries.find((e) => e.type === "codec");

  const summary: StatsSummary = {
    width: inbound?.frameWidth,
    height: inbound?.frameHeight,
    packetsLost: inbound?.packetsLost,
    jitterMs: inbound?.jitter !== undefined ? inbound.jitter * 1000 : undefined,
    framesDecoded: inbound?.framesDecoded,
    codec: codecEntry?.mimeType,
    rttMs:
      pair?.currentRoundTripTime !== undefined ? pair.currentRoundTripTime * 1000 : undefined,
  };

  const elapsedSec = prev !== undefined ? (nowMs - prev.timestampMs) / 1000 : undefined;

  if (inbound?.framesPerSecond !== undefined) {
    summary.fps = inbound.framesPerSecond;
  } else if (
    elapsedSec !== undefined &&
    elapsedSec > 0 &&
    prev?.framesDecoded !== undefined &&
    inbound?.framesDecoded !== undefined
  ) {
    summary.fps = (inbound.framesDecoded - prev.framesDecoded) / elapsedSec;
  }

  if (
    elapsedSec !== undefined &&
    elapsedSec > 0 &&
    prev?.bytesReceived !== undefined &&
    inbound?.bytesReceived !== undefined
  ) {
    const deltaBytes = inbound.bytesReceived - prev.bytesReceived;
    summary.kbps = (deltaBytes * 8) / 1000 / elapsedSec;
  }

  if (
    prev?.jitterBufferDelay !== undefined &&
    prev.jitterBufferEmittedCount !== undefined &&
    inbound?.jitterBufferDelay !== undefined &&
    inbound?.jitterBufferEmittedCount !== undefined
  ) {
    const deltaCount = inbound.jitterBufferEmittedCount - prev.jitterBufferEmittedCount;
    if (deltaCount > 0) {
      const deltaDelay = inbound.jitterBufferDelay - prev.jitterBufferDelay;
      summary.jitterBufferMs = (deltaDelay / deltaCount) * 1000;
    }
  }

  return summary;
}

// Client -> host file upload over the `file` data channel (slice 3.4b); see
// `proto::file` for the wire contract (JSON text `start`/`end`/`abort`/
// `saved`, raw binary chunks between `start` and `end`, one transfer per
// direction at a time).
//
// No DOM access here: the channel is injected through `FileChannelLike`, so
// this stays unit testable under vitest's `node` environment (same approach
// as `clipboard.ts`). `app.ts` wires the real channel and the UI.

import type { AbortReason } from "./generated/AbortReason";
import type { FileMessage } from "./generated/FileMessage";

/** Upper bound for one chunk (the protocol's 64 KiB maximum). */
export const MAX_CHUNK_BYTES = 64 * 1024;
/** `bufferedAmountLowThreshold` the sender sets on the channel. */
export const BUFFER_LOW_BYTES = 256 * 1024;
/** Above this much queued data the sender waits for `bufferedamountlow`. */
export const BUFFER_HIGH_BYTES = 1024 * 1024;

/** Chunk size for a peer connection whose SCTP `maxMessageSize` is
 * `maxMessageSize` (`pc.sctp?.maxMessageSize`): `min(64 KiB, max)` when the
 * value is known and positive, else 64 KiB. */
export function chooseChunkSize(maxMessageSize: number | null | undefined): number {
  if (
    typeof maxMessageSize === "number" &&
    Number.isFinite(maxMessageSize) &&
    maxMessageSize > 0
  ) {
    return Math.min(MAX_CHUNK_BYTES, Math.floor(maxMessageSize));
  }
  return MAX_CHUNK_BYTES;
}

/** The subset of `RTCDataChannel` the sender uses. */
export interface FileChannelLike {
  readonly readyState: string;
  readonly bufferedAmount: number;
  bufferedAmountLowThreshold: number;
  send(data: string | ArrayBuffer): void;
  addEventListener(type: string, listener: (event: Event) => void): void;
  removeEventListener(type: string, listener: (event: Event) => void): void;
}

/** The subset of `File` the sender uses. */
export interface FileLike {
  readonly name: string;
  readonly size: number;
  slice(start: number, end: number): { arrayBuffer(): Promise<ArrayBuffer> };
}

/** Why a file failed: a host `abort` reason, or the channel closed. */
export type FileFailReason = AbortReason | "closed";

const REASON_LABELS: Record<FileFailReason, string> = {
  cancelled: "отменено",
  disabled: "передача файлов отключена на хосте",
  io: "ошибка записи на хосте",
  protocol: "нарушение протокола",
  closed: "соединение закрыто",
};

/** Russian text for `reason`, shown in the "not sent" note. */
export function failReasonLabel(reason: FileFailReason): string {
  return REASON_LABELS[reason];
}

export interface FileProgress {
  name: string;
  sent: number;
  size: number;
  /** 1-based position of this file in the current batch. */
  index: number;
  /** Files in the current batch (done + current + queued). */
  total: number;
}

export interface FileSenderCallbacks {
  onProgress?: (progress: FileProgress) => void;
  /** The host saved the file; `savedName` is its final name there. */
  onSaved?: (savedName: string) => void;
  onError?: (name: string, reason: FileFailReason) => void;
  /** The queue ran dry (after completion, cancel, or channel close). */
  onDone?: () => void;
}

interface Current {
  id: number;
  outcome: "saved" | "aborted" | "cancelled" | null;
}

/** Sends queued files one at a time over a `file` channel. */
export class FileSender {
  private readonly channel: FileChannelLike;
  private readonly chunkSize: number;
  private readonly cb: FileSenderCallbacks;
  private queue: FileLike[] = [];
  private current: Current | null = null;
  private running = false;
  private closed = false;
  private nextId = 1;
  private total = 0;
  private done = 0;
  private waiters: Array<() => void> = [];

  constructor(channel: FileChannelLike, chunkSize: number, callbacks: FileSenderCallbacks = {}) {
    this.channel = channel;
    this.chunkSize = chunkSize;
    this.cb = callbacks;
    channel.bufferedAmountLowThreshold = BUFFER_LOW_BYTES;
    channel.addEventListener("bufferedamountlow", this.onLow);
    channel.addEventListener("message", this.onMessage);
    channel.addEventListener("close", this.onClose);
    if (channel.readyState === "closing" || channel.readyState === "closed") this.closed = true;
  }

  /** Queues `files`; starts sending if idle. */
  enqueue(files: FileLike[]): void {
    if (files.length === 0) return;
    if (this.closed) {
      for (const f of files) this.cb.onError?.(f.name, "closed");
      return;
    }
    this.queue.push(...files);
    this.total += files.length;
    if (!this.running) void this.run();
  }

  /** Aborts the current transfer (telling the host) and drops the queue. */
  cancelAll(): void {
    this.total -= this.queue.length;
    this.queue = [];
    const cur = this.current;
    if (cur && cur.outcome === null) {
      cur.outcome = "cancelled";
      if (!this.closed) this.sendJson({ type: "abort", id: cur.id, reason: "cancelled" });
    }
    this.wake();
  }

  /** Detaches from the channel and drops everything without sending. */
  dispose(): void {
    this.shutdown();
    this.channel.removeEventListener("bufferedamountlow", this.onLow);
    this.channel.removeEventListener("message", this.onMessage);
    this.channel.removeEventListener("close", this.onClose);
  }

  private readonly onLow = (): void => {
    this.wake();
  };

  private readonly onClose = (): void => {
    this.shutdown();
  };

  private shutdown(): void {
    this.closed = true;
    this.total -= this.queue.length;
    this.queue = [];
    this.wake();
  }

  private readonly onMessage = (event: Event): void => {
    const data = (event as MessageEvent<unknown>).data;
    if (typeof data !== "string") return;
    let msg: FileMessage;
    try {
      msg = JSON.parse(data) as FileMessage;
    } catch {
      return;
    }
    const cur = this.current;
    if (!cur || cur.outcome !== null) return;
    if (msg.type === "saved" && msg.id === cur.id) {
      cur.outcome = "saved";
      this.cb.onSaved?.(msg.name);
      this.wake();
    } else if (msg.type === "abort" && msg.id === cur.id) {
      cur.outcome = "aborted";
      if (msg.reason === "disabled") {
        this.total -= this.queue.length;
        this.queue = [];
      }
      this.failReason = msg.reason;
      this.wake();
    }
  };

  private failReason: AbortReason = "protocol";

  private sendJson(msg: FileMessage): void {
    this.channel.send(JSON.stringify(msg));
  }

  private wake(): void {
    const ws = this.waiters;
    this.waiters = [];
    for (const w of ws) w();
  }

  private waitWake(): Promise<void> {
    return new Promise((resolve) => this.waiters.push(resolve));
  }

  private async run(): Promise<void> {
    this.running = true;
    try {
      while (this.queue.length > 0 && !this.closed) {
        const file = this.queue.shift()!;
        await this.transfer(file);
        this.done += 1;
      }
    } finally {
      this.running = false;
      this.current = null;
      this.total = 0;
      this.done = 0;
      this.cb.onDone?.();
    }
  }

  private progress(file: FileLike, sent: number): void {
    this.cb.onProgress?.({
      name: file.name,
      sent,
      size: file.size,
      index: this.done + 1,
      total: this.total,
    });
  }

  private async transfer(file: FileLike): Promise<void> {
    const cur: Current = { id: this.nextId++, outcome: null };
    this.current = cur;
    try {
      this.sendJson({ type: "start", id: cur.id, name: file.name, size: file.size });
      this.progress(file, 0);
      let offset = 0;
      while (offset < file.size) {
        while (
          this.channel.bufferedAmount > BUFFER_HIGH_BYTES &&
          cur.outcome === null &&
          !this.closed
        ) {
          await this.waitWake();
        }
        if (cur.outcome !== null || this.closed) break;
        const end = Math.min(offset + this.chunkSize, file.size);
        const buf = await file.slice(offset, end).arrayBuffer();
        // The transfer may have been aborted/closed while reading.
        if (cur.outcome !== null || this.closed) break;
        this.channel.send(buf);
        offset = end;
        this.progress(file, offset);
      }
      if (cur.outcome === null && !this.closed) {
        this.sendJson({ type: "end", id: cur.id });
        while (cur.outcome === null && !this.closed) await this.waitWake();
      }
      if (cur.outcome === "aborted") this.cb.onError?.(file.name, this.failReason);
    } catch {
      // `send` throws if the channel closed under us.
      if (cur.outcome === null) {
        cur.outcome = "aborted";
        this.cb.onError?.(file.name, "closed");
      }
    }
  }
}

/** Minimal shape of a `DataTransferItem`. */
export interface DropItemLike {
  readonly kind: string;
  webkitGetAsEntry?(): { isDirectory: boolean } | null;
  getAsFile(): File | null;
}

/** Minimal shape of a `DataTransfer`. */
export interface DataTransferLike {
  readonly items?: ArrayLike<DropItemLike>;
  readonly files: ArrayLike<File>;
}

/** Picks the plain files out of a drop: folders are skipped and counted.
 * Must be called synchronously in the `drop` handler (Safari clears the
 * `DataTransfer` afterwards). */
export function pickDroppedFiles(dt: DataTransferLike): { files: File[]; folders: number } {
  const files: File[] = [];
  let folders = 0;
  if (dt.items && dt.items.length > 0) {
    for (let i = 0; i < dt.items.length; i++) {
      const item = dt.items[i]!;
      if (item.kind !== "file") continue;
      if (item.webkitGetAsEntry?.()?.isDirectory) {
        folders += 1;
        continue;
      }
      const f = item.getAsFile();
      if (f) files.push(f);
    }
  } else {
    for (let i = 0; i < dt.files.length; i++) files.push(dt.files[i]!);
  }
  return { files, folders };
}

/** Whether a drag carries files (as opposed to text/links). */
export function dragHasFiles(dt: { types?: ArrayLike<string> } | null): boolean {
  if (!dt?.types) return false;
  return Array.from(dt.types).includes("Files");
}

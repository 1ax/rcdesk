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
import type { OfferedFile } from "./generated/OfferedFile";

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
  /** Set to `"arraybuffer"` by `FileReceiver` (browser default is `"blob"`). */
  binaryType?: string;
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

/** Host-originated transfer ids start here (`proto::file::HOST_ID_BASE`);
 * the client's own uploads count from 1, so ids never clash. */
export const HOST_ID_BASE = 0x8000_0000;

/** Largest total size of one download (it is assembled in browser memory). */
export const MAX_DOWNLOAD_BYTES = 2 * 1024 * 1024 * 1024;

/** Why a download failed: a host `abort` reason, a local check, or close. */
export type DownloadFailReason = AbortReason | "closed" | "too_large";

const DOWNLOAD_REASON_LABELS: Record<DownloadFailReason, string> = {
  cancelled: "отменено",
  disabled: "передача файлов отключена на хосте",
  io: "ошибка чтения файла на хосте",
  protocol: "нарушение протокола",
  closed: "соединение закрыто",
  too_large: "слишком большой объём (лимит 2 ГиБ)",
};

/** Russian text for `reason`, shown in the "not downloaded" note. */
export function downloadFailLabel(reason: DownloadFailReason): string {
  return DOWNLOAD_REASON_LABELS[reason];
}

export interface FileReceiverCallbacks {
  /** The host offers `files` (empty with no folders: the offer is gone). */
  onOffer?: (files: OfferedFile[], skippedFolders: number) => void;
  onProgress?: (progress: FileProgress) => void;
  /** A file arrived completely. */
  onFile?: (name: string, blob: Blob) => void;
  /** A file failed (`name` is empty for a whole-download problem). */
  onError?: (name: string, reason: DownloadFailReason) => void;
  /** The download ended (all files, cancel, or channel close). */
  onDone?: () => void;
}

interface Incoming {
  id: number;
  name: string;
  size: number;
  received: number;
  chunks: ArrayBuffer[];
}

/** Receives files the host offers (host -> client). Only reacts to
 * `offer` and to `start`/`end`/`abort` with host-range ids, so it does not
 * mix up the replies a `FileSender` on the same channel handles. */
export class FileReceiver {
  private readonly channel: FileChannelLike;
  private readonly cb: FileReceiverCallbacks;
  private offer: { listId: number; files: OfferedFile[] } | null = null;
  /** The list being downloaded, or null. */
  private list: { total: number; done: number } | null = null;
  private current: Incoming | null = null;
  private lastId = HOST_ID_BASE;
  /** After `cancel`, everything the host still sends is dropped until the
   * next `fetch`. */
  private cancelled = false;

  constructor(channel: FileChannelLike, callbacks: FileReceiverCallbacks = {}) {
    this.channel = channel;
    this.cb = callbacks;
    channel.binaryType = "arraybuffer";
    channel.addEventListener("message", this.onMessage);
    channel.addEventListener("close", this.onClose);
  }

  /** Asks the host for all files of the last offer. Returns false when
   * nothing was requested (no offer, already downloading, too large). */
  fetch(): boolean {
    const offer = this.offer;
    if (!offer || offer.files.length === 0 || this.list) return false;
    const total = offer.files.reduce((sum, f) => sum + f.size, 0);
    if (total > MAX_DOWNLOAD_BYTES) {
      this.cb.onError?.("", "too_large");
      return false;
    }
    this.cancelled = false;
    try {
      this.sendJson({ type: "fetch", list_id: offer.listId });
    } catch {
      this.cb.onError?.("", "closed");
      return false;
    }
    this.list = { total: offer.files.length, done: 0 };
    return true;
  }

  /** Aborts the current file (telling the host), which also ends the rest
   * of the list. */
  cancel(): void {
    if (!this.list) return;
    const id = this.current?.id ?? this.lastId;
    try {
      this.sendJson({ type: "abort", id, reason: "cancelled" });
    } catch {
      // Channel already gone.
    }
    this.cancelled = true;
    this.finish();
  }

  /** Detaches from the channel and drops everything without sending. */
  dispose(): void {
    this.channel.removeEventListener("message", this.onMessage);
    this.channel.removeEventListener("close", this.onClose);
    this.current = null;
    this.list = null;
    this.offer = null;
  }

  private sendJson(msg: FileMessage): void {
    this.channel.send(JSON.stringify(msg));
  }

  private readonly onClose = (): void => {
    const cur = this.current;
    if (cur) this.cb.onError?.(cur.name, "closed");
    this.offer = null;
    this.finish();
  };

  private finish(): void {
    const had = this.list !== null;
    this.current = null;
    this.list = null;
    if (had) this.cb.onDone?.();
  }

  private progress(cur: Incoming): void {
    this.cb.onProgress?.({
      name: cur.name,
      sent: cur.received,
      size: cur.size,
      index: (this.list?.done ?? 0) + 1,
      total: this.list?.total ?? 1,
    });
  }

  /** A file of the list is over (saved or failed). */
  private fileOver(): void {
    this.current = null;
    if (!this.list) return;
    this.list.done += 1;
    if (this.list.done >= this.list.total) this.finish();
  }

  private readonly onMessage = (event: Event): void => {
    const data = (event as MessageEvent<unknown>).data;
    if (typeof data !== "string") {
      if (data instanceof ArrayBuffer) this.onChunk(data);
      return;
    }
    let msg: FileMessage;
    try {
      msg = JSON.parse(data) as FileMessage;
    } catch {
      return;
    }
    switch (msg.type) {
      case "offer":
        this.offer =
          msg.files.length === 0 && msg.skipped_folders === 0
            ? null
            : { listId: msg.list_id, files: msg.files };
        this.cb.onOffer?.(msg.files, msg.skipped_folders);
        return;
      case "start":
        if (msg.id >= HOST_ID_BASE) this.onStart(msg.id, msg.name, msg.size);
        return;
      case "end":
        if (msg.id >= HOST_ID_BASE) this.onEnd(msg.id);
        return;
      case "abort":
        if (msg.id >= HOST_ID_BASE) this.onHostAbort(msg.id, msg.reason);
        return;
      default:
        return;
    }
  };

  private onStart(id: number, name: string, size: number): void {
    if (!this.list || this.cancelled) return;
    this.lastId = id;
    this.current = { id, name, size, received: 0, chunks: [] };
    this.progress(this.current);
  }

  private onChunk(buf: ArrayBuffer): void {
    const cur = this.current;
    if (!cur || this.cancelled) return;
    cur.received += buf.byteLength;
    if (cur.received > cur.size) {
      this.protocolError(cur);
      return;
    }
    cur.chunks.push(buf);
    this.progress(cur);
  }

  private onEnd(id: number): void {
    const cur = this.current;
    if (!cur || cur.id !== id || this.cancelled) return;
    if (cur.received !== cur.size) {
      this.protocolError(cur);
      return;
    }
    const blob = new Blob(cur.chunks);
    const name = cur.name;
    this.fileOver();
    this.cb.onFile?.(name, blob);
  }

  private onHostAbort(id: number, reason: AbortReason): void {
    const cur = this.current;
    if (!cur || cur.id !== id || this.cancelled) return;
    this.cb.onError?.(cur.name, reason);
    this.fileOver();
  }

  /** The host sent a wrong amount of data: tell it, drop the file and the
   * rest of the list (the host stops on `protocol`). */
  private protocolError(cur: Incoming): void {
    try {
      this.sendJson({ type: "abort", id: cur.id, reason: "protocol" });
    } catch {
      // Channel already gone.
    }
    this.cb.onError?.(cur.name, "protocol");
    this.cancelled = true;
    this.finish();
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

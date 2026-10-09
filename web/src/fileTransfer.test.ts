import { describe, expect, it } from "vitest";
import {
  BUFFER_HIGH_BYTES,
  BUFFER_LOW_BYTES,
  FileReceiver,
  FileSender,
  HOST_ID_BASE,
  MAX_DOWNLOAD_BYTES,
  chooseChunkSize,
  downloadFailLabel,
  dragHasFiles,
  failReasonLabel,
  pickDroppedFiles,
  type DataTransferLike,
  type FileChannelLike,
  type DownloadFailReason,
  type FileFailReason,
  type FileLike,
} from "./fileTransfer";

class FakeChannel implements FileChannelLike {
  readyState = "open";
  bufferedAmount = 0;
  bufferedAmountLowThreshold = 0;
  sent: Array<string | ArrayBuffer> = [];
  private listeners = new Map<string, Set<(e: Event) => void>>();
  send(data: string | ArrayBuffer): void {
    this.sent.push(data);
  }
  addEventListener(type: string, l: (e: Event) => void): void {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type)!.add(l);
  }
  removeEventListener(type: string, l: (e: Event) => void): void {
    this.listeners.get(type)?.delete(l);
  }
  emit(type: string, extra: object = {}): void {
    for (const l of [...(this.listeners.get(type) ?? [])]) l({ type, ...extra } as Event);
  }
  hostSays(msg: object): void {
    this.emit("message", { data: JSON.stringify(msg) });
  }
  /** Sent messages as JSON for text, byte length for binary. */
  trace(): Array<object | number> {
    return this.sent.map((m) => (typeof m === "string" ? (JSON.parse(m) as object) : m.byteLength));
  }
}

function fakeFile(name: string, size: number): FileLike {
  return {
    name,
    size,
    slice: (start, end) => ({
      arrayBuffer: async () => new ArrayBuffer(Math.min(end, size) - start),
    }),
  };
}

const flush = async (): Promise<void> => {
  for (let i = 0; i < 20; i++) await Promise.resolve();
};

interface Events {
  saved: string[];
  errors: Array<[string, FileFailReason]>;
  progress: string[];
  done: number;
}

function setup(chunk = 4) {
  const ch = new FakeChannel();
  const ev: Events = { saved: [], errors: [], progress: [], done: 0 };
  const sender = new FileSender(ch, chunk, {
    onSaved: (n) => ev.saved.push(n),
    onError: (n, r) => ev.errors.push([n, r]),
    onProgress: (p) => ev.progress.push(`${p.name} ${p.sent}/${p.size} ${p.index}/${p.total}`),
    onDone: () => {
      ev.done += 1;
    },
  });
  return { ch, ev, sender };
}

describe("chooseChunkSize", () => {
  it("uses 64 KiB unless the SCTP limit is smaller", () => {
    expect(chooseChunkSize(undefined)).toBe(65536);
    expect(chooseChunkSize(null)).toBe(65536);
    expect(chooseChunkSize(0)).toBe(65536);
    expect(chooseChunkSize(Infinity)).toBe(65536);
    expect(chooseChunkSize(262144)).toBe(65536);
    expect(chooseChunkSize(16384)).toBe(16384);
  });
});

describe("FileSender", () => {
  it("sets the low-water threshold", () => {
    const { ch } = setup();
    expect(ch.bufferedAmountLowThreshold).toBe(BUFFER_LOW_BYTES);
  });

  it("sends start, exact chunks, end", async () => {
    const { ch, ev, sender } = setup(4);
    sender.enqueue([fakeFile("a.bin", 10)]);
    await flush();
    expect(ch.trace()).toEqual([
      { type: "start", id: 1, name: "a.bin", size: 10 },
      4,
      4,
      2,
      { type: "end", id: 1 },
    ]);
    ch.hostSays({ type: "saved", id: 1, name: "a (1).bin" });
    await flush();
    expect(ev.saved).toEqual(["a (1).bin"]);
    expect(ev.done).toBe(1);
    expect(ev.progress).toEqual([
      "a.bin 0/10 1/1",
      "a.bin 4/10 1/1",
      "a.bin 8/10 1/1",
      "a.bin 10/10 1/1",
    ]);
  });

  it("handles an empty file and an exact multiple of the chunk", async () => {
    const { ch, sender } = setup(4);
    sender.enqueue([fakeFile("empty", 0)]);
    await flush();
    expect(ch.trace()).toEqual([
      { type: "start", id: 1, name: "empty", size: 0 },
      { type: "end", id: 1 },
    ]);
    ch.hostSays({ type: "saved", id: 1, name: "empty" });
    sender.enqueue([fakeFile("eight", 8)]);
    await flush();
    expect(ch.trace().slice(2)).toEqual([
      { type: "start", id: 2, name: "eight", size: 8 },
      4,
      4,
      { type: "end", id: 2 },
    ]);
  });

  it("waits for bufferedamountlow above the high-water mark", async () => {
    const { ch, sender } = setup(4);
    ch.bufferedAmount = BUFFER_HIGH_BYTES + 1;
    sender.enqueue([fakeFile("a", 8)]);
    await flush();
    expect(ch.trace()).toEqual([{ type: "start", id: 1, name: "a", size: 8 }]);
    ch.bufferedAmount = 0;
    ch.emit("bufferedamountlow");
    await flush();
    expect(ch.trace()).toEqual([
      { type: "start", id: 1, name: "a", size: 8 },
      4,
      4,
      { type: "end", id: 1 },
    ]);
  });

  it("starts the next file only after saved", async () => {
    const { ch, sender } = setup(4);
    sender.enqueue([fakeFile("a", 4), fakeFile("b", 4)]);
    await flush();
    expect(ch.trace()).toEqual([
      { type: "start", id: 1, name: "a", size: 4 },
      4,
      { type: "end", id: 1 },
    ]);
    ch.hostSays({ type: "saved", id: 1, name: "a" });
    await flush();
    expect(ch.trace().slice(3)).toEqual([
      { type: "start", id: 2, name: "b", size: 4 },
      4,
      { type: "end", id: 2 },
    ]);
  });

  it("ignores saved/abort with a foreign id", async () => {
    const { ch, ev, sender } = setup(4);
    sender.enqueue([fakeFile("a", 4), fakeFile("b", 4)]);
    await flush();
    ch.hostSays({ type: "saved", id: 99, name: "x" });
    ch.hostSays({ type: "abort", id: 99, reason: "io" });
    await flush();
    expect(ch.sent.length).toBe(3);
    expect(ev.saved).toEqual([]);
  });

  it("cancelAll aborts the current file and clears the queue", async () => {
    const { ch, ev, sender } = setup(4);
    ch.bufferedAmount = BUFFER_HIGH_BYTES + 1;
    sender.enqueue([fakeFile("a", 8), fakeFile("b", 4)]);
    await flush();
    sender.cancelAll();
    await flush();
    expect(ch.trace()).toEqual([
      { type: "start", id: 1, name: "a", size: 8 },
      { type: "abort", id: 1, reason: "cancelled" },
    ]);
    ch.bufferedAmount = 0;
    ch.emit("bufferedamountlow");
    await flush();
    expect(ch.sent.length).toBe(2);
    expect(ev.errors).toEqual([]);
    expect(ev.done).toBe(1);
  });

  it("cancelAll after end was sent still aborts", async () => {
    const { ch, sender } = setup(4);
    sender.enqueue([fakeFile("a", 4), fakeFile("b", 4)]);
    await flush();
    sender.cancelAll();
    await flush();
    expect(ch.trace().at(-1)).toEqual({ type: "abort", id: 1, reason: "cancelled" });
    expect(ch.sent.length).toBe(4);
  });

  it("moves to the next file after a host abort", async () => {
    const { ch, ev, sender } = setup(4);
    sender.enqueue([fakeFile("a", 4), fakeFile("b", 4)]);
    await flush();
    ch.hostSays({ type: "abort", id: 1, reason: "io" });
    await flush();
    expect(ev.errors).toEqual([["a", "io"]]);
    expect(ch.trace().slice(3)).toEqual([
      { type: "start", id: 2, name: "b", size: 4 },
      4,
      { type: "end", id: 2 },
    ]);
  });

  it("stops sending chunks after a host abort mid-file", async () => {
    const { ch, ev, sender } = setup(4);
    ch.bufferedAmount = BUFFER_HIGH_BYTES + 1;
    sender.enqueue([fakeFile("a", 8)]);
    await flush();
    ch.hostSays({ type: "abort", id: 1, reason: "protocol" });
    await flush();
    expect(ch.sent.length).toBe(1);
    expect(ev.errors).toEqual([["a", "protocol"]]);
    expect(ev.done).toBe(1);
  });

  it("clears the whole queue on abort disabled", async () => {
    const { ch, ev, sender } = setup(4);
    sender.enqueue([fakeFile("a", 4), fakeFile("b", 4), fakeFile("c", 4)]);
    await flush();
    ch.hostSays({ type: "abort", id: 1, reason: "disabled" });
    await flush();
    expect(ev.errors).toEqual([["a", "disabled"]]);
    expect(ch.sent.length).toBe(3);
    expect(ev.done).toBe(1);
  });

  it("drops the queue when the channel closes, sending nothing", async () => {
    const { ch, ev, sender } = setup(4);
    sender.enqueue([fakeFile("a", 4), fakeFile("b", 4)]);
    await flush();
    ch.emit("close");
    await flush();
    expect(ch.sent.length).toBe(3);
    expect(ev.done).toBe(1);
    sender.enqueue([fakeFile("c", 1)]);
    expect(ev.errors).toEqual([["c", "closed"]]);
    expect(ch.sent.length).toBe(3);
  });

  it("numbers ids increasingly across batches", async () => {
    const { ch, sender } = setup(4);
    sender.enqueue([fakeFile("a", 0)]);
    await flush();
    ch.hostSays({ type: "saved", id: 1, name: "a" });
    await flush();
    sender.enqueue([fakeFile("b", 0)]);
    await flush();
    expect(ch.trace()[2]).toEqual({ type: "start", id: 2, name: "b", size: 0 });
  });
});

describe("failReasonLabel", () => {
  it("has Russian text for every reason", () => {
    for (const r of ["cancelled", "disabled", "io", "protocol", "closed"] as const) {
      expect(failReasonLabel(r).length).toBeGreaterThan(0);
    }
  });
});

describe("pickDroppedFiles", () => {
  const file = (name: string) => ({ name }) as unknown as File;
  const item = (f: File | null, dir: boolean | null, kind = "file") => ({
    kind,
    getAsFile: () => f,
    webkitGetAsEntry: () => (dir === null ? null : { isDirectory: dir }),
  });

  it("keeps files and counts folders", () => {
    const a = file("a");
    const b = file("b");
    const dt: DataTransferLike = {
      items: [item(a, false), item(null, true), item(b, null), item(null, null, "string")],
      files: [],
    };
    expect(pickDroppedFiles(dt)).toEqual({ files: [a, b], folders: 1 });
  });

  it("falls back to files when items are unavailable", () => {
    const a = file("a");
    expect(pickDroppedFiles({ files: [a] })).toEqual({ files: [a], folders: 0 });
  });

  it("detects file drags", () => {
    expect(dragHasFiles({ types: ["Files"] })).toBe(true);
    expect(dragHasFiles({ types: ["text/plain"] })).toBe(false);
    expect(dragHasFiles(null)).toBe(false);
  });
});

describe("FileReceiver", () => {
  const H = HOST_ID_BASE;

  function setupRecv() {
    const ch = new FakeChannel() as FakeChannel & { binaryType?: string };
    const ev = {
      offers: [] as Array<[Array<{ name: string; size: number }>, number]>,
      files: [] as Array<{ name: string; blob: Blob }>,
      errors: [] as Array<[string, DownloadFailReason]>,
      progress: [] as string[],
      done: 0,
    };
    const rx = new FileReceiver(ch, {
      onOffer: (files, skipped) => ev.offers.push([files, skipped]),
      onFile: (name, blob) => ev.files.push({ name, blob }),
      onError: (name, reason) => ev.errors.push([name, reason]),
      onProgress: (p) => ev.progress.push(`${p.name} ${p.sent}/${p.size} ${p.index}/${p.total}`),
      onDone: () => ev.done++,
    });
    return { ch, ev, rx };
  }

  const bin = (...bytes: number[]): ArrayBuffer => new Uint8Array(bytes).buffer;
  const hostChunk = (ch: FakeChannel, data: ArrayBuffer): void =>
    ch.emit("message", { data });
  const offerMsg = (listId: number, files: Array<[string, number]>, skipped = 0) => ({
    type: "offer",
    list_id: listId,
    files: files.map(([name, size]) => ({ name, size })),
    skipped_folders: skipped,
  });

  it("asks for arraybuffer chunks and reports an offer", () => {
    const { ch, ev } = setupRecv();
    expect(ch.binaryType).toBe("arraybuffer");
    ch.hostSays(offerMsg(3, [["a.txt", 5]], 2));
    expect(ev.offers).toEqual([[[{ name: "a.txt", size: 5 }], 2]]);
  });

  it("fetch sends the list id of the last offer", () => {
    const { ch, rx } = setupRecv();
    expect(rx.fetch()).toBe(false);
    ch.hostSays(offerMsg(3, [["a.txt", 5]]));
    ch.hostSays(offerMsg(4, [["b.txt", 1]]));
    expect(rx.fetch()).toBe(true);
    expect(ch.trace()).toEqual([{ type: "fetch", list_id: 4 }]);
    // A second fetch while downloading is ignored.
    expect(rx.fetch()).toBe(false);
    expect(ch.trace().length).toBe(1);
  });

  it("an empty offer withdraws the previous one", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(3, [["a.txt", 5]]));
    ch.hostSays(offerMsg(4, []));
    expect(ev.offers[1]).toEqual([[], 0]);
    expect(rx.fetch()).toBe(false);
    expect(ch.trace()).toEqual([]);
  });

  it("refuses a download above the limit", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a", MAX_DOWNLOAD_BYTES], ["b", 1]]));
    expect(rx.fetch()).toBe(false);
    expect(ch.trace()).toEqual([]);
    expect(ev.errors).toEqual([["", "too_large"]]);
    ch.hostSays(offerMsg(2, [["a", MAX_DOWNLOAD_BYTES]]));
    expect(rx.fetch()).toBe(true);
  });

  it("assembles files into blobs with the right size and content", async () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a.bin", 5], ["b.txt", 0]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a.bin", size: 5 });
    hostChunk(ch, bin(1, 2, 3));
    hostChunk(ch, bin(4, 5));
    ch.hostSays({ type: "end", id: H });
    ch.hostSays({ type: "start", id: H + 1, name: "b.txt", size: 0 });
    ch.hostSays({ type: "end", id: H + 1 });
    expect(ev.files.map((f) => [f.name, f.blob.size])).toEqual([
      ["a.bin", 5],
      ["b.txt", 0],
    ]);
    expect(new Uint8Array(await ev.files[0]!.blob.arrayBuffer())).toEqual(
      new Uint8Array([1, 2, 3, 4, 5]),
    );
    expect(ev.done).toBe(1);
    expect(ev.progress).toContain("a.bin 3/5 1/2");
    expect(ev.progress).toContain("b.txt 0/0 2/2");
  });

  it("drops a file the host aborts and waits for the next one", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a", 3], ["b", 1]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a", size: 3 });
    hostChunk(ch, bin(1));
    ch.hostSays({ type: "abort", id: H, reason: "io" });
    ch.hostSays({ type: "start", id: H + 1, name: "b", size: 1 });
    hostChunk(ch, bin(9));
    ch.hostSays({ type: "end", id: H + 1 });
    expect(ev.errors).toEqual([["a", "io"]]);
    expect(ev.files.map((f) => f.name)).toEqual(["b"]);
    expect(ev.done).toBe(1);
  });

  it("cancel aborts the current file and ignores the rest", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a", 3], ["b", 1]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a", size: 3 });
    hostChunk(ch, bin(1));
    rx.cancel();
    expect(ch.trace()[1]).toEqual({ type: "abort", id: H, reason: "cancelled" });
    expect(ev.done).toBe(1);
    // The host had not seen the abort yet.
    hostChunk(ch, bin(2, 3));
    ch.hostSays({ type: "end", id: H });
    ch.hostSays({ type: "start", id: H + 1, name: "b", size: 1 });
    hostChunk(ch, bin(9));
    ch.hostSays({ type: "end", id: H + 1 });
    expect(ev.files).toEqual([]);
    expect(ev.errors).toEqual([]);
    // A new fetch works again.
    expect(rx.fetch()).toBe(true);
  });

  it("aborts with protocol when the size does not match", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a", 3]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a", size: 3 });
    hostChunk(ch, bin(1, 2));
    ch.hostSays({ type: "end", id: H });
    expect(ev.errors).toEqual([["a", "protocol"]]);
    expect(ev.files).toEqual([]);
    expect(ch.trace()[1]).toEqual({ type: "abort", id: H, reason: "protocol" });
    expect(ev.done).toBe(1);
  });

  it("aborts with protocol when more data arrives than announced", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a", 2]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a", size: 2 });
    hostChunk(ch, bin(1, 2, 3));
    expect(ev.errors).toEqual([["a", "protocol"]]);
  });

  it("reports a closed channel mid-download", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a", 2]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a", size: 2 });
    ch.emit("close");
    expect(ev.errors).toEqual([["a", "closed"]]);
    expect(ev.done).toBe(1);
  });

  it("ignores data without a started download", () => {
    const { ch, ev } = setupRecv();
    ch.hostSays({ type: "start", id: H, name: "a", size: 1 });
    hostChunk(ch, bin(1));
    ch.hostSays({ type: "end", id: H });
    expect(ev.files).toEqual([]);
    expect(ev.progress).toEqual([]);
  });

  it("sender and receiver on one channel do not disturb each other", async () => {
    const ch = new FakeChannel() as FakeChannel & { binaryType?: string };
    const saved: string[] = [];
    const senderErrors: Array<[string, FileFailReason]> = [];
    const files: string[] = [];
    const recvErrors: Array<[string, DownloadFailReason]> = [];
    const sender = new FileSender(ch, 4, {
      onSaved: (n) => saved.push(n),
      onError: (n, r) => senderErrors.push([n, r]),
    });
    const rx = new FileReceiver(ch, {
      onFile: (n) => files.push(n),
      onError: (n, r) => recvErrors.push([n, r]),
    });
    ch.hostSays({ type: "offer", list_id: 1, files: [{ name: "d", size: 2 }], skipped_folders: 0 });
    rx.fetch();
    sender.enqueue([fakeFile("up.bin", 4)]);
    await flush();
    // Upload id 1 is in flight while download id H starts.
    ch.hostSays({ type: "start", id: H, name: "d", size: 2 });
    ch.emit("message", { data: bin(1, 2) });
    // Host aborts its own download: must not touch the upload.
    ch.hostSays({ type: "abort", id: H, reason: "io" });
    expect(senderErrors).toEqual([]);
    expect(recvErrors).toEqual([["d", "io"]]);
    // Host answers the upload: must not touch the receiver.
    ch.hostSays({ type: "saved", id: 1, name: "up.bin" });
    await flush();
    expect(saved).toEqual(["up.bin"]);
    expect(files).toEqual([]);
    // An upload abort with a small id is not a download abort either.
    ch.hostSays({ type: "offer", list_id: 2, files: [{ name: "e", size: 1 }], skipped_folders: 0 });
    rx.fetch();
    ch.hostSays({ type: "start", id: H + 1, name: "e", size: 1 });
    ch.hostSays({ type: "abort", id: 1, reason: "io" });
    ch.emit("message", { data: bin(7) });
    ch.hostSays({ type: "end", id: H + 1 });
    expect(files).toEqual(["e"]);
  });
});

describe("downloadFailLabel", () => {
  it("covers every reason", () => {
    for (const r of ["cancelled", "disabled", "io", "protocol", "closed", "too_large"] as const) {
      expect(downloadFailLabel(r).length).toBeGreaterThan(0);
    }
  });
});

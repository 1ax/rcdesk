import { describe, expect, it } from "vitest";
import {
  BUFFER_HIGH_BYTES,
  BUFFER_LOW_BYTES,
  FileSender,
  chooseChunkSize,
  dragHasFiles,
  failReasonLabel,
  pickDroppedFiles,
  type DataTransferLike,
  type FileChannelLike,
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

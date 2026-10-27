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
  downloadFileName,
  dragHasFiles,
  failReasonLabel,
  groupByRelativePath,
  pickDroppedFiles,
  readFolder,
  type DirEntryLike,
  type EntryLike,
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
  const folderEntry = { isDirectory: true, isFile: false, name: "Фото" };
  const item = (f: File | null, dir: boolean | null, kind = "file") => ({
    kind,
    getAsFile: () => f,
    webkitGetAsEntry: () =>
      dir === null ? null : dir ? folderEntry : { isDirectory: false, isFile: true, name: "x" },
  });

  it("separates files and folder entries", () => {
    const a = file("a");
    const b = file("b");
    const dt: DataTransferLike = {
      items: [item(a, false), item(null, true), item(b, null), item(null, null, "string")],
      files: [],
    };
    const res = pickDroppedFiles(dt);
    expect(res.files).toEqual([a, b]);
    expect(res.folders).toHaveLength(1);
    expect(res.folders[0]).toBe(folderEntry);
  });

  it("falls back to files when items are unavailable", () => {
    const a = file("a");
    expect(pickDroppedFiles({ files: [a] })).toEqual({ files: [a], folders: [] });
  });

  it("detects file drags", () => {
    expect(dragHasFiles({ types: ["Files"] })).toBe(true);
    expect(dragHasFiles({ types: ["text/plain"] })).toBe(false);
    expect(dragHasFiles(null)).toBe(false);
  });
});

describe("FileSender folders", () => {
  it("sends folder and dir in start, numbering folders from 1", async () => {
    const ch = new FakeChannel();
    const tx = new FileSender(ch, 100);
    tx.enqueueFolder([
      { file: fakeFile("a", 0), dir: "Фото" },
      { file: fakeFile("b", 0), dir: "Фото/2024" },
    ]);
    await flush();
    ch.hostSays({ type: "saved", id: 1, name: "a" });
    await flush();
    ch.hostSays({ type: "saved", id: 2, name: "b" });
    await flush();
    tx.enqueue([fakeFile("plain", 0)]);
    await flush();
    ch.hostSays({ type: "saved", id: 3, name: "plain" });
    await flush();
    tx.enqueueFolder([{ file: fakeFile("c", 0), dir: "Док" }]);
    await flush();
    const starts = ch.trace().filter((m) => (m as { type?: string }).type === "start");
    expect(starts).toEqual([
      { type: "start", id: 1, name: "a", size: 0, folder: 1, dir: "Фото" },
      { type: "start", id: 2, name: "b", size: 0, folder: 1, dir: "Фото/2024" },
      { type: "start", id: 3, name: "plain", size: 0 },
      { type: "start", id: 4, name: "c", size: 0, folder: 2, dir: "Док" },
    ]);
  });

  it("empty folder sends nothing and does not use a number", async () => {
    const ch = new FakeChannel();
    const tx = new FileSender(ch, 100);
    tx.enqueueFolder([]);
    await flush();
    expect(ch.sent).toHaveLength(0);
    tx.enqueueFolder([{ file: fakeFile("a", 0), dir: "D" }]);
    await flush();
    expect(ch.trace()[0]).toEqual({ type: "start", id: 1, name: "a", size: 0, folder: 1, dir: "D" });
  });
});

type FakeNode = EntryLike & Partial<DirEntryLike> & { file?: FakeFileFn };
type FakeFileFn = (ok: (f: File) => void, err: (e: unknown) => void) => void;

const fakeFileEntry = (name: string, fail = false): FakeNode => ({
  isDirectory: false,
  isFile: true,
  name,
  file: (ok, err) => (fail ? err(new Error("x")) : ok({ name } as unknown as File)),
});

/** `batches` are returned by successive `readEntries` calls, then `[]`;
 * a `fail` directory errors on read. */
const fakeDir = (name: string, batches: FakeNode[][], fail = false): DirEntryLike => ({
  isDirectory: true,
  isFile: false,
  name,
  createReader: () => {
    let i = 0;
    return {
      readEntries: (ok, err) => {
        if (fail) err(new Error("x"));
        else ok(i < batches.length ? (batches[i++] as EntryLike[]) : []);
      },
    };
  },
});

describe("readFolder", () => {
  it("walks nested folders with dir from names", async () => {
    const sub = fakeDir("2024", [[fakeFileEntry("b")]]);
    const root = fakeDir("Фото", [[fakeFileEntry("a"), sub as FakeNode]]);
    const res = await readFolder(root);
    expect(res.skipped).toBe(0);
    expect(res.entries.map((e) => [e.file.name, e.dir])).toEqual([
      ["a", "Фото"],
      ["b", "Фото/2024"],
    ]);
  });

  it("reads entries in batches until empty", async () => {
    const root = fakeDir("R", [[fakeFileEntry("a")], [fakeFileEntry("b")]]);
    const res = await readFolder(root);
    expect(res.entries.map((e) => e.file.name)).toEqual(["a", "b"]);
  });

  it("empty subfolder yields nothing", async () => {
    const root = fakeDir("R", [[fakeDir("empty", []) as FakeNode]]);
    expect(await readFolder(root)).toEqual({ entries: [], skipped: 0 });
  });

  it("skips .DS_Store silently", async () => {
    const root = fakeDir("R", [[fakeFileEntry(".DS_Store"), fakeFileEntry("a")]]);
    const res = await readFolder(root);
    expect(res.entries.map((e) => e.file.name)).toEqual(["a"]);
    expect(res.skipped).toBe(0);
  });

  it("counts failed file() and failed subfolder reads", async () => {
    const bad = fakeDir("bad", [], true);
    const root = fakeDir("R", [[fakeFileEntry("x", true), bad as FakeNode, fakeFileEntry("ok")]]);
    const res = await readFolder(root);
    expect(res.entries.map((e) => e.file.name)).toEqual(["ok"]);
    expect(res.skipped).toBe(2);
  });
});

describe("groupByRelativePath", () => {
  it("derives dir, skipping .DS_Store and empty paths", () => {
    const f = (name: string, rel: string) => ({ name, webkitRelativePath: rel });
    const res = groupByRelativePath([
      f("a.txt", "Фото/2024/a.txt"),
      f("b.txt", "Фото/b.txt"),
      f(".DS_Store", "Фото/.DS_Store"),
      f("loose", ""),
    ]);
    expect(res.map((e) => e.dir)).toEqual(["Фото/2024", "Фото"]);
    expect(res.map((e) => e.file.name)).toEqual(["a.txt", "b.txt"]);
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
    expect(ch.trace()).toEqual([{ type: "fetch", list_id: 4, fetch: 1 }]);
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

  it("aborts with protocol when start claims more than the offer (slice E4)", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a.bin", 5]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a.bin", size: 6 });
    expect(ch.trace()).toContainEqual({ type: "abort", id: H, reason: "protocol" });
    expect(ev.errors).toEqual([["a.bin", "protocol"]]);
    const sent = ch.trace().length;
    hostChunk(ch, bin(1, 2, 3));
    ch.hostSays({ type: "end", id: H });
    expect(ch.trace().length).toBe(sent);
    expect(ev.files).toEqual([]);
    expect(ev.errors.length).toBe(1);
  });

  it("aborts with protocol when the second start exceeds the remaining budget (slice E4)", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a", 3], ["b", 2]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a", size: 3 });
    hostChunk(ch, bin(1, 2, 3));
    ch.hostSays({ type: "end", id: H });
    expect(ev.files.map((f) => f.name)).toEqual(["a"]);
    ch.hostSays({ type: "start", id: H + 1, name: "b", size: 3 });
    expect(ch.trace()).toContainEqual({ type: "abort", id: H + 1, reason: "protocol" });
    expect(ev.errors).toEqual([["b", "protocol"]]);
  });

  it("accepts a start within the remaining budget (slice E4)", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a", 3], ["b", 2]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a", size: 3 });
    hostChunk(ch, bin(1, 2, 3));
    ch.hostSays({ type: "end", id: H });
    ch.hostSays({ type: "start", id: H + 1, name: "b", size: 2 });
    hostChunk(ch, bin(4, 5));
    ch.hostSays({ type: "end", id: H + 1 });
    expect(ev.files.map((f) => f.name)).toEqual(["a", "b"]);
    expect(ev.errors).toEqual([]);
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

  it("ignores a stale start after cancel and fetch, accepts the new request", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a", 3], ["b", 1]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a", size: 3, fetch: 1 });
    rx.cancel();
    expect(rx.fetch()).toBe(true);
    expect(ch.trace()[2]).toEqual({ type: "fetch", list_id: 1, fetch: 2 });
    // The host sent the next file of the old list before it saw the abort.
    ch.hostSays({ type: "start", id: H + 1, name: "b", size: 1, fetch: 1 });
    hostChunk(ch, bin(9));
    ch.hostSays({ type: "end", id: H + 1 });
    expect(ev.files).toEqual([]);
    expect(ev.errors).toEqual([]);
    expect(ev.progress).toEqual(["a 0/3 1/2"]);
    // The new request is served.
    ch.hostSays({ type: "start", id: H + 2, name: "a", size: 1, fetch: 2 });
    hostChunk(ch, bin(5));
    ch.hostSays({ type: "end", id: H + 2 });
    expect(ev.files.map((f) => f.name)).toEqual(["a"]);
  });

  it("accepts a start without a request number (old host)", () => {
    const { ch, ev, rx } = setupRecv();
    ch.hostSays(offerMsg(1, [["a", 1]]));
    rx.fetch();
    ch.hostSays({ type: "start", id: H, name: "a", size: 1 });
    hostChunk(ch, bin(5));
    ch.hostSays({ type: "end", id: H });
    expect(ev.files.map((f) => f.name)).toEqual(["a"]);
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

describe("downloadFileName", () => {
  it("truncates a real long name, keeping the extension", () => {
    const name =
      "Решение мирового судьи СУ 4 Центрального округа Курска от 28.09.2026 № 2-645-2026 (мотивированное, изготовлено 13.10.2026) Борзенков - Ситилинк, отказ в передаче ноутбука из-за отсутствия предоплаты, 5 000 моральный + 2 500 штраф.pdf";
    const out = downloadFileName(name);
    expect(out.length).toBeLessThanOrEqual(150);
    expect(out.endsWith(".pdf")).toBe(true);
    expect(out.startsWith("Решение мирового судьи")).toBe(true);
  });

  it("keeps a name of exactly 150 characters", () => {
    const name = "a".repeat(146) + ".pdf";
    expect(name.length).toBe(150);
    expect(downloadFileName(name)).toBe(name);
  });

  it("does not leave a lone surrogate at the truncation boundary", () => {
    const name = "a".repeat(145) + "😀".repeat(10) + ".txt";
    const out = downloadFileName(name);
    expect(out).not.toMatch(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])/);
    expect(out.length).toBeLessThanOrEqual(150);
    expect(out.endsWith(".txt")).toBe(true);
  });

  it("truncates a long name without extension to exactly 150", () => {
    expect(downloadFileName("б".repeat(300)).length).toBe(150);
  });

  it("replaces U+202F and U+00A0 with a plain space", () => {
    expect(downloadFileName("Сканирование 16\u202Fокт. 2026\u202Fг..pdf")).toBe("Сканирование 16 окт. 2026 г..pdf");
    expect(downloadFileName("a\u00A0b.txt")).toBe("a b.txt");
  });

  it("composes decomposed letters to NFC", () => {
    const out = downloadFileName("\u0438\u0306.txt");
    expect(out).toBe("\u0439.txt");
    expect(out.length).toBe(5);
  });

  it("drops format and control characters", () => {
    expect(downloadFileName("a\u200Eb.pdf")).toBe("ab.pdf");
    expect(downloadFileName("a\u0007b.pdf")).toBe("ab.pdf");
  });

  it("falls back to file when nothing is left", () => {
    expect(downloadFileName("")).toBe("file");
    expect(downloadFileName("\u200E")).toBe("file");
  });

  it("leaves an ordinary name alone", () => {
    expect(downloadFileName("report.docx")).toBe("report.docx");
  });
});

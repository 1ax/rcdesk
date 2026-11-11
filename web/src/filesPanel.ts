/**
 * File transfer inside a session (slice 3.4): upload to the host (button,
 * folder picker, drag-and-drop onto the video area) and download of files the
 * host user copied. UI on top of `fileTransfer.ts`. A feature factory split
 * out of `app.ts`'s `mount()`: it finds its own DOM in `root`, keeps its state
 * in the closure and talks to the rest of `mount()` only through
 * `FilesPanelDeps` and the returned `FilesPanel`.
 */
import {
  FileReceiver,
  FileSender,
  chooseChunkSize,
  defaultMaxDownloadBytes,
  downloadFailLabel,
  downloadFileName,
  dragHasFiles,
  failReasonLabel,
  groupByRelativePath,
  pickDroppedFiles,
  readFolder,
  type DirEntryLike,
} from "./fileTransfer";
import type { OfferedFile } from "./generated/OfferedFile";


/** How long a downloaded file's object URL stays valid (slice 3.4c). */
const DOWNLOAD_URL_REVOKE_MS = 60_000;

export interface FilesPanelDeps {
  /** The `mount()` root; the panel looks its own elements up here. */
  root: Element;
  /** `.session-video`: shared with `mount()`; file drags are handled on it. */
  sessionVideoEl: HTMLDivElement;
  /** `#clipboard-note` text line (shared with the clipboard bridge). */
  showClipboardNote: (note: string) => void;
  /** The SCTP `maxMessageSize` of the current session, if known. */
  maxMessageSize: () => number | undefined;
  /** Files on the host clipboard replaced the last synced text. */
  forgetClipboardText: () => void;
}

export interface FilesPanel {
  /** `host_info.files_enabled` arrived: stores it and refreshes the file UI. */
  onHostInfo: (filesEnabled: boolean) => void;
  /** The `file` data channel arrived: (re)creates the sender and receiver on it. */
  attachChannel: (dc: RTCDataChannel) => void;
  /** Session ended: disposes sender/receiver, forgets the host flag, hides the UI. */
  resetSession: () => void;
}

export function createFilesPanel(deps: FilesPanelDeps): FilesPanel {
  const { root, sessionVideoEl, showClipboardNote } = deps;


  const fileStatusEl = root.querySelector<HTMLSpanElement>("#file-status")!;
  const fileStatusTextEl = root.querySelector<HTMLSpanElement>("#file-status-text")!;
  const fileCancelBtn = root.querySelector<HTMLButtonElement>("#file-cancel-btn")!;
  const downloadStatusEl = root.querySelector<HTMLSpanElement>("#download-status")!;
  const downloadStatusTextEl = root.querySelector<HTMLSpanElement>("#download-status-text")!;
  const downloadCancelBtn = root.querySelector<HTMLButtonElement>("#download-cancel-btn")!;
  const fileFetchBtn = root.querySelector<HTMLButtonElement>("#file-fetch-btn")!;
  const fileSendBtn = root.querySelector<HTMLButtonElement>("#file-send-btn")!;
  const fileInput = root.querySelector<HTMLInputElement>("#file-input")!;
  const folderSendBtn = root.querySelector<HTMLButtonElement>("#folder-send-btn")!;
  const folderInput = root.querySelector<HTMLInputElement>("#folder-input")!;
  const fileDropHint = root.querySelector<HTMLDivElement>("#file-drop-hint")!;

  // Slice 3.4b: file upload to the host. `filesEnabled` is the host's
  // `host_info.files_enabled`; `fileSender` exists once the `file` channel
  // arrived. Both are reset in `stopSessionResources`.
  let filesEnabled = false;
  let fileSender: FileSender | null = null;
  // Slice 3.4c: files the host user copied, offered for download.
  let fileReceiver: FileReceiver | null = null;
  let offeredFiles: OfferedFile[] = [];
  let offeredFolders = 0;
  let downloading = false;
  // Set when the current download is cancelled or a file fails: the offer
  // then stays so the user can retry; a clean download forgets it.
  let downloadFailed = false;
  let fileDragDepth = 0;

  /** Whether file upload can be used right now (host allows it and the
   * `file` channel is in hand). */
  function filesAvailable(): boolean {
    return filesEnabled && fileSender !== null;
  }

  function updateFileUi(): void {
    fileSendBtn.hidden = !filesAvailable();
    folderSendBtn.hidden = !filesAvailable();
    if (!filesAvailable()) {
      fileStatusEl.hidden = true;
      fileStatusTextEl.textContent = "";
      offeredFiles = [];
      offeredFolders = 0;
      downloading = false;
      downloadStatusEl.hidden = true;
      downloadStatusTextEl.textContent = "";
    }
    const canFetch = filesAvailable() && offeredFiles.length > 0 && !downloading;
    fileFetchBtn.hidden = !canFetch;
    fileFetchBtn.textContent = `Скачать (${offeredFiles.length})`;
    fileFetchBtn.title = offeredFiles.map((f) => f.name).join("\n");
  }

  /** Hands a downloaded file to the browser as an ordinary download. */
  function saveDownloadedFile(name: string, blob: Blob): void {
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = downloadFileName(name);
    a.style.display = "none";
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), DOWNLOAD_URL_REVOKE_MS);
  }

  function hideFileDropHint(): void {
    fileDragDepth = 0;
    fileDropHint.hidden = true;
  }

  /** Queues `files` for upload (slice 3.4b). */
  function submitFiles(files: File[]): void {
    if (!fileSender || !filesEnabled) return;
    fileSender.enqueue(files);
  }

  /** Reads dropped folders one by one and queues each as a folder upload. */
  async function submitDroppedFolders(folders: DirEntryLike[]): Promise<void> {
    for (const folder of folders) {
      try {
        const { entries, skipped } = await readFolder(folder);
        if (entries.length === 0) {
          showClipboardNote(`Пустая папка не отправлена: ${folder.name}`);
        } else if (fileSender && filesEnabled) {
          fileSender.enqueueFolder(entries);
        }
        if (skipped > 0) showClipboardNote(`Не удалось прочитать: ${skipped}`);
      } catch {
        showClipboardNote(`Не удалось прочитать: ${folder.name}`);
      }
    }
  }

  function setupFileChannel(dc: RTCDataChannel): void {
    fileSender?.dispose();
    fileReceiver?.dispose();
    fileReceiver = new FileReceiver(dc, {
      onOffer: (files, skippedFolders) => {
        // Files on the host clipboard replaced the last synced text.
        if (files.length > 0 || skippedFolders > 0) deps.forgetClipboardText();
        offeredFiles = files;
        offeredFolders = skippedFolders;
        if (files.length === 0 && skippedFolders > 0) {
          showClipboardNote(`Папки пока не поддерживаются: пропущено ${skippedFolders}`);
        }
        updateFileUi();
      },
      onProgress: (p) => {
        const pct = p.size > 0 ? Math.floor((p.sent / p.size) * 100) : 0;
        downloadStatusTextEl.textContent = `⬇ ${p.name} ${pct}% (${p.index}/${p.total})`;
        downloadStatusEl.hidden = false;
      },
      onFile: (name, blob) => {
        saveDownloadedFile(name, blob);
        showClipboardNote(`Скачано: ${name}`);
      },
      onError: (name, reason) => {
        downloadFailed = true;
        showClipboardNote(
          name === ""
            ? `Не скачано: ${downloadFailLabel(reason, defaultMaxDownloadBytes())}`
            : `Не скачано: ${name} — ${downloadFailLabel(reason, defaultMaxDownloadBytes())}`,
        );
      },
      onDone: () => {
        if (downloading && !downloadFailed) {
          offeredFiles = [];
          offeredFolders = 0;
        }
        downloading = false;
        downloadStatusEl.hidden = true;
        downloadStatusTextEl.textContent = "";
        updateFileUi();
      },
    });
    fileSender = new FileSender(dc, chooseChunkSize(deps.maxMessageSize()), {
      onProgress: (p) => {
        const pct = p.size > 0 ? Math.floor((p.sent / p.size) * 100) : 0;
        fileStatusTextEl.textContent = `⬆ ${p.name} ${pct}% (${p.index}/${p.total})`;
        fileStatusEl.hidden = false;
      },
      onSaved: (name) => showClipboardNote(`Сохранено в Загрузки: ${name}`),
      onError: (name, reason) =>
        showClipboardNote(`Не отправлено: ${name} — ${failReasonLabel(reason)}`),
      onDone: () => {
        fileStatusEl.hidden = true;
        fileStatusTextEl.textContent = "";
      },
    });
    updateFileUi();
  }

  fileSendBtn.addEventListener("click", () => {
    fileInput.click();
  });
  fileInput.addEventListener("change", () => {
    const files = Array.from(fileInput.files ?? []);
    // Reset so picking the same file again fires `change` again.
    fileInput.value = "";
    submitFiles(files);
  });
  folderSendBtn.addEventListener("click", () => {
    folderInput.click();
  });
  folderInput.addEventListener("change", () => {
    const entries = groupByRelativePath(Array.from(folderInput.files ?? []));
    folderInput.value = "";
    if (entries.length === 0) return;
    if (!fileSender || !filesEnabled) return;
    fileSender.enqueueFolder(entries);
  });
  fileCancelBtn.addEventListener("click", () => {
    fileSender?.cancelAll();
  });
  fileFetchBtn.addEventListener("click", () => {
    if (!fileReceiver || offeredFiles.length === 0) return;
    if (offeredFolders > 0) {
      showClipboardNote(`Папки пока не поддерживаются: пропущено ${offeredFolders}`);
    }
    downloadFailed = false;
    if (fileReceiver.fetch()) {
      downloading = true;
      updateFileUi();
    }
  });
  downloadCancelBtn.addEventListener("click", () => {
    downloadFailed = true;
    fileReceiver?.cancel();
  });
  // Drag-and-drop onto the video area. These are HTML5 drag events, which
  // are separate from the pointer/mouse events `input.ts` listens to on
  // `video` (the browser does not fire pointermove/pointerdown during an
  // OS-level file drag), so nothing is sent to the host's `input` channel.
  // File drags are always `preventDefault`ed over the video so a stray drop
  // never navigates the tab to the file; the hint shows only when uploads
  // are available.
  sessionVideoEl.addEventListener("dragenter", (e) => {
    if (!dragHasFiles(e.dataTransfer)) return;
    e.preventDefault();
    fileDragDepth += 1;
    if (filesAvailable()) fileDropHint.hidden = false;
  });
  sessionVideoEl.addEventListener("dragover", (e) => {
    if (!dragHasFiles(e.dataTransfer)) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = filesAvailable() ? "copy" : "none";
  });
  sessionVideoEl.addEventListener("dragleave", (e) => {
    if (!dragHasFiles(e.dataTransfer)) return;
    fileDragDepth = Math.max(0, fileDragDepth - 1);
    if (fileDragDepth === 0) fileDropHint.hidden = true;
  });
  sessionVideoEl.addEventListener("drop", (e) => {
    if (!dragHasFiles(e.dataTransfer)) return;
    e.preventDefault();
    hideFileDropHint();
    if (!filesAvailable() || !e.dataTransfer) return;
    const { files, folders } = pickDroppedFiles(e.dataTransfer);
    submitFiles(files);
    void submitDroppedFolders(folders);
  });

  return {
    onHostInfo: (enabled) => {
      filesEnabled = enabled;
      updateFileUi();
    },
    attachChannel: setupFileChannel,
    resetSession: () => {
    if (fileSender) {
      try {
        fileSender.cancelAll();
        fileSender.dispose();
      } catch (err) {
        console.warn("[rcdesk] file sender cleanup failed", err);
      }
      fileSender = null;
    }
    if (fileReceiver) {
      try {
        fileReceiver.cancel();
        fileReceiver.dispose();
      } catch (err) {
        console.warn("[rcdesk] file receiver cleanup failed", err);
      }
      fileReceiver = null;
    }
    filesEnabled = false;
    hideFileDropHint();
    updateFileUi();
    },
  };
}

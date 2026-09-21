// Assembles the two-screen UI (PIN entry -> live session) around
// `SignalingClient` and `PeerSession`. Signaling flow mirrors what the host
// speaks (see `host/src/signaling/mod.rs`): the host is the offer side, so
// this client only ever answers -- `joined` creates the `PeerSession`,
// `offer` is answered, ICE trickles both ways, and `ontrack` feeds the
// `<video>` element. Data channels opened by the host are accepted and
// stored by label (`PeerSession.getDataChannel`); once both `input` and
// `pointer` have arrived, `attachInput` (see `input.ts`) starts forwarding
// mouse/keyboard events to the host.

import "./style.css";
import { SignalingClient } from "./signaling";
import { PeerSession, toRtcIceServers } from "./session";
import { summarizeStats, takeSnapshot } from "./stats";
import type { Snapshot, StatsSummary } from "./stats";
import { attachInput } from "./input";
import { ClipboardBridge, isSafari } from "./clipboard";
import { applyCursor } from "./cursor";
import { displayOptions, parseDisplayId, shouldShowPicker } from "./displays";
import { inputBlockedLabel } from "./inputBlocked";
import { viewOnlyLabel } from "./inputStatus";
import {
  canConnect,
  deviceLabel,
  deviceStatusLabel,
  loadOwnerToken,
  saveOwnerToken,
  sortDevices,
} from "./myDevices";
import { reconnectBackoffMs } from "./reconnectBackoff";
import type { ControlMessage } from "./generated/ControlMessage";
import type { DisplayEntry } from "./generated/DisplayEntry";
import type { DeviceEntry } from "./generated/DeviceEntry";

type SessionStatus = "connecting" | "connected" | "disconnected" | "error";

/** Russian display text for each `SessionStatus`, shown in `#session-status`
 * (see `setSessionStatus`) -- the `data-status` attribute keeps the English
 * enum value unchanged (CSS selectors like `.status[data-status="error"]`
 * key off it), only the visible text is translated. */
const SESSION_STATUS_LABELS: Record<SessionStatus, string> = {
  connecting: "подключение",
  connected: "подключено",
  disconnected: "отключено",
  error: "ошибка",
};

/** Russian labels for the adaptation controller's `Quality.reason` codes
 * (see the `Quality` interface below and `host/src/adapt/mod.rs`, which
 * mints these as plain protocol strings -- not translated on the wire, only
 * here for display). An unrecognized code (shouldn't happen) is shown as
 * received rather than hidden. */
const QUALITY_REASON_LABELS: Record<string, string> = {
  bitrate: "битрейт",
  loss: "потери",
  encoder: "кодер",
  probe: "проба",
  remb: "remb",
};

/** `window.localStorage` can throw on *access*, not just on read/write, when
 * the browser blocks site data for this origin entirely -- `loadOwnerToken`'s
 * own try/catch is too late in that case, since the throw happens while
 * evaluating its argument. Returning `null` keeps the whole client working
 * (PIN entry included), just without a remembered owner token. */
function ownerStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function signalUrl(): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws`;
}

/** The signaling server's `error` messages are protocol strings, deliberately
 * kept stable and English server-side (`server/src/registry.rs`,
 * `server/src/ws.rs`); the client is what localizes them (slice 2.6h). An
 * unknown code still reaches the user verbatim rather than being swallowed. */
const SIGNAL_ERROR_LABELS: Record<string, string> = {
  "unknown pin": "Хост с таким PIN не найден — проверьте код на хосте",
  "host busy": "К этому хосту уже подключён другой клиент",
  "invalid message": "Сервер не понял сообщение клиента",
  "expected host_register": "Сервер не понял сообщение клиента",
  "device offline": "Устройство сейчас не в сети — попробуйте позже",
  "device not linked": "Это устройство не привязано к вашему аккаунту",
  "not authenticated": "Не удалось подтвердить вход — обновите страницу",
  "internal error": "Внутренняя ошибка сервера — попробуйте ещё раз",
  "replaced by a new connection": "Вы вошли с этим аккаунтом в другой вкладке — эта отключена",
};

export function signalErrorLabel(message: string): string {
  return SIGNAL_ERROR_LABELS[message] ?? `Ошибка: ${message}`;
}

/** Formats the `.build-badge` text (slice 2.6f): a visible commit id so a
 * tab left open across a deploy is obviously stale instead of silently
 * missing new protocol handling (see docs/dev-run.md) -- this cost a whole
 * live-check session once. `__BUILD_ID__` is defined by Vite from
 * `GITHUB_SHA` in CI, `"dev"` locally (see `vite.config.ts`). */
export function formatBuildBadge(buildId: string): string {
  return `build ${buildId}`;
}

/** The adaptation controller's current rate target (slice 2.3), from the
 * host's `ControlMessage::Quality` -- informational only, shown in the
 * overlay as `target N.N Mbit/s @ N fps (reason)`. */
interface Quality {
  bitrateKbps: number;
  fps: number;
  reason: string;
}

/** `appRttMs`, when given, is the application-level ping/pong round trip
 * measured over the `control` channel (see `startPingLoop`) -- distinct
 * from `rttMs`, the WebRTC-level candidate-pair RTT from `getStats()`.
 * `quality`, when given, is the adaptation controller's last `Quality`
 * message (see the `Quality` interface above). */
function formatOverlay(s: StatsSummary, appRttMs?: number, quality?: Quality): string {
  const parts: string[] = [];
  if (s.fps !== undefined) parts.push(`${s.fps.toFixed(0)} fps`);
  if (s.width !== undefined && s.height !== undefined) parts.push(`${s.width}x${s.height}`);
  if (s.kbps !== undefined) parts.push(`${(s.kbps / 1000).toFixed(1)} Mbit/s`);
  if (s.rttMs !== undefined) parts.push(`rtt ${s.rttMs.toFixed(0)} ms`);
  if (s.packetsLost !== undefined) parts.push(`loss ${s.packetsLost}`);
  if (s.codec !== undefined) parts.push(s.codec.replace(/^video\//, "").toUpperCase());
  if (appRttMs !== undefined) parts.push(`app ${appRttMs.toFixed(0)} ms`);
  if (s.jitterBufferMs !== undefined) parts.push(`jb ${s.jitterBufferMs.toFixed(0)} ms`);
  if (quality !== undefined) {
    const reason = QUALITY_REASON_LABELS[quality.reason] ?? quality.reason;
    parts.push(
      `target ${(quality.bitrateKbps / 1000).toFixed(1)} Mbit/s @ ${quality.fps} fps (${reason})`,
    );
  }
  return parts.join(" · ");
}

/** How often `app.ts` sends a `control`-channel `ping` to measure the
 * application-level round trip shown as `app N ms` in the overlay. */
const PING_INTERVAL_MS = 1000;

const STATS_INTERVAL_MS = 1000;

/** How often `app.ts` re-requests the device list (`list_devices`) while
 * the PIN/list screen is showing -- state (online/busy, a rename from
 * another tab) can change at any time. */
const DEVICE_LIST_INTERVAL_MS = 10000;

/** How long `#clipboard-note` stays visible after `ClipboardBridge.notify`
 * reports a too-large clipboard text (slice 2.5c, decision 5). */
const CLIPBOARD_NOTE_MS = 4000;

/** Builds the PIN/session UI inside `root` and wires it up. `root` may be
 * `null` (e.g. in an environment without the expected markup) -- a no-op. */
export function mount(root: Element | null): void {
  if (!root) return;

  root.innerHTML = `
    <div class="build-badge" id="build-badge"></div>
    <div class="pin-screen" id="pin-screen">
      <div class="card">
        <h1>rcdesk</h1>
        <div class="devices" id="devices" hidden>
          <h2>Мои компьютеры</h2>
          <ul class="device-list" id="device-list"></ul>
        </div>
        <p class="hint pin-hint">Или введите PIN с экрана хоста</p>
        <input
          id="pin-input"
          class="pin-input"
          type="text"
          inputmode="numeric"
          pattern="[0-9]*"
          maxlength="6"
          placeholder="000000"
          autocomplete="one-time-code"
        />
        <button id="connect-btn" class="btn">Подключиться</button>
        <p id="pin-status" class="hint"></p>
      </div>
    </div>
    <div class="session-screen" id="session-screen" hidden>
      <video id="video" autoplay playsinline muted></video>
      <div class="overlay" id="stats-overlay"></div>
      <div class="controls">
        <select id="display-select" class="display-select" hidden></select>
        <span id="view-only" class="status view-only" hidden></span>
        <span id="input-blocked" class="status input-blocked" hidden></span>
        <span id="clipboard-note" class="status clipboard-note" hidden></span>
        <span id="session-status" class="status"></span>
        <button id="disconnect-btn" class="btn btn-secondary">Отключиться</button>
      </div>
    </div>
  `;

  // Not part of either screen (see the markup above): a single element
  // outside the `hidden`-toggled `pin-screen`/`session-screen` pair, so it
  // stays visible before a session starts too -- the whole point (a stale
  // tab left open across a deploy) doesn't wait for a connection to matter.
  const buildBadge = root.querySelector<HTMLDivElement>("#build-badge")!;
  buildBadge.textContent = formatBuildBadge(__BUILD_ID__);

  const pinScreen = root.querySelector<HTMLDivElement>("#pin-screen")!;
  const sessionScreen = root.querySelector<HTMLDivElement>("#session-screen")!;
  const pinInput = root.querySelector<HTMLInputElement>("#pin-input")!;
  const connectBtn = root.querySelector<HTMLButtonElement>("#connect-btn")!;
  const pinStatus = root.querySelector<HTMLParagraphElement>("#pin-status")!;
  const video = root.querySelector<HTMLVideoElement>("#video")!;
  // Focusable so it can receive `keydown`/`keyup` (see `input.ts`'s
  // `attachInput`, which listens on the video element itself); a click
  // gives it focus the same way clicking any input widget would.
  video.tabIndex = 0;
  video.addEventListener("click", () => video.focus());
  const overlay = root.querySelector<HTMLDivElement>("#stats-overlay")!;
  const sessionStatus = root.querySelector<HTMLSpanElement>("#session-status")!;
  const disconnectBtn = root.querySelector<HTMLButtonElement>("#disconnect-btn")!;
  const displaySelect = root.querySelector<HTMLSelectElement>("#display-select")!;
  const viewOnlyEl = root.querySelector<HTMLSpanElement>("#view-only")!;
  const inputBlockedEl = root.querySelector<HTMLSpanElement>("#input-blocked")!;
  const clipboardNoteEl = root.querySelector<HTMLSpanElement>("#clipboard-note")!;
  const devicesEl = root.querySelector<HTMLDivElement>("#devices")!;
  const deviceListEl = root.querySelector<HTMLUListElement>("#device-list")!;

  // One long-lived signaling connection for the whole tab (slice 3.1e): the
  // server remembers which owner a socket authenticated as (and which
  // device, mid-session) only for the lifetime of that one WebSocket, so a
  // PIN-less reconnect to a device the owner already owns needs the same
  // connection to stay open across the "list of my computers" screen and a
  // live session, not a fresh one per session like before this slice.
  const signaling = new SignalingClient();
  let session: PeerSession | null = null;
  let sessionId: string | null = null;
  let statsTimer: ReturnType<typeof setInterval> | undefined;
  let prevSnapshot: Snapshot | undefined;
  let inputChannel: RTCDataChannel | null = null;
  let pointerChannel: RTCDataChannel | null = null;
  let detachInput: (() => void) | null = null;
  let pingTimer: ReturnType<typeof setInterval> | undefined;
  // The most recent application-level ping/pong round trip (see
  // `startPingLoop`), shown in the overlay as `app N ms` -- `undefined`
  // until the first `pong` arrives.
  let appRttMs: number | undefined;
  // The adaptation controller's last rate target (see `setupControlChannel`
  // and `formatOverlay`'s `Quality` interface) -- `undefined` until the
  // first `quality` message arrives (slice 2.3 -- `--no-adapt` sessions
  // never send one, so the overlay simply never grows this segment).
  let quality: Quality | undefined;
  // The host's display list and the id of the one currently streamed (slice
  // 2.4d), from `ControlMessage::Displays` -- drives `renderDisplayPicker`.
  // `currentDisplay` is `undefined` until the first `displays` message.
  let displays: DisplayEntry[] = [];
  let currentDisplay: number | undefined;
  // The `control` data channel, kept around so the picker's `change`
  // handler can send `ControlMessage::SelectDisplay` on it directly.
  let controlChannel: RTCDataChannel | null = null;
  // Whether the host reported it can't inject input for this session (slice
  // 2.5a, debt D26), from `ControlMessage::InputStatus`. While true, input
  // is never attached (see `maybeAttachInput`) and `#view-only` shows why.
  let inputBlocked = false;
  // Mirrors the clipboard with the host for this session (slice 2.5c), or
  // `null` when there's no session or `navigator.clipboard` isn't available
  // (e.g. an insecure context) -- see the `joined` handler and `teardown`.
  let clipboardBridge: ClipboardBridge | null = null;
  // Whether `onClipboardChange` is currently registered on
  // `navigator.clipboard` (only where `clipboardchange` exists -- Chrome),
  // so `teardown` knows whether to remove it.
  let clipboardChangeAttached = false;
  let clipboardNoteTimer: ReturnType<typeof setTimeout> | undefined;
  // The owner's device list (slice 3.1e), from `Authenticated`/`Devices` --
  // drives `renderDeviceList`. Empty until the first of either arrives.
  let devices: DeviceEntry[] = [];
  // The device currently being renamed inline (its row shows a text input
  // instead of its label), or `null` when no row is in that state. Only one
  // row at a time.
  let renamingDeviceId: string | null = null;
  // The device whose "Удалить" button currently reads "Точно?", awaiting a
  // second click to confirm (see the document-level `click` listener below,
  // which resets this when the user clicks anything else).
  let confirmingForgetId: string | null = null;
  // Set once the signaling socket itself closes (not just a session ending)
  // -- disables every connect affordance until `scheduleReconnect`'s next
  // attempt succeeds (slice 3.5a; the live WebRTC session, if any, is
  // untouched -- signaling is only needed to set one up).
  let connectionLost = false;
  // Attempt counter for `scheduleReconnect`'s backoff (`reconnectBackoffMs`);
  // reset to 0 once `authenticated` confirms the reconnect worked.
  let reconnectAttempt = 0;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  let deviceListTimer: ReturnType<typeof setInterval> | undefined;

  /** Shows `note` in `#clipboard-note` for `CLIPBOARD_NOTE_MS`, used as
   * `ClipboardBridge`'s `notify` dependency (decision 5: a too-large
   * clipboard text, in either direction). */
  function showClipboardNote(note: string): void {
    clipboardNoteEl.textContent = note;
    clipboardNoteEl.hidden = false;
    if (clipboardNoteTimer !== undefined) clearTimeout(clipboardNoteTimer);
    clipboardNoteTimer = setTimeout(() => {
      clipboardNoteEl.hidden = true;
      clipboardNoteEl.textContent = "";
      clipboardNoteTimer = undefined;
    }, CLIPBOARD_NOTE_MS);
  }

  function onClipboardFocusOrGesture(): void {
    clipboardBridge?.onFocusOrGesture();
  }

  function onClipboardChange(): void {
    void clipboardBridge?.onLocalClipboardChange();
  }

  /** Rebuilds `#display-select`'s options from `displays`/`currentDisplay`
   * and shows/hides it (only worth showing with 2+ displays, see
   * `shouldShowPicker`). Re-enables the select, since a fresh `displays`
   * message means any pending switch has resolved (see the `change`
   * listener below, which disables it while a switch is in flight). */
  function renderDisplayPicker(): void {
    displaySelect.innerHTML = "";
    for (const option of displayOptions(displays, currentDisplay ?? -1)) {
      const el = document.createElement("option");
      el.value = option.value;
      el.textContent = option.label;
      el.selected = option.selected;
      displaySelect.appendChild(el);
    }
    displaySelect.hidden = !shouldShowPicker(displays);
    displaySelect.disabled = false;
  }

  /** Rebuilds `#device-list` from `devices` (see `sortDevices`/`deviceLabel`/
   * `deviceStatusLabel`/`canConnect` in `myDevices.ts`) and shows/hides
   * `#devices` -- only worth showing once the owner has at least one linked
   * device. Rebuilt from scratch on every call, the same approach as
   * `renderDisplayPicker` above; the list is small and this runs at most a
   * few times a minute (see `startDeviceListLoop`). */
  function renderDeviceList(): void {
    const sorted = sortDevices(devices);
    devicesEl.hidden = sorted.length === 0;
    deviceListEl.innerHTML = "";
    const nowSecs = Math.floor(Date.now() / 1000);

    for (const entry of sorted) {
      const li = document.createElement("li");
      li.className = "device-item";

      if (renamingDeviceId === entry.device_id) {
        const input = document.createElement("input");
        input.className = "device-rename-input";
        input.value = deviceLabel(entry);
        input.addEventListener("keydown", (event) => {
          if (event.key === "Enter") {
            commitRename(entry.device_id, input.value);
          } else if (event.key === "Escape") {
            renamingDeviceId = null;
            renderDeviceList();
          }
        });
        const saveBtn = document.createElement("button");
        saveBtn.className = "btn-secondary device-rename-save";
        saveBtn.textContent = "Сохранить";
        saveBtn.addEventListener("click", () => commitRename(entry.device_id, input.value));
        li.appendChild(input);
        li.appendChild(saveBtn);
        deviceListEl.appendChild(li);
        continue;
      }

      const info = document.createElement("div");
      info.className = "device-info";
      const nameEl = document.createElement("span");
      nameEl.className = "device-name";
      nameEl.textContent = deviceLabel(entry);
      const statusEl = document.createElement("span");
      statusEl.className = "device-status";
      statusEl.textContent = deviceStatusLabel(entry, nowSecs);
      info.appendChild(nameEl);
      info.appendChild(statusEl);

      const actions = document.createElement("div");
      actions.className = "device-actions";

      const connectRowBtn = document.createElement("button");
      connectRowBtn.className = "btn device-connect";
      connectRowBtn.textContent = "Подключиться";
      connectRowBtn.disabled = connectionLost || !canConnect(entry);
      connectRowBtn.addEventListener("click", () => {
        // Call play() inside this click handler (a user gesture), same
        // reasoning as `connectBtn`'s listener below (ARCHITECTURE.md §10).
        void video.play().catch(() => {
          // Expected: there's no source yet. The gesture is what matters.
        });
        beginSession(() =>
          signaling.send({ type: "connect_device", device_id: entry.device_id }),
        );
      });

      const renameBtn = document.createElement("button");
      renameBtn.className = "btn-secondary device-rename";
      renameBtn.textContent = "Переименовать";
      renameBtn.addEventListener("click", () => {
        renamingDeviceId = entry.device_id;
        renderDeviceList();
      });

      const forgetBtn = document.createElement("button");
      forgetBtn.className = "btn-secondary device-forget";
      forgetBtn.dataset.deviceId = entry.device_id;
      forgetBtn.textContent = confirmingForgetId === entry.device_id ? "Точно?" : "Удалить";
      forgetBtn.addEventListener("click", () => {
        if (confirmingForgetId === entry.device_id) {
          signaling.send({ type: "forget_device", device_id: entry.device_id });
          confirmingForgetId = null;
        } else {
          confirmingForgetId = entry.device_id;
        }
        renderDeviceList();
      });

      actions.appendChild(connectRowBtn);
      actions.appendChild(renameBtn);
      actions.appendChild(forgetBtn);
      li.appendChild(info);
      li.appendChild(actions);
      deviceListEl.appendChild(li);
    }
  }

  /** Sends the renamed alias (or `null` to clear it, when blank after
   * trimming) and leaves rename mode. */
  function commitRename(deviceId: string, value: string): void {
    const alias = value.trim();
    signaling.send({
      type: "rename_device",
      device_id: deviceId,
      alias: alias === "" ? null : alias,
    });
    renamingDeviceId = null;
    renderDeviceList();
  }

  // Resets a pending "Точно?" delete confirmation when the user clicks
  // anything other than that same button (see `forgetBtn` above) -- a click
  // on the confirming button itself is handled, and re-renders, before this
  // listener runs (document is last in the bubbling chain), and `closest`
  // matches the (possibly now-detached) target element itself first, so
  // that click is never mistaken for "clicked elsewhere".
  document.addEventListener("click", (event) => {
    if (confirmingForgetId === null) return;
    const target = event.target as Element | null;
    if (target?.closest(`.device-forget[data-device-id="${confirmingForgetId}"]`)) return;
    confirmingForgetId = null;
    renderDeviceList();
  });

  function stopDeviceListLoop(): void {
    if (deviceListTimer !== undefined) {
      clearInterval(deviceListTimer);
      deviceListTimer = undefined;
    }
  }

  function stopReconnectTimer(): void {
    if (reconnectTimer !== undefined) {
      clearTimeout(reconnectTimer);
      reconnectTimer = undefined;
    }
  }

  /** Schedules the next signaling reconnect attempt after the WebSocket
   * closes (slice 3.5a). The delay follows `reconnectBackoffMs`'s schedule
   * (1, 2, 4, 8, 16, 30s); a failed attempt (server still down) reaches
   * `close` again, which calls this again for the next attempt, and a
   * successful one is detected in the `authenticated` handler, which resets
   * `reconnectAttempt` back to 0. Only the signaling connection is affected
   * -- an already-live WebRTC session doesn't need it and is left running. */
  function scheduleReconnect(): void {
    stopReconnectTimer();
    reconnectAttempt += 1;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = undefined;
      signaling.connect(signalUrl());
      const storage = ownerStorage();
      signaling.send({ type: "client_auth", token: storage ? loadOwnerToken(storage) : null });
    }, reconnectBackoffMs(reconnectAttempt));
  }

  /** Polls the device list every `DEVICE_LIST_INTERVAL_MS` while the
   * PIN/list screen is showing (state can change on another device or
   * another tab at any time -- online/busy, a rename, ...); stopped for the
   * duration of a session (see `showSessionScreen`) and restarted once
   * `teardown` returns to this screen (see `showPinScreen`). */
  function startDeviceListLoop(): void {
    stopDeviceListLoop();
    deviceListTimer = setInterval(() => {
      signaling.send({ type: "list_devices" });
    }, DEVICE_LIST_INTERVAL_MS);
  }

  // `input` and `pointer` arrive via `onDataChannel` in whatever order the
  // host happened to open them in, independent of the connection state
  // reaching "connected" -- attach as soon as both are in hand.
  function maybeAttachInput(): void {
    if (detachInput || inputBlocked || !inputChannel || !pointerChannel) return;
    const bridge = clipboardBridge;
    detachInput = attachInput(
      video,
      { input: inputChannel, pointer: pointerChannel },
      bridge
        ? {
            beforePaste: () => bridge.syncBeforePaste(),
            onCopyShortcut: () => bridge.beginDeferredCopy(),
          }
        : undefined,
    );
  }

  function stopPingLoop(): void {
    if (pingTimer !== undefined) {
      clearInterval(pingTimer);
      pingTimer = undefined;
    }
  }

  /** Sends a `ping` once a second so the overlay can show the
   * application-level round trip (`app N ms`, see `formatOverlay`) -- how
   * long a message actually takes over the `control` data channel, as
   * opposed to the WebRTC-level candidate-pair RTT `getStats()` reports. */
  function startPingLoop(dc: RTCDataChannel): void {
    stopPingLoop();
    pingTimer = setInterval(() => {
      if (dc.readyState !== "open") return;
      const msg: ControlMessage = { type: "ping", ts: performance.now() };
      dc.send(JSON.stringify(msg));
    }, PING_INTERVAL_MS);
  }

  function setupControlChannel(dc: RTCDataChannel): void {
    controlChannel = dc;
    if (dc.readyState === "open") startPingLoop(dc);
    else dc.addEventListener("open", () => startPingLoop(dc));
    dc.addEventListener("message", (event: MessageEvent<unknown>) => {
      if (typeof event.data !== "string") return;
      let msg: ControlMessage;
      try {
        msg = JSON.parse(event.data) as ControlMessage;
      } catch (err) {
        console.error("invalid control message", err);
        return;
      }
      if (msg.type === "pong") {
        appRttMs = performance.now() - msg.ts;
        return;
      }
      if (msg.type === "quality") {
        quality = { bitrateKbps: msg.bitrate_kbps, fps: msg.fps, reason: msg.reason };
        return;
      }
      if (msg.type === "displays") {
        displays = msg.displays;
        currentDisplay = msg.current;
        renderDisplayPicker();
        return;
      }
      if (msg.type === "input_status") {
        const label = viewOnlyLabel(msg);
        if (label !== null) {
          inputBlocked = true;
          detachInput?.();
          detachInput = null;
          viewOnlyEl.textContent = label;
          viewOnlyEl.hidden = false;
        } else {
          inputBlocked = false;
          viewOnlyEl.hidden = true;
          viewOnlyEl.textContent = "";
          maybeAttachInput();
        }
        return;
      }
      if (msg.type === "input_blocked") {
        // Slice 2.6e: this is a warning banner, not a view-only switch --
        // input keeps flowing as usual (`maybeAttachInput`/`inputBlocked`
        // above are untouched), the host's `SendInput` calls are just
        // silently dropped by Windows UIPI while an elevated window is
        // focused.
        const label = inputBlockedLabel(msg);
        inputBlockedEl.textContent = label ?? "";
        inputBlockedEl.hidden = label === null;
        return;
      }
      if (msg.type === "clipboard_text") {
        clipboardBridge?.onHostText(msg.text);
        return;
      }
      applyCursor(video, msg);
    });
  }

  function setSessionStatus(status: SessionStatus): void {
    sessionStatus.textContent = SESSION_STATUS_LABELS[status];
    sessionStatus.dataset.status = status;
  }

  function showPinScreen(): void {
    sessionScreen.hidden = true;
    pinScreen.hidden = false;
    startDeviceListLoop();
  }

  function showSessionScreen(): void {
    pinScreen.hidden = true;
    sessionScreen.hidden = false;
    stopDeviceListLoop();
  }

  function stopStatsLoop(): void {
    if (statsTimer !== undefined) {
      clearInterval(statsTimer);
      statsTimer = undefined;
    }
  }

  function startStatsLoop(): void {
    stopStatsLoop();
    statsTimer = setInterval(() => {
      const current = session;
      if (!current) return;
      current
        .getStats()
        .then((report) => {
          const now = Date.now();
          const summary = summarizeStats(report.values(), prevSnapshot, now);
          prevSnapshot = takeSnapshot(report.values(), now);
          overlay.textContent = formatOverlay(summary, appRttMs, quality);
        })
        .catch((err: unknown) => {
          console.error("failed to read stats", err);
        });
    }, STATS_INTERVAL_MS);
  }

  function teardown(reason: string): void {
    stopStatsLoop();
    stopPingLoop();
    detachInput?.();
    detachInput = null;
    inputChannel = null;
    pointerChannel = null;
    appRttMs = undefined;
    quality = undefined;
    displays = [];
    currentDisplay = undefined;
    controlChannel = null;
    displaySelect.hidden = true;
    displaySelect.innerHTML = "";
    inputBlocked = false;
    viewOnlyEl.hidden = true;
    viewOnlyEl.textContent = "";
    inputBlockedEl.hidden = true;
    inputBlockedEl.textContent = "";
    if (clipboardBridge) {
      window.removeEventListener("focus", onClipboardFocusOrGesture);
      document.removeEventListener("pointerdown", onClipboardFocusOrGesture, true);
      document.removeEventListener("keydown", onClipboardFocusOrGesture, true);
      if (clipboardChangeAttached) {
        navigator.clipboard.removeEventListener("clipboardchange", onClipboardChange);
        clipboardChangeAttached = false;
      }
    }
    clipboardBridge = null;
    if (clipboardNoteTimer !== undefined) {
      clearTimeout(clipboardNoteTimer);
      clipboardNoteTimer = undefined;
    }
    clipboardNoteEl.hidden = true;
    clipboardNoteEl.textContent = "";
    session?.close();
    session = null;
    // Unlike before this slice, the signaling connection itself is *not*
    // closed or discarded here -- it's one long-lived connection for the
    // whole tab now (see where `signaling` is created above), since the
    // server only remembers which owner (and, mid-session, which device) a
    // socket authenticated as for the lifetime of that one WebSocket. Only
    // the WebRTC session ends; the tab drops back to the PIN/list screen on
    // the same connection.
    sessionId = null;
    prevSnapshot = undefined;
    video.srcObject = null;
    video.style.cursor = "";
    overlay.textContent = "";
    connectBtn.disabled = connectionLost;
    pinStatus.textContent = reason;
    showPinScreen();
    // Refresh the list right away rather than waiting for the next
    // `startDeviceListLoop` tick (up to `DEVICE_LIST_INTERVAL_MS` later) --
    // state (this device's own `busy`, in particular) just changed.
    signaling.send({ type: "list_devices" });
  }

  /** Shared setup for both ways to start a session -- entering a PIN or
   * picking a device from "Мои компьютеры" (slice 3.1e). The only
   * difference between the two paths is which message kicks it off (`join`
   * vs `connect_device`); `send` performs that one. Must be called only
   * after the caller has already invoked `video.play()` synchronously
   * inside the click that triggered it -- Safari requires that exact call
   * to happen inside the user gesture (ARCHITECTURE.md §10), so it can't be
   * moved in here. */
  function beginSession(send: () => void): void {
    pinStatus.textContent = "";
    connectBtn.disabled = true;
    send();
  }

  // The `joined`/`offer`/`ice`/`bye`/`error` subscriptions below used to be
  // (re-)created inside `connectBtn`'s click listener, on a fresh
  // `SignalingClient` made for that one PIN entry. Slice 3.1e made the
  // connection long-lived (see where `signaling` is declared above), so
  // these are set up once, here, for the connection's whole lifetime
  // instead -- both the PIN path and the device-list path drive the same
  // session lifecycle through them.
  signaling.on("authenticated", (msg) => {
    const storage = ownerStorage();
    if (storage) saveOwnerToken(storage, msg.token);
    devices = msg.devices;
    renderDeviceList();
    // Confirms a reconnect (slice 3.5a) succeeded, if that's what this is --
    // a no-op the rest of the time, since `connectionLost` starts `false`.
    if (connectionLost) {
      connectionLost = false;
      reconnectAttempt = 0;
      pinStatus.textContent = "";
      connectBtn.disabled = connectionLost;
      // Only while the PIN/list screen is actually showing -- during a
      // session this loop stays stopped, same as `showSessionScreen` left it
      // (see `startDeviceListLoop`'s doc comment).
      if (!pinScreen.hidden) startDeviceListLoop();
      renderDeviceList();
    }
  });

  signaling.on("devices", (msg) => {
    devices = msg.devices;
    renderDeviceList();
  });

  signaling.on("joined", (msg) => {
    sessionId = msg.session_id;
    showSessionScreen();
    setSessionStatus("connecting");

    if (navigator.clipboard) {
      const clipboard = navigator.clipboard;
      clipboardBridge = new ClipboardBridge({
        writeText: (text) => clipboard.writeText(text),
        readText: () => clipboard.readText(),
        // The deferred `ClipboardItem` write (decision 3) is a
        // Safari-only trick -- Chrome already gets the same result from
        // `writeText` in `onHostText` (decision 1b).
        writeDeferred: isSafari(navigator.userAgent)
          ? (blob) => clipboard.write([new ClipboardItem({ "text/plain": blob })])
          : undefined,
        send: (msg) => {
          if (inputChannel?.readyState === "open") {
            inputChannel.send(JSON.stringify(msg));
          }
        },
        notify: (note) => showClipboardNote(note),
        setTimeout: (handler, ms) => setTimeout(handler, ms),
        clearTimeout: (handle) => clearTimeout(handle),
      });
      window.addEventListener("focus", onClipboardFocusOrGesture);
      document.addEventListener("pointerdown", onClipboardFocusOrGesture, true);
      document.addEventListener("keydown", onClipboardFocusOrGesture, true);
      if ("onclipboardchange" in clipboard) {
        clipboard.addEventListener("clipboardchange", onClipboardChange);
        clipboardChangeAttached = true;
      }
    } else {
      console.warn("navigator.clipboard unavailable; clipboard sync disabled");
    }

    session = new PeerSession(
      { iceServers: toRtcIceServers(msg.ice_servers) },
      {
        onIceCandidate: (candidate) => {
          if (sessionId) {
            signaling.send({ type: "ice", session_id: sessionId, candidate });
          }
        },
        onTrack: (stream) => {
          video.srcObject = stream;
        },
        onDataChannel: (label, dc) => {
          if (label === "input") inputChannel = dc;
          else if (label === "pointer") pointerChannel = dc;
          else if (label === "control") setupControlChannel(dc);
          maybeAttachInput();
        },
        onConnectionStateChange: (state) => {
          if (state === "connected") {
            setSessionStatus("connected");
          } else if (state === "failed" || state === "closed" || state === "disconnected") {
            teardown(`Соединение разорвано (${state})`);
          }
        },
      },
    );
    startStatsLoop();
  });

  signaling.on("offer", (msg) => {
    if (!session) return;
    session
      .acceptOffer(msg.sdp)
      .then((sdp) => {
        if (sessionId) {
          signaling.send({ type: "answer", session_id: sessionId, sdp });
        }
      })
      .catch((err: unknown) => {
        console.error("failed to negotiate session", err);
        setSessionStatus("error");
        teardown("Не удалось согласовать сеанс");
      });
  });

  signaling.on("ice", (msg) => {
    session?.addRemoteIce(msg.candidate).catch((err: unknown) => {
      console.error("failed to add remote ice candidate", err);
    });
  });

  signaling.on("bye", (msg) => {
    // A `bye` for a session that isn't the current one is stale -- e.g. one
    // sent for a session that already ended some other way before a
    // signaling reconnect (slice 3.5a; the server itself no longer sends
    // `bye` at all for a plain signaling-only disconnect, see
    // `server/src/ws.rs`, but an explicit `bye` can still race a reconnect).
    // Tearing down whatever session *is* now running would be wrong.
    if (msg.session_id !== sessionId) return;
    setSessionStatus("disconnected");
    teardown("Сеанс завершён");
  });

  signaling.on("error", (msg) => {
    if (sessionId === null) {
      // No session yet (a bad PIN, a stale device row, an auth hiccup --
      // all the new 3.1e error codes land here): stay on the PIN/list
      // screen, just surface it and make sure nothing's left disabled from
      // the attempt that failed. `teardown` would be wrong here -- there is
      // no session to tear down, and it would also (harmlessly but
      // needlessly) re-request the device list and flip the screen it's
      // already on.
      pinStatus.textContent = signalErrorLabel(msg.message);
      connectBtn.disabled = connectionLost;
      renderDeviceList();
      return;
    }
    setSessionStatus("error");
    teardown(signalErrorLabel(msg.message));
  });

  // Not a `SignalMessage` (so not reachable through `.on`, whose listener
  // type is keyed on `SignalMessage["type"]`) -- `SignalingClient` extends
  // `EventTarget` and dispatches this itself on the underlying WebSocket's
  // `close` (see `signaling.ts`). Slice 3.5a: signaling is only needed to
  // set a session up, not to keep one running, so a live WebRTC session is
  // left completely alone here -- only the ability to start a *new* one is
  // disabled until `scheduleReconnect` gets the socket back.
  signaling.addEventListener("close", () => {
    connectionLost = true;
    pinStatus.textContent = "Нет связи с сервером — переподключаюсь…";
    connectBtn.disabled = true;
    stopDeviceListLoop();
    renderDeviceList();
    scheduleReconnect();
  });

  // A tab brought back into view may have missed a while of state changes
  // (another tab/device connecting, going busy, being renamed) -- refresh
  // right away instead of waiting for `startDeviceListLoop`'s next tick.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible" && !pinScreen.hidden) {
      signaling.send({ type: "list_devices" });
    }
  });

  // Best-effort notice that this tab is going away (slice 3.5a): `pagehide`
  // fires reliably on navigation/tab close/reload (unlike `beforeunload`,
  // unreliable on mobile Safari in particular). Not guaranteed to reach the
  // server -- the socket may already be down -- but when it does, the host
  // learns the session is over immediately instead of only noticing once its
  // own connection drops.
  window.addEventListener("pagehide", () => {
    if (sessionId) {
      signaling.send({ type: "bye", session_id: sessionId });
    }
    session?.close();
  });

  // One connection for the whole tab (see where `signaling` is declared
  // above): open it now, at mount, rather than waiting for a PIN/device
  // click -- `client_auth` needs to ride this same connection for the
  // server to recognize the owner and hand back their device list.
  signaling.connect(signalUrl());
  const storage = ownerStorage();
  signaling.send({ type: "client_auth", token: storage ? loadOwnerToken(storage) : null });

  connectBtn.addEventListener("click", () => {
    const pin = pinInput.value.trim();
    if (!/^\d{6}$/.test(pin)) {
      pinStatus.textContent = "Введите 6-значный PIN";
      return;
    }

    // Call play() inside this click handler (a user gesture) so Safari
    // allows the video to keep playing once `srcObject` is assigned later,
    // asynchronously, from `ontrack` (see ARCHITECTURE.md §10).
    void video.play().catch(() => {
      // Expected: there's no source yet. The gesture is what matters.
    });

    beginSession(() => signaling.send({ type: "join", pin }));
  });

  disconnectBtn.addEventListener("click", () => {
    if (sessionId) {
      signaling.send({ type: "bye", session_id: sessionId });
    }
    teardown("Отключено");
  });

  displaySelect.addEventListener("change", () => {
    const id = parseDisplayId(displaySelect.value);
    if (id !== null && controlChannel?.readyState === "open") {
      const msg: ControlMessage = { type: "select_display", id };
      controlChannel.send(JSON.stringify(msg));
      displaySelect.disabled = true;
    } else if (currentDisplay !== undefined) {
      displaySelect.value = String(currentDisplay);
    }
    // Give focus back to the video so keyboard input keeps going to the
    // session (see the `click` listener above, which does the same after a
    // user gesture on the video itself).
    video.focus();
  });

  showPinScreen();
}

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
import {
  fullscreenButtonLabel,
  fullscreenHintLabel,
  isFullscreenSupported,
  supportsKeyboardLock,
} from "./fullscreen";
import type { NavigatorWithKeyboard } from "./fullscreen";
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
import {
  loadQualityPreset,
  QUALITY_PRESET_LABELS,
  saveQualityPreset,
} from "./qualityPreset";
import {
  cmdAsCtrlApplies,
  isMacClient,
  loadCmdAsCtrlSetting,
  saveCmdAsCtrlSetting,
} from "./keyRemap";
import {
  collapseButtonTitle,
  loadSessionBarCollapsed,
  loadStatsVisible,
  saveSessionBarCollapsed,
  saveStatsVisible,
} from "./sessionBar";
import { reconnectBackoffMs } from "./reconnectBackoff";
import {
  connectionBannerLabel,
  connectionStateOutcome,
  DISCONNECT_GRACE_MS,
  recoveryDelayMs,
  recoveryExhausted,
  shouldAttemptRecovery,
} from "./sessionRecovery";
import type { ConnectionBannerPhase } from "./sessionRecovery";
import type { ControlMessage } from "./generated/ControlMessage";
import type { DisplayEntry } from "./generated/DisplayEntry";
import type { DeviceEntry } from "./generated/DeviceEntry";
import type { HostOs } from "./generated/HostOs";
import type { InputMessage } from "./generated/InputMessage";
import type { QualityPreset } from "./generated/QualityPreset";

/** Slice 3.5b adds `disconnecting` (a `DISCONNECT_GRACE_MS` window is
 * running, see `sessionRecovery.connectionStateOutcome`) and `reconnecting`
 * (an automatic recovery attempt to the same device is in flight, see
 * `beginRecovery`). */
type SessionStatus =
  | "connecting"
  | "connected"
  | "disconnecting"
  | "reconnecting"
  | "disconnected"
  | "error";

/** Russian display text for each `SessionStatus`, shown in `#session-status`
 * (see `setSessionStatus`) -- the `data-status` attribute keeps the English
 * enum value unchanged (CSS selectors like `.status[data-status="error"]`
 * key off it), only the visible text is translated. */
const SESSION_STATUS_LABELS: Record<SessionStatus, string> = {
  connecting: "подключение",
  connected: "подключено",
  disconnecting: "связь прерывается…",
  reconnecting: "переподключение…",
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
  preset: "пресет",
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

/** How long `#fullscreen-note` (the "for exit press/hold Esc" hint) stays
 * visible after entering fullscreen (slice 3.5c). */
const FULLSCREEN_NOTE_MS = 3000;

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
      <div class="session-bar" id="session-bar">
        <div class="session-bar-left">
          <label class="stats-toggle" title="Показывать статистику видеопотока">
            <input type="checkbox" id="stats-checkbox" />
            Статистика
          </label>
          <span class="overlay" id="stats-overlay" hidden></span>
        </div>
        <div class="controls">
          <select id="display-select" class="display-select" hidden></select>
          <select id="quality-select" class="quality-select" title="Качество"></select>
          <label id="cmd-as-ctrl-label" class="cmd-as-ctrl-toggle" hidden>
            <input type="checkbox" id="cmd-as-ctrl-checkbox" />
            Cmd как Ctrl
          </label>
          <span id="view-only" class="status view-only" hidden></span>
          <span id="input-blocked" class="status input-blocked" hidden></span>
          <span id="clipboard-note" class="status clipboard-note" hidden></span>
          <span id="fullscreen-note" class="status fullscreen-note" hidden></span>
          <span id="session-status" class="status"></span>
          <button id="fullscreen-btn" class="btn btn-secondary" hidden>На весь экран</button>
          <button id="disconnect-btn" class="btn btn-secondary">Отключиться</button>
          <button
            id="bar-collapse-btn"
            class="btn btn-secondary bar-collapse-btn"
            title="Свернуть панель"
            aria-label="Свернуть панель"
          >▲</button>
        </div>
      </div>
      <button
        class="session-bar-handle"
        id="session-bar-handle"
        title="Показать панель"
        aria-label="Показать панель"
        hidden
      ></button>
      <div class="session-video">
        <video id="video" autoplay playsinline muted></video>
        <div class="connection-banner" id="connection-banner" hidden></div>
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
  // D32: the first real frame of a (re)started session -- `playing` rather
  // than `loadeddata` since it only fires once decoding/rendering actually
  // resumes (`loadeddata` can fire for a frame that's then immediately
  // stalled). Attached once, here, since `video` itself persists across
  // sessions -- `srcObject` is what gets reassigned on each new one (see
  // `joined`, which resets `firstFrameShown` before that reassignment).
  video.addEventListener("playing", () => {
    if (firstFrameShown) return;
    firstFrameShown = true;
    updateConnectionBanner();
  });
  const overlay = root.querySelector<HTMLSpanElement>("#stats-overlay")!;
  const connectionBannerEl = root.querySelector<HTMLDivElement>("#connection-banner")!;
  const sessionStatus = root.querySelector<HTMLSpanElement>("#session-status")!;
  const disconnectBtn = root.querySelector<HTMLButtonElement>("#disconnect-btn")!;
  const displaySelect = root.querySelector<HTMLSelectElement>("#display-select")!;
  const qualitySelect = root.querySelector<HTMLSelectElement>("#quality-select")!;
  // Fixed set of options (unlike `#display-select`'s, which is rebuilt from
  // the host's list on every `displays` message) -- built once, here, from
  // `QUALITY_PRESET_LABELS` so the picker's text has a single source of
  // truth (see `qualityPreset.ts`).
  for (const [value, label] of Object.entries(QUALITY_PRESET_LABELS)) {
    const el = document.createElement("option");
    el.value = value;
    el.textContent = label;
    qualitySelect.appendChild(el);
  }
  const cmdAsCtrlLabel = root.querySelector<HTMLLabelElement>("#cmd-as-ctrl-label")!;
  const cmdAsCtrlCheckbox = root.querySelector<HTMLInputElement>("#cmd-as-ctrl-checkbox")!;
  // Decided once per tab (the physical keyboard doesn't change mid-session),
  // unlike `hostOs` below, which is per-session and arrives from the host.
  const clientIsMac = isMacClient(navigator);
  // Slice 3.5f: a global (not per-device) setting -- see `keyRemap.ts`'s
  // `CMD_AS_CTRL_STORAGE_KEY` doc comment for why. Loaded once here rather
  // than per-session, like `qualitySelect`'s per-device choice is.
  const initialStorage = ownerStorage();
  let cmdAsCtrl = initialStorage ? loadCmdAsCtrlSetting(initialStorage) : true;
  cmdAsCtrlCheckbox.checked = cmdAsCtrl;
  const viewOnlyEl = root.querySelector<HTMLSpanElement>("#view-only")!;
  const inputBlockedEl = root.querySelector<HTMLSpanElement>("#input-blocked")!;
  const clipboardNoteEl = root.querySelector<HTMLSpanElement>("#clipboard-note")!;
  const fullscreenNoteEl = root.querySelector<HTMLSpanElement>("#fullscreen-note")!;
  const fullscreenBtn = root.querySelector<HTMLButtonElement>("#fullscreen-btn")!;
  // `document.fullscreenEnabled` doesn't change over a tab's lifetime, so
  // this is decided once, here, rather than on every render (slice 3.5c).
  fullscreenBtn.hidden = !isFullscreenSupported(document);
  const devicesEl = root.querySelector<HTMLDivElement>("#devices")!;
  const deviceListEl = root.querySelector<HTMLUListElement>("#device-list")!;

  // Slice 3.5g: the session bar, its collapse-to-a-strip control, and the
  // "Статистика" checkbox gating the overlay's text -- both choices are
  // global settings (see `sessionBar.ts`'s doc comments), loaded once here
  // from `initialStorage`, same as `cmdAsCtrl` above.
  const sessionBarEl = root.querySelector<HTMLDivElement>("#session-bar")!;
  const sessionBarHandle = root.querySelector<HTMLButtonElement>("#session-bar-handle")!;
  const barCollapseBtn = root.querySelector<HTMLButtonElement>("#bar-collapse-btn")!;
  const statsCheckbox = root.querySelector<HTMLInputElement>("#stats-checkbox")!;
  let statsVisible = initialStorage ? loadStatsVisible(initialStorage) : false;
  statsCheckbox.checked = statsVisible;
  overlay.hidden = !statsVisible;

  /** Applies `collapsed` to the session bar/handle (hidden state, title/
   * aria-label) and saves the choice (slice 3.5g) -- used both to apply the
   * setting loaded from storage at mount, and by `#bar-collapse-btn`'s and
   * `#session-bar-handle`'s click listeners below. */
  function applyBarCollapsed(collapsed: boolean): void {
    sessionBarEl.hidden = collapsed;
    sessionBarHandle.hidden = !collapsed;
    const title = collapseButtonTitle(collapsed);
    barCollapseBtn.title = title;
    barCollapseBtn.setAttribute("aria-label", title);
    sessionBarHandle.title = title;
    sessionBarHandle.setAttribute("aria-label", title);
    const storage = ownerStorage();
    if (storage) saveSessionBarCollapsed(storage, collapsed);
  }

  applyBarCollapsed(initialStorage ? loadSessionBarCollapsed(initialStorage) : false);

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
  // The host's OS for this session (slice 3.5f), from
  // `ControlMessage::HostInfo` -- `null` until it arrives (or once
  // `stopSessionResources` resets it for the next session). Drives whether
  // the "Cmd как Ctrl" toggle is shown (`updateCmdAsCtrlVisibility`) and, via
  // `maybeAttachInput`'s `getCmdAsCtrl`, whether it has any effect at all.
  let hostOs: HostOs | null = null;
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
  let fullscreenNoteTimer: ReturnType<typeof setTimeout> | undefined;
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
  // Slice 3.5b: the persistent device id from the current/last `Joined`
  // (`#[serde(default)]` on the wire, so `null` with a pre-3.5b server) --
  // what `beginRecovery` reconnects to after losing a session. Cleared on
  // any intentional teardown (see `teardown`) so a stray timer left over
  // from a previous session can never reconnect to the wrong device.
  let deviceId: string | null = null;
  // Whether the video has shown a frame yet for the *current* `PeerSession`
  // -- drives the D32 "Подключение к хосту…" banner (see
  // `updateConnectionBanner`). Reset every time a fresh session starts
  // (`joined`), since `video.srcObject` is reassigned to a new stream then.
  let firstFrameShown = false;
  // The pending `DISCONNECT_GRACE_MS` timer armed on a `disconnected`
  // connection state (see `onConnectionStateChange` below); cleared on
  // `connected` (recovered on its own), when it fires (the session is
  // lost), or on any teardown.
  let disconnectGraceTimer: ReturnType<typeof setTimeout> | undefined;
  // Slice 3.5b: `true` while automatically reconnecting to `deviceId` after
  // losing a session (see `beginRecovery`) -- the session screen stays up
  // throughout, showing the "reconnecting" status/banner.
  let recovering = false;
  // 1-based attempt count for `sessionRecovery.recoveryDelayMs`'s backoff;
  // only meaningful while `recovering`.
  let recoveryAttempt = 0;
  let recoveryTimer: ReturnType<typeof setTimeout> | undefined;
  // Set when a recovery attempt's `connect_device` couldn't be sent because
  // the signaling socket itself is down (`connectionLost`) -- the attempt is
  // retried as soon as `authenticated` confirms the socket is back, without
  // burning it on a send that would just be dropped (see
  // `performRecoveryAttempt`).
  let recoveryWaitingForSocket = false;

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

  /** Shows `note` in `#fullscreen-note` for `FULLSCREEN_NOTE_MS` -- the "for
   * exit press/hold Esc" hint shown once on entering fullscreen (see the
   * `fullscreenchange` listener below). */
  function showFullscreenNote(note: string): void {
    fullscreenNoteEl.textContent = note;
    fullscreenNoteEl.hidden = false;
    if (fullscreenNoteTimer !== undefined) clearTimeout(fullscreenNoteTimer);
    fullscreenNoteTimer = setTimeout(() => {
      hideFullscreenNote();
    }, FULLSCREEN_NOTE_MS);
  }

  /** Hides `#fullscreen-note` right away, e.g. on exiting fullscreen (so a
   * still-pending hint from a brief fullscreen stay doesn't linger). */
  function hideFullscreenNote(): void {
    if (fullscreenNoteTimer !== undefined) {
      clearTimeout(fullscreenNoteTimer);
      fullscreenNoteTimer = undefined;
    }
    fullscreenNoteEl.hidden = true;
    fullscreenNoteEl.textContent = "";
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
      // Slice 3.5f: a getter, not `cmdAsCtrl` itself, so a later toggle
      // (see the checkbox's `change` listener below) is picked up by every
      // keydown/keyup without re-attaching.
      () => cmdAsCtrl,
    );
  }

  /** Shows/hides `#cmd-as-ctrl-label` for the current `hostOs`/`clientIsMac`
   * (slice 3.5f) -- called whenever either could have changed: the host's
   * `host_info` arriving, and `stopSessionResources` resetting `hostOs` back
   * to `null` between sessions. */
  function updateCmdAsCtrlVisibility(): void {
    cmdAsCtrlLabel.hidden = !cmdAsCtrlApplies(clientIsMac, hostOs);
  }

  /** Sends `InputMessage::ReleaseAll` on the `input` channel directly (a
   * no-op if it isn't open) -- used when the "Cmd как Ctrl" setting changes
   * mid-session (see the checkbox's `change` listener below), so a Cmd/Ctrl
   * held down through the flip can't get stuck pressed on the host (its
   * `code` on the wire would otherwise switch mid-press, e.g. "Meta down,
   * Ctrl up"). */
  function releaseAllKeys(): void {
    if (inputChannel?.readyState === "open") {
      const msg: InputMessage = { type: "release_all" };
      inputChannel.send(JSON.stringify(msg));
    }
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

  /** Sends `ControlMessage::SetQuality` on `dc` if it's open, a no-op
   * otherwise (mirrors `displaySelect`'s change listener, which checks
   * `readyState` itself rather than relying on a caller to). */
  function sendQualityPreset(dc: RTCDataChannel, preset: QualityPreset): void {
    if (dc.readyState !== "open") return;
    const msg: ControlMessage = { type: "set_quality", preset };
    dc.send(JSON.stringify(msg));
  }

  function setupControlChannel(dc: RTCDataChannel): void {
    controlChannel = dc;
    // Slice 3.5e: a fresh `control` channel (a new session, or a 3.5b
    // automatic reconnect) means a fresh host-side session too -- the
    // adaptation controller always starts at `auto` (see
    // `host/src/adapt/mod.rs`'s `Controller::new`), so a remembered non-auto
    // choice has to be resent every time, not just once per device.
    const announcePreset = () => {
      const storage = ownerStorage();
      const preset = storage ? loadQualityPreset(storage, deviceId) : "auto";
      if (preset !== "auto") sendQualityPreset(dc, preset);
    };
    if (dc.readyState === "open") {
      startPingLoop(dc);
      announcePreset();
    } else {
      dc.addEventListener("open", () => {
        startPingLoop(dc);
        announcePreset();
      });
    }
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
      if (msg.type === "host_info") {
        hostOs = msg.os;
        updateCmdAsCtrlVisibility();
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
    // Slice 3.5g: the collapsed bar's handle strip mirrors the status via
    // color (`.session-bar-handle[data-status=...]` in style.css) since its
    // text isn't visible while collapsed.
    sessionBarHandle.dataset.status = status;
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
          const text = formatOverlay(summary, appRttMs, quality);
          overlay.textContent = text;
          // Slice 3.5g: the bar no longer has room to show the full line
          // unclipped at every viewport width (`text-overflow: ellipsis` in
          // style.css) -- the tooltip keeps the whole string reachable.
          overlay.title = text;
        })
        .catch((err: unknown) => {
          console.error("failed to read stats", err);
        });
    }, STATS_INTERVAL_MS);
  }

  function clearDisconnectGraceTimer(): void {
    if (disconnectGraceTimer !== undefined) {
      clearTimeout(disconnectGraceTimer);
      disconnectGraceTimer = undefined;
    }
  }

  function clearRecoveryTimer(): void {
    if (recoveryTimer !== undefined) {
      clearTimeout(recoveryTimer);
      recoveryTimer = undefined;
    }
  }

  /** Recomputes and applies the D32 video-overlay banner (`#connection-banner`)
   * from the current state -- called after anything that can change which
   * `ConnectionBannerPhase` applies (a fresh `joined`, the first video
   * frame, the disconnect grace window starting/ending, a recovery attempt
   * ticking over). Priority mirrors `sessionRecovery.ConnectionBannerPhase`'s
   * doc comment: reconnecting > disconnecting > connecting (no frame yet) >
   * hidden. */
  function updateConnectionBanner(): void {
    let phase: ConnectionBannerPhase;
    if (recovering) {
      phase = { kind: "reconnecting", attempt: recoveryAttempt };
    } else if (disconnectGraceTimer !== undefined) {
      phase = { kind: "disconnecting" };
    } else if (!firstFrameShown) {
      phase = { kind: "connecting" };
    } else {
      phase = { kind: "streaming" };
    }
    const label = connectionBannerLabel(phase);
    connectionBannerEl.textContent = label ?? "";
    connectionBannerEl.hidden = label === null;
  }

  /** Tears down everything belonging to the *current* `PeerSession` --
   * shared by `teardown` (an intentional/final end of session) and
   * `handleSessionLost` (which, unlike `teardown`, may then start
   * `beginRecovery` instead of returning to the PIN/list screen). Leaves the
   * screen, `pinStatus`/`connectBtn`, and the recovery/grace-window state
   * machine itself untouched -- callers decide those. */
  function stopSessionResources(): void {
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
    hostOs = null;
    updateCmdAsCtrlVisibility();
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
    sessionId = null;
    prevSnapshot = undefined;
    video.srcObject = null;
    video.style.cursor = "";
    overlay.textContent = "";
    firstFrameShown = false;
  }

  /** Final, intentional end of session (slice 3.5b: button, peer `bye`,
   * negotiation error, or a `beginRecovery` run that exhausted all its
   * attempts) -- unlike `handleSessionLost`, always returns to the PIN/list
   * screen and never triggers `beginRecovery`. Clears every piece of the
   * disconnect-grace/recovery state machine so nothing left over from this
   * session can fire later. */
  function teardown(reason: string): void {
    clearDisconnectGraceTimer();
    clearRecoveryTimer();
    recovering = false;
    recoveryAttempt = 0;
    recoveryWaitingForSocket = false;
    deviceId = null;
    // Slice 3.5c: a final end of session (unlike `handleSessionLost` starting
    // a recovery attempt, which leaves the session screen -- and fullscreen
    // -- up) shouldn't leave the tab stuck in fullscreen with the PIN/list
    // screen behind it. `exitFullscreen` is async; the `fullscreenchange`
    // listener below does the rest (button label, Keyboard Lock unlock,
    // refocus) once it resolves.
    if (document.fullscreenElement === sessionScreen) {
      void document.exitFullscreen().catch((err: unknown) => {
        console.error("failed to exit fullscreen", err);
      });
    }
    stopSessionResources();
    updateConnectionBanner();
    // Unlike before this slice, the signaling connection itself is *not*
    // closed or discarded here -- it's one long-lived connection for the
    // whole tab now (see where `signaling` is created above), since the
    // server only remembers which owner (and, mid-session, which device) a
    // socket authenticated as for the lifetime of that one WebSocket. Only
    // the WebRTC session ends; the tab drops back to the PIN/list screen on
    // the same connection.
    connectBtn.disabled = connectionLost;
    pinStatus.textContent = reason;
    showPinScreen();
    // Refresh the list right away rather than waiting for the next
    // `startDeviceListLoop` tick (up to `DEVICE_LIST_INTERVAL_MS` later) --
    // state (this device's own `busy`, in particular) just changed.
    signaling.send({ type: "list_devices" });
  }

  /** Sends `connect_device` for `deviceId` to retry the current recovery
   * attempt, or, if the signaling socket itself is down right now, defers it
   * -- `authenticated` (below) retries it as soon as the socket is back,
   * without spending another attempt on a send that would just be dropped
   * (slice 3.5b). */
  function performRecoveryAttempt(): void {
    if (!deviceId) {
      // Can't happen (`beginRecovery` only runs when `shouldAttemptRecovery`
      // is true), but stay defensive rather than reconnecting to nothing.
      teardown("Не удалось переподключиться");
      return;
    }
    if (connectionLost) {
      recoveryWaitingForSocket = true;
      return;
    }
    signaling.send({ type: "connect_device", device_id: deviceId });
  }

  /** Schedules recovery attempt number `recoveryAttempt + 1` after
   * `recoveryDelayMs`, or gives up (`teardown`) once
   * `recoveryExhausted` -- called for the first attempt (`beginRecovery`)
   * and again after each failed one (the `error` handler below). */
  function scheduleRecoveryAttempt(): void {
    recoveryAttempt += 1;
    updateConnectionBanner();
    if (recoveryExhausted(recoveryAttempt)) {
      teardown("Не удалось переподключиться");
      return;
    }
    clearRecoveryTimer();
    recoveryTimer = setTimeout(() => {
      recoveryTimer = undefined;
      performRecoveryAttempt();
    }, recoveryDelayMs(recoveryAttempt));
  }

  /** Starts automatic reconnection to `deviceId` after `handleSessionLost`
   * -- the session screen stays up (only `stopSessionResources` already
   * ran), showing the "reconnecting" status/banner until either a `joined`
   * succeeds (see that handler, which resets `recovering`) or every attempt
   * is exhausted (`scheduleRecoveryAttempt`, which then tears down to the
   * PIN/list screen). */
  function beginRecovery(): void {
    recovering = true;
    recoveryAttempt = 0;
    setSessionStatus("reconnecting");
    scheduleRecoveryAttempt();
  }

  /** The session is gone -- either the `DISCONNECT_GRACE_MS` window expired
   * or the connection went straight to `failed`/`closed` (slice 3.5b, see
   * `connectionStateOutcome`). Tells the host (`bye`, best-effort -- only if
   * the signaling socket is actually up), tears down the dead
   * `PeerSession`, and either starts `beginRecovery` (a known `deviceId`) or
   * finishes with `teardown` back to the PIN/list screen. */
  function handleSessionLost(): void {
    const lostSessionId = sessionId;
    const canRecover = shouldAttemptRecovery(deviceId);
    stopSessionResources();
    if (lostSessionId && !connectionLost) {
      signaling.send({ type: "bye", session_id: lostSessionId });
    }
    if (canRecover) {
      beginRecovery();
    } else {
      teardown("Связь потеряна");
    }
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
    // Slice 3.5b: a recovery attempt deferred because the socket was down
    // (`performRecoveryAttempt`) retries right away now that it's back --
    // without this it would otherwise sit idle until `beginRecovery`'s
    // caller gives up waiting, since nothing else re-triggers it.
    if (recovering && recoveryWaitingForSocket) {
      recoveryWaitingForSocket = false;
      performRecoveryAttempt();
    }
  });

  signaling.on("devices", (msg) => {
    devices = msg.devices;
    renderDeviceList();
  });

  signaling.on("joined", (msg) => {
    sessionId = msg.session_id;
    // Slice 3.5b: refreshed on *every* `joined` (a fresh PIN join, a device-list
    // connect, or a `beginRecovery` retry) -- what a later `handleSessionLost`
    // reconnects to. Also resets the whole disconnect-grace/recovery state
    // machine: this is a brand new `PeerSession`, so a leftover window/attempt
    // from whatever session (if any) preceded it no longer applies.
    deviceId = msg.device_id;
    // Slice 3.5e: reflect the owner's remembered choice for this device (or
    // "auto" for a fresh/unknown one) in the picker right away -- the actual
    // `set_quality` message goes out once `control` opens (see
    // `setupControlChannel`), since there's no channel to send it on yet.
    const storage = ownerStorage();
    qualitySelect.value = storage ? loadQualityPreset(storage, deviceId) : "auto";
    firstFrameShown = false;
    clearDisconnectGraceTimer();
    clearRecoveryTimer();
    recovering = false;
    recoveryAttempt = 0;
    recoveryWaitingForSocket = false;
    showSessionScreen();
    setSessionStatus("connecting");
    updateConnectionBanner();

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

    // Bound to `session` right after construction, below; captured by
    // `onConnectionStateChange` so a stray event delivered from *this*
    // `RTCPeerConnection` after it's been superseded (`stopSessionResources`
    // reassigns `session`, e.g. via `handleSessionLost` starting a recovery
    // attempt) is recognized as stale and ignored, instead of e.g. tearing
    // down a session that has already moved on (slice 3.5b -- the client
    // analog of the host's `current_session_id`/generation checks).
    let thisSession: PeerSession;
    thisSession = session = new PeerSession(
      { iceServers: toRtcIceServers(msg.ice_servers) },
      {
        onIceCandidate: (candidate) => {
          if (sessionId) {
            signaling.send({ type: "ice", session_id: sessionId, candidate });
          }
        },
        onTrack: (stream) => {
          video.srcObject = stream;
          // A recovery session (slice 3.5b) starts without a user gesture,
          // and `stopSessionResources` left the element paused with no
          // source -- `autoplay` alone doesn't resume it (seen live in a
          // background tab: frames decoded, `video.paused` stayed true).
          // Muted playback needs no gesture, so ask explicitly.
          void video.play().catch(() => {
            // Safari's first session is covered by the play() inside the
            // click handler; nothing more to do if this one is refused.
          });
        },
        onDataChannel: (label, dc) => {
          if (label === "input") inputChannel = dc;
          else if (label === "pointer") pointerChannel = dc;
          else if (label === "control") setupControlChannel(dc);
          maybeAttachInput();
        },
        onConnectionStateChange: (state) => {
          if (session !== thisSession) return;
          // Slice 3.5b: `disconnected` no longer tears the session down --
          // see `sessionRecovery.connectionStateOutcome`'s doc comment for
          // why (mirrors `host/src/signaling/mod.rs`'s `DisconnectGrace`).
          const outcome = connectionStateOutcome(state, disconnectGraceTimer !== undefined);
          if (outcome === "connected") {
            clearDisconnectGraceTimer();
            setSessionStatus("connected");
            updateConnectionBanner();
          } else if (outcome === "start-disconnect-grace") {
            setSessionStatus("disconnecting");
            disconnectGraceTimer = setTimeout(() => {
              disconnectGraceTimer = undefined;
              handleSessionLost();
            }, DISCONNECT_GRACE_MS);
            updateConnectionBanner();
          } else if (outcome === "session-lost") {
            clearDisconnectGraceTimer();
            handleSessionLost();
          }
          // "already-disconnecting" / "ignore": nothing to do.
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
    if (recovering) {
      // A recovery attempt's `connect_device` was rejected (device offline
      // or busy, most likely) -- try again per the backoff schedule, or
      // give up once attempts are exhausted (slice 3.5b,
      // `scheduleRecoveryAttempt`). The session screen stays up either way;
      // the specific server message isn't surfaced per-attempt, only the
      // attempt count (the banner/status already show that).
      console.warn("recovery attempt failed", msg.message);
      scheduleRecoveryAttempt();
      return;
    }
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
    // Slice 3.5b: an intentional end of session -- no point in a page that's
    // going away scheduling a recovery attempt (or the grace window that
    // would lead to one) for a moment after it's gone.
    clearDisconnectGraceTimer();
    clearRecoveryTimer();
    recovering = false;
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

  fullscreenBtn.addEventListener("click", () => {
    if (document.fullscreenElement) {
      void document.exitFullscreen().catch((err: unknown) => {
        console.error("failed to exit fullscreen", err);
      });
    } else {
      void sessionScreen.requestFullscreen().catch((err: unknown) => {
        console.error("failed to enter fullscreen", err);
      });
    }
  });

  // Slice 3.5c: fires for every fullscreen transition, however it happened
  // (the button above, a long-press Esc while Keyboard Lock is active, or
  // the browser force-exiting because `#session-screen` got hidden -- see
  // `teardown`). Handles Keyboard Lock (Chrome only, see
  // `fullscreen.supportsKeyboardLock`) and the button label/hint/focus in one
  // place instead of duplicating them at every call site that can change
  // fullscreen state.
  document.addEventListener("fullscreenchange", () => {
    const isFullscreen = document.fullscreenElement === sessionScreen;
    fullscreenBtn.textContent = fullscreenButtonLabel(isFullscreen);
    const nav = navigator as Navigator & NavigatorWithKeyboard;
    if (isFullscreen) {
      const keyboardLockActive = supportsKeyboardLock(nav);
      if (keyboardLockActive) {
        void nav.keyboard?.lock?.().catch((err: unknown) => {
          console.error("failed to lock keyboard", err);
        });
      }
      showFullscreenNote(fullscreenHintLabel(keyboardLockActive));
    } else {
      nav.keyboard?.unlock?.();
      hideFullscreenNote();
    }
    // Keyboard input is listened for on `video` itself (see `input.ts`'s
    // `attachInput`) -- give it focus back after the transition either way,
    // same reasoning as the `click`/`change` listeners elsewhere in this file.
    video.focus();
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

  qualitySelect.addEventListener("change", () => {
    // The `<option>`s are exactly `QualityPreset`'s wire values (see the
    // options built from `QUALITY_PRESET_LABELS` above), so the select's
    // own value is already valid -- no parsing needed here.
    const preset = qualitySelect.value as QualityPreset;
    const storage = ownerStorage();
    if (storage) saveQualityPreset(storage, deviceId, preset);
    if (controlChannel) sendQualityPreset(controlChannel, preset);
    // Give focus back to the video, same reasoning as `displaySelect`'s
    // change listener above.
    video.focus();
  });

  cmdAsCtrlCheckbox.addEventListener("change", () => {
    cmdAsCtrl = cmdAsCtrlCheckbox.checked;
    const storage = ownerStorage();
    if (storage) saveCmdAsCtrlSetting(storage, cmdAsCtrl);
    // A held Cmd/Ctrl must not get stuck on the host mid-flip -- see
    // `releaseAllKeys`'s doc comment.
    releaseAllKeys();
    // Give focus back to the video, same reasoning as `displaySelect`'s/
    // `qualitySelect`'s change listeners above.
    video.focus();
  });

  // Slice 3.5g: collapses the session bar to its thin handle strip, which
  // becomes the only layout element left above the video -- not a pixel of
  // the remote screen is covered either way.
  barCollapseBtn.addEventListener("click", () => {
    applyBarCollapsed(true);
    video.focus();
  });

  sessionBarHandle.addEventListener("click", () => {
    applyBarCollapsed(false);
    video.focus();
  });

  statsCheckbox.addEventListener("change", () => {
    statsVisible = statsCheckbox.checked;
    overlay.hidden = !statsVisible;
    const storage = ownerStorage();
    if (storage) saveStatsVisible(storage, statsVisible);
    // Give focus back to the video, same reasoning as `displaySelect`'s/
    // `qualitySelect`'s change listeners above.
    video.focus();
  });

  showPinScreen();
}

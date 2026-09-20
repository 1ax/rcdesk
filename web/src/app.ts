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
import type { ControlMessage } from "./generated/ControlMessage";
import type { DisplayEntry } from "./generated/DisplayEntry";

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

  let signaling: SignalingClient | null = null;
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
  }

  function showSessionScreen(): void {
    pinScreen.hidden = true;
    sessionScreen.hidden = false;
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
    signaling?.close();
    signaling = null;
    sessionId = null;
    prevSnapshot = undefined;
    video.srcObject = null;
    video.style.cursor = "";
    overlay.textContent = "";
    connectBtn.disabled = false;
    pinStatus.textContent = reason;
    showPinScreen();
  }

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

    pinStatus.textContent = "";
    connectBtn.disabled = true;

    const client = new SignalingClient();
    signaling = client;

    client.on("joined", (msg) => {
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
              client.send({ type: "ice", session_id: sessionId, candidate });
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
            } else if (
              state === "failed" ||
              state === "closed" ||
              state === "disconnected"
            ) {
              teardown(`Соединение разорвано (${state})`);
            }
          },
        },
      );
      startStatsLoop();
    });

    client.on("offer", (msg) => {
      if (!session) return;
      session
        .acceptOffer(msg.sdp)
        .then((sdp) => {
          if (sessionId) {
            client.send({ type: "answer", session_id: sessionId, sdp });
          }
        })
        .catch((err: unknown) => {
          console.error("failed to negotiate session", err);
          setSessionStatus("error");
          teardown("Не удалось согласовать сеанс");
        });
    });

    client.on("ice", (msg) => {
      session?.addRemoteIce(msg.candidate).catch((err: unknown) => {
        console.error("failed to add remote ice candidate", err);
      });
    });

    client.on("bye", () => {
      setSessionStatus("disconnected");
      teardown("Сеанс завершён");
    });

    client.on("error", (msg) => {
      setSessionStatus("error");
      teardown(signalErrorLabel(msg.message));
    });

    client.connect(signalUrl());
    client.join(pin);
  });

  disconnectBtn.addEventListener("click", () => {
    if (signaling && sessionId) {
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

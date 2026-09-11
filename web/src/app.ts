// Assembles the two-screen UI (PIN entry -> live session) around
// `SignalingClient` and `PeerSession`. Signaling flow mirrors what the host
// speaks (see `host/src/signaling/mod.rs`): the host is the offer side, so
// this client only ever answers -- `joined` creates the `PeerSession`,
// `offer` is answered, ICE trickles both ways, and `ontrack` feeds the
// `<video>` element. Mouse/keyboard input (slice 1.4) is out of scope here;
// data channels opened by the host are accepted and stored by label but
// otherwise unused.

import "./style.css";
import { SignalingClient } from "./signaling";
import { PeerSession } from "./session";
import { summarizeStats, takeSnapshot } from "./stats";
import type { Snapshot, StatsSummary } from "./stats";

const ICE_SERVERS: RTCIceServer[] = [{ urls: "stun:stun.l.google.com:19302" }];

type SessionStatus = "connecting" | "connected" | "disconnected" | "error";

function signalUrl(): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws`;
}

function formatOverlay(s: StatsSummary): string {
  const parts: string[] = [];
  if (s.fps !== undefined) parts.push(`${s.fps.toFixed(0)} fps`);
  if (s.width !== undefined && s.height !== undefined) parts.push(`${s.width}x${s.height}`);
  if (s.kbps !== undefined) parts.push(`${(s.kbps / 1000).toFixed(1)} Mbit/s`);
  if (s.rttMs !== undefined) parts.push(`rtt ${s.rttMs.toFixed(0)} ms`);
  if (s.packetsLost !== undefined) parts.push(`loss ${s.packetsLost}`);
  if (s.codec !== undefined) parts.push(s.codec.replace(/^video\//, "").toUpperCase());
  return parts.join(" · ");
}

const STATS_INTERVAL_MS = 1000;

/** Builds the PIN/session UI inside `root` and wires it up. `root` may be
 * `null` (e.g. in an environment without the expected markup) -- a no-op. */
export function mount(root: Element | null): void {
  if (!root) return;

  root.innerHTML = `
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
        <button id="connect-btn" class="btn">Connect</button>
        <p id="pin-status" class="hint"></p>
      </div>
    </div>
    <div class="session-screen" id="session-screen" hidden>
      <video id="video" autoplay playsinline muted></video>
      <div class="overlay" id="stats-overlay"></div>
      <div class="controls">
        <span id="session-status" class="status"></span>
        <button id="disconnect-btn" class="btn btn-secondary">Disconnect</button>
      </div>
    </div>
  `;

  const pinScreen = root.querySelector<HTMLDivElement>("#pin-screen")!;
  const sessionScreen = root.querySelector<HTMLDivElement>("#session-screen")!;
  const pinInput = root.querySelector<HTMLInputElement>("#pin-input")!;
  const connectBtn = root.querySelector<HTMLButtonElement>("#connect-btn")!;
  const pinStatus = root.querySelector<HTMLParagraphElement>("#pin-status")!;
  const video = root.querySelector<HTMLVideoElement>("#video")!;
  const overlay = root.querySelector<HTMLDivElement>("#stats-overlay")!;
  const sessionStatus = root.querySelector<HTMLSpanElement>("#session-status")!;
  const disconnectBtn = root.querySelector<HTMLButtonElement>("#disconnect-btn")!;

  let signaling: SignalingClient | null = null;
  let session: PeerSession | null = null;
  let sessionId: string | null = null;
  let statsTimer: ReturnType<typeof setInterval> | undefined;
  let prevSnapshot: Snapshot | undefined;

  function setSessionStatus(status: SessionStatus): void {
    sessionStatus.textContent = status;
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
          overlay.textContent = formatOverlay(summary);
        })
        .catch((err: unknown) => {
          console.error("failed to read stats", err);
        });
    }, STATS_INTERVAL_MS);
  }

  function teardown(reason: string): void {
    stopStatsLoop();
    session?.close();
    session = null;
    signaling?.close();
    signaling = null;
    sessionId = null;
    prevSnapshot = undefined;
    video.srcObject = null;
    overlay.textContent = "";
    connectBtn.disabled = false;
    pinStatus.textContent = reason;
    showPinScreen();
  }

  connectBtn.addEventListener("click", () => {
    const pin = pinInput.value.trim();
    if (!/^\d{6}$/.test(pin)) {
      pinStatus.textContent = "Enter the 6-digit PIN";
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

      session = new PeerSession(
        { iceServers: ICE_SERVERS },
        {
          onIceCandidate: (candidate) => {
            if (sessionId) {
              client.send({ type: "ice", session_id: sessionId, candidate });
            }
          },
          onTrack: (stream) => {
            video.srcObject = stream;
          },
          onDataChannel: () => {
            // Accepted and stored by label in PeerSession; handling input
            // over these channels is slice 1.4.
          },
          onConnectionStateChange: (state) => {
            if (state === "connected") {
              setSessionStatus("connected");
            } else if (
              state === "failed" ||
              state === "closed" ||
              state === "disconnected"
            ) {
              teardown(`disconnected (${state})`);
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
          teardown("Failed to negotiate the session");
        });
    });

    client.on("ice", (msg) => {
      session?.addRemoteIce(msg.candidate).catch((err: unknown) => {
        console.error("failed to add remote ice candidate", err);
      });
    });

    client.on("bye", () => {
      setSessionStatus("disconnected");
      teardown("Session ended");
    });

    client.on("error", (msg) => {
      setSessionStatus("error");
      teardown(`Error: ${msg.message}`);
    });

    client.connect(signalUrl());
    client.join(pin);
  });

  disconnectBtn.addEventListener("click", () => {
    if (signaling && sessionId) {
      signaling.send({ type: "bye", session_id: sessionId });
    }
    teardown("Disconnected");
  });

  showPinScreen();
}

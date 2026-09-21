// WebSocket client for the signaling protocol (see `server/src/ws.rs` and
// `proto::signal::SignalMessage`, generated into `./generated/SignalMessage`).
//
// On connect this always sends `hello` with role "client" first, as
// `server/src/ws.rs::run_connection` requires that to be the first message on
// any connection before it will accept a `join`.

import type { SignalMessage } from "./generated/SignalMessage";

/** Minimal shape of the WebSocket API this client depends on, so tests can
 * inject a fake implementation instead of a real `WebSocket`. */
export interface WebSocketLike extends EventTarget {
  send(data: string): void;
  close(): void;
  readyState: number;
}

export type WebSocketFactory = (url: string) => WebSocketLike;

/** `WebSocket.OPEN` (not referenced via the global so tests without a
 * `WebSocket` global still work). */
const WS_OPEN = 1;

type SignalListener<T extends SignalMessage["type"]> = (
  msg: Extract<SignalMessage, { type: T }>,
) => void;

/** Version sent in the `hello` message. Defined by Vite (see
 * `vite.config.ts`); declared for the browser build in `vite-env.d.ts`. */
declare const __APP_VERSION__: string;

/** Client-side signaling connection: connects, sends `hello`/`join`, and
 * dispatches typed events for incoming `SignalMessage`s. */
export class SignalingClient extends EventTarget {
  private readonly wsFactory: WebSocketFactory;
  private ws: WebSocketLike | null = null;
  /** Messages requested before the socket reached OPEN; flushed right after
   * `hello` on `open`, in the order they were queued. */
  private pending: SignalMessage[] = [];

  constructor(wsFactory: WebSocketFactory = (url) => new WebSocket(url) as unknown as WebSocketLike) {
    super();
    this.wsFactory = wsFactory;
  }

  /** Opens the WebSocket connection and sends `hello` with role "client" as
   * soon as it's open. */
  connect(url: string): void {
    const ws = this.wsFactory(url);
    this.ws = ws;

    ws.addEventListener("open", () => {
      ws.send(JSON.stringify({ type: "hello", role: "client", version: __APP_VERSION__ }));
      const queued = this.pending;
      this.pending = [];
      for (const msg of queued) {
        ws.send(JSON.stringify(msg));
      }
    });

    ws.addEventListener("message", (event) => {
      const data = (event as MessageEvent).data;
      let msg: SignalMessage;
      try {
        msg = JSON.parse(typeof data === "string" ? data : String(data)) as SignalMessage;
      } catch (err) {
        console.error("invalid signal message json", err);
        return;
      }
      this.dispatchEvent(new CustomEvent(msg.type, { detail: msg }));
    });

    ws.addEventListener("close", () => {
      // Only reset if `ws` is still the current socket -- a stale listener
      // from a socket a previous `close()`/reconnect already replaced must
      // not clobber the new one. Pending messages are dropped, not queued
      // across the gap: they were addressed to a connection that's gone, and
      // by the time a reconnect (see `app.ts`) succeeds they'd be stale
      // anyway (slice 3.5a -- losing the WS must not affect a live WebRTC
      // session, so nothing here is quietly retried).
      if (this.ws === ws) {
        this.ws = null;
        this.pending = [];
      }
      this.dispatchEvent(new CustomEvent("close"));
    });

    ws.addEventListener("error", (event) => {
      this.dispatchEvent(new CustomEvent("socket-error", { detail: event }));
    });
  }

  /** Sends a `join` message for the given 6-digit PIN. */
  join(pin: string): void {
    this.send({ type: "join", pin });
  }

  /** Sends any `SignalMessage` to the server. */
  send(msg: SignalMessage): void {
    if (!this.ws) {
      console.error("cannot send: not connected");
      return;
    }
    // `WebSocket.send` throws while the socket is still CONNECTING; the
    // caller typically calls `join` right after `connect`, so queue until
    // `open` (where `hello` must go first anyway).
    if (this.ws.readyState !== WS_OPEN) {
      this.pending.push(msg);
      return;
    }
    this.ws.send(JSON.stringify(msg));
  }

  /** Subscribes to a typed incoming message. Returns an unsubscribe function. */
  on<T extends SignalMessage["type"]>(type: T, listener: SignalListener<T>): () => void {
    const handler = (event: Event) => {
      listener((event as CustomEvent).detail as Extract<SignalMessage, { type: T }>);
    };
    this.addEventListener(type, handler);
    return () => this.removeEventListener(type, handler);
  }

  close(): void {
    this.ws?.close();
    this.ws = null;
    this.pending = [];
  }
}

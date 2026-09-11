import { describe, expect, it, vi } from "vitest";
import { SignalingClient } from "./signaling";
import type { WebSocketLike } from "./signaling";

/** A fake `WebSocket` good enough to drive `SignalingClient`: records every
 * frame passed to `send`, and lets the test push `open`/`message`/`close`
 * events at will. */
class FakeWebSocket extends EventTarget implements WebSocketLike {
  sent: string[] = [];
  readyState = 0;

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    this.readyState = 3;
    this.dispatchEvent(new CustomEvent("close"));
  }

  open(): void {
    this.readyState = 1;
    this.dispatchEvent(new Event("open"));
  }

  message(payload: unknown): void {
    const data = typeof payload === "string" ? payload : JSON.stringify(payload);
    this.dispatchEvent(new MessageEvent("message", { data }));
  }
}

describe("SignalingClient", () => {
  it("sends hello with role client as the first message once open", () => {
    let ws!: FakeWebSocket;
    const client = new SignalingClient((_url) => {
      ws = new FakeWebSocket();
      return ws;
    });

    client.connect("ws://example.test/ws");
    expect(ws.sent).toHaveLength(0);

    ws.open();

    expect(ws.sent).toHaveLength(1);
    const first = JSON.parse(ws.sent[0]);
    expect(first.type).toBe("hello");
    expect(first.role).toBe("client");
    expect(typeof first.version).toBe("string");
  });

  it("queues join() called before open and sends it right after hello", () => {
    let ws!: FakeWebSocket;
    const client = new SignalingClient((_url) => {
      ws = new FakeWebSocket();
      return ws;
    });
    client.connect("ws://example.test/ws");
    client.join("146581"); // socket still CONNECTING: must not throw
    expect(ws.sent).toHaveLength(0);

    ws.open();

    expect(ws.sent.map((f) => JSON.parse(f).type)).toEqual(["hello", "join"]);
    expect(JSON.parse(ws.sent[1]).pin).toBe("146581");
  });

  it("join() sends a join message with the given pin", () => {
    let ws!: FakeWebSocket;
    const client = new SignalingClient((_url) => {
      ws = new FakeWebSocket();
      return ws;
    });
    client.connect("ws://example.test/ws");
    ws.open();

    client.join("123456");

    const joinMsg = JSON.parse(ws.sent[ws.sent.length - 1]);
    expect(joinMsg).toEqual({ type: "join", pin: "123456" });
  });

  it("dispatches a typed callback for an incoming joined message", () => {
    let ws!: FakeWebSocket;
    const client = new SignalingClient((_url) => {
      ws = new FakeWebSocket();
      return ws;
    });
    client.connect("ws://example.test/ws");
    ws.open();

    const onJoined = vi.fn();
    client.on("joined", onJoined);

    ws.message({ type: "joined", session_id: "sess-1", host_name: "mac-mini" });

    expect(onJoined).toHaveBeenCalledTimes(1);
    expect(onJoined).toHaveBeenCalledWith({
      type: "joined",
      session_id: "sess-1",
      host_name: "mac-mini",
    });
  });

  it("dispatches a typed callback for an incoming offer message", () => {
    let ws!: FakeWebSocket;
    const client = new SignalingClient((_url) => {
      ws = new FakeWebSocket();
      return ws;
    });
    client.connect("ws://example.test/ws");
    ws.open();

    const onOffer = vi.fn();
    client.on("offer", onOffer);

    ws.message({ type: "offer", session_id: "sess-1", sdp: "v=0..." });

    expect(onOffer).toHaveBeenCalledWith({
      type: "offer",
      session_id: "sess-1",
      sdp: "v=0...",
    });
  });

  it("does not throw and logs on invalid json", () => {
    let ws!: FakeWebSocket;
    const client = new SignalingClient((_url) => {
      ws = new FakeWebSocket();
      return ws;
    });
    client.connect("ws://example.test/ws");
    ws.open();

    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const onJoined = vi.fn();
    client.on("joined", onJoined);

    expect(() => ws.message("not json{{{")).not.toThrow();

    expect(onJoined).not.toHaveBeenCalled();
    expect(errorSpy).toHaveBeenCalled();
    errorSpy.mockRestore();
  });
});

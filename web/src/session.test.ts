import { describe, expect, it, vi } from "vitest";
import { PeerSession, toRtcIceServers } from "./session";
import type { RTCPeerConnectionLike } from "./session";

/** A fake `RTCPeerConnection` covering exactly what `PeerSession` uses, so
 * tests run without a real WebRTC stack (vitest here runs with
 * `environment: "node"`, no jsdom/browser WebRTC available). */
class FakePeerConnection implements RTCPeerConnectionLike {
  calls: string[] = [];
  onicecandidate: ((event: RTCPeerConnectionIceEvent) => void) | null = null;
  ontrack: ((event: RTCTrackEvent) => void) | null = null;
  ondatachannel: ((event: RTCDataChannelEvent) => void) | null = null;
  onconnectionstatechange: (() => void) | null = null;
  connectionState: RTCPeerConnectionState = "new";
  onsignalingstatechange: (() => void) | null = null;
  signalingState: RTCSignalingState = "stable";

  async setRemoteDescription(_desc: RTCSessionDescriptionInit): Promise<void> {
    this.calls.push("setRemoteDescription");
  }

  async setLocalDescription(_desc: RTCSessionDescriptionInit): Promise<void> {
    this.calls.push("setLocalDescription");
  }

  async createAnswer(): Promise<RTCSessionDescriptionInit> {
    this.calls.push("createAnswer");
    return { type: "answer", sdp: "v=0...answer" };
  }

  async addIceCandidate(_candidate: RTCIceCandidateInit): Promise<void> {
    this.calls.push("addIceCandidate");
  }

  close(): void {
    this.calls.push("close");
  }

  async getStats(): Promise<RTCStatsReport> {
    return new Map() as unknown as RTCStatsReport;
  }
}

describe("PeerSession.acceptOffer", () => {
  it("calls setRemoteDescription, createAnswer, setLocalDescription in order and returns the answer sdp", async () => {
    let pc!: FakePeerConnection;
    const session = new PeerSession({ iceServers: [] }, {}, (_config) => {
      pc = new FakePeerConnection();
      return pc;
    });

    const sdp = await session.acceptOffer("v=0...offer");

    expect(pc.calls).toEqual(["setRemoteDescription", "createAnswer", "setLocalDescription"]);
    expect(sdp).toBe("v=0...answer");
  });
});

describe("PeerSession.addRemoteIce", () => {
  it("maps sdp_mid -> sdpMid and sdp_mline_index -> sdpMLineIndex", async () => {
    let pc!: FakePeerConnection;
    const addIceCandidate = vi.fn().mockResolvedValue(undefined);
    const session = new PeerSession({ iceServers: [] }, {}, (_config) => {
      pc = new FakePeerConnection();
      pc.addIceCandidate = addIceCandidate;
      return pc;
    });

    await session.addRemoteIce({
      candidate: "candidate:1 1 UDP 2130706431 10.0.0.1 12345 typ host",
      sdp_mid: "0",
      sdp_mline_index: 1,
    });

    expect(addIceCandidate).toHaveBeenCalledWith({
      candidate: "candidate:1 1 UDP 2130706431 10.0.0.1 12345 typ host",
      sdpMid: "0",
      sdpMLineIndex: 1,
    });
  });
});

describe("toRtcIceServers", () => {
  it("maps urls/username/credential and turns null into undefined", () => {
    expect(
      toRtcIceServers([
        { urls: ["stun:stun.l.google.com:19302"], username: null, credential: null },
        {
          urls: ["turn:turn.rcdesk.app:3478?transport=udp"],
          username: "1700000000:rcdesk",
          credential: "c29tZS1jcmVk",
        },
      ]),
    ).toEqual([
      { urls: ["stun:stun.l.google.com:19302"], username: undefined, credential: undefined },
      {
        urls: ["turn:turn.rcdesk.app:3478?transport=udp"],
        username: "1700000000:rcdesk",
        credential: "c29tZS1jcmVk",
      },
    ]);
  });
});

describe("PeerSession ondatachannel", () => {
  it("stores an incoming data channel by label and notifies the callback", () => {
    let pc!: FakePeerConnection;
    const onDataChannel = vi.fn();
    const session = new PeerSession({ iceServers: [] }, { onDataChannel }, (_config) => {
      pc = new FakePeerConnection();
      return pc;
    });

    const fakeChannel = { label: "input" } as unknown as RTCDataChannel;
    pc.ondatachannel?.({ channel: fakeChannel } as RTCDataChannelEvent);

    expect(session.getDataChannel("input")).toBe(fakeChannel);
    expect(onDataChannel).toHaveBeenCalledWith("input", fakeChannel);
  });
});

describe("PeerSession ontrack jitterBufferTarget", () => {
  function setup(onTrack = vi.fn()) {
    let pc!: FakePeerConnection;
    new PeerSession({ iceServers: [] }, { onTrack }, (_config) => {
      pc = new FakePeerConnection();
      return pc;
    });
    return { pc, onTrack };
  }
  const stream = { id: "s" } as unknown as MediaStream;

  it("sets jitterBufferTarget to 0 when the receiver supports it", () => {
    const { pc } = setup();
    const receiver = { jitterBufferTarget: null as number | null };
    pc.ontrack?.({ receiver, streams: [stream] } as unknown as RTCTrackEvent);
    expect(receiver.jitterBufferTarget).toBe(0);
  });

  it("does not add jitterBufferTarget to a receiver without the property", () => {
    const { pc } = setup();
    const receiver = {};
    pc.ontrack?.({ receiver, streams: [stream] } as unknown as RTCTrackEvent);
    expect("jitterBufferTarget" in receiver).toBe(false);
  });

  it("still calls onTrack when the setter throws", () => {
    const { pc, onTrack } = setup();
    const receiver = {
      set jitterBufferTarget(_v: number | null) {
        throw new Error("bad value");
      },
      get jitterBufferTarget(): number | null {
        return null;
      },
    };
    pc.ontrack?.({ receiver, streams: [stream] } as unknown as RTCTrackEvent);
    expect(onTrack).toHaveBeenCalledWith(stream);
  });

  it("tolerates an event without receiver", () => {
    const { pc, onTrack } = setup();
    pc.ontrack?.({ streams: [stream] } as unknown as RTCTrackEvent);
    expect(onTrack).toHaveBeenCalledWith(stream);
  });
});

describe("PeerSession onClosedUnexpectedly (D67c)", () => {
  function setup() {
    let pc!: FakePeerConnection;
    const onClosedUnexpectedly = vi.fn();
    const session = new PeerSession({ iceServers: [] }, { onClosedUnexpectedly }, () => {
      pc = new FakePeerConnection();
      return pc;
    });
    return { pc, session, onClosedUnexpectedly };
  }

  it("fires once when the browser closes the connection", () => {
    const { pc, onClosedUnexpectedly } = setup();
    pc.signalingState = "closed";
    pc.onsignalingstatechange?.();
    expect(onClosedUnexpectedly).toHaveBeenCalledTimes(1);
  });

  it("does not fire a second time on a repeated event", () => {
    const { pc, onClosedUnexpectedly } = setup();
    pc.signalingState = "closed";
    pc.onsignalingstatechange?.();
    pc.onsignalingstatechange?.();
    expect(onClosedUnexpectedly).toHaveBeenCalledTimes(1);
  });

  it("does not fire after our own close()", () => {
    const { pc, session, onClosedUnexpectedly } = setup();
    session.close();
    pc.signalingState = "closed";
    pc.onsignalingstatechange?.();
    expect(onClosedUnexpectedly).not.toHaveBeenCalled();
  });

  it("does not fire for a non-closed signalingState", () => {
    const { pc, onClosedUnexpectedly } = setup();
    pc.signalingState = "stable";
    pc.onsignalingstatechange?.();
    expect(onClosedUnexpectedly).not.toHaveBeenCalled();
  });
});

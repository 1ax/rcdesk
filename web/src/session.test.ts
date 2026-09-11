import { describe, expect, it, vi } from "vitest";
import { PeerSession } from "./session";
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

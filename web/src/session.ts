// Thin wrapper around `RTCPeerConnection` for the client (answer) side of a
// session. The host is always the offer side (see `host/src/transport/mod.rs`
// and `host/src/signaling/mod.rs`): it creates the four fixed data channels
// (`input`, `pointer`, `control`, `file`) before sending its offer, so this
// class only ever *receives* data channels via `ondatachannel`, never opens
// its own.

import type { IceCandidate } from "./generated/IceCandidate";
import type { IceServer } from "./generated/IceServer";

/** Minimal shape of `RTCPeerConnection` this class depends on, so tests can
 * inject a fake implementation. */
export interface RTCPeerConnectionLike {
  setRemoteDescription(desc: RTCSessionDescriptionInit): Promise<void>;
  setLocalDescription(desc: RTCSessionDescriptionInit): Promise<void>;
  createAnswer(): Promise<RTCSessionDescriptionInit>;
  addIceCandidate(candidate: RTCIceCandidateInit): Promise<void>;
  close(): void;
  getStats(): Promise<RTCStatsReport>;
  onicecandidate: ((event: RTCPeerConnectionIceEvent) => void) | null;
  ontrack: ((event: RTCTrackEvent) => void) | null;
  ondatachannel: ((event: RTCDataChannelEvent) => void) | null;
  onconnectionstatechange: (() => void) | null;
  connectionState: RTCPeerConnectionState;
}

export type RTCPeerConnectionFactory = (
  config: RTCConfiguration,
) => RTCPeerConnectionLike;

export interface PeerSessionCallbacks {
  onIceCandidate?: (candidate: IceCandidate) => void;
  onTrack?: (stream: MediaStream) => void;
  onDataChannel?: (label: string, dc: RTCDataChannel) => void;
  onConnectionStateChange?: (state: RTCPeerConnectionState) => void;
}

function defaultFactory(config: RTCConfiguration): RTCPeerConnectionLike {
  return new RTCPeerConnection(config) as unknown as RTCPeerConnectionLike;
}

/** Maps a proto `IceCandidate` (snake_case, matching `server`/`host`'s Rust
 * types) to the browser's `RTCIceCandidateInit` (camelCase). */
function iceCandidateToInit(c: IceCandidate): RTCIceCandidateInit {
  return {
    candidate: c.candidate,
    sdpMid: c.sdp_mid,
    sdpMLineIndex: c.sdp_mline_index ?? undefined,
  };
}

/** Maps a browser `RTCIceCandidate` back to the proto `IceCandidate` shape. */
function iceCandidateFromRtc(c: RTCIceCandidate): IceCandidate {
  return {
    candidate: c.candidate,
    sdp_mid: c.sdpMid,
    sdp_mline_index: c.sdpMLineIndex,
  };
}

/** Maps the proto `IceServer`s sent in `joined` (snake_case, see
 * `server/src/ice.rs`) to the browser's `RTCIceServer[]`, turning the `null`
 * ts-rs gives `Option::None` into `undefined` (the shape `RTCPeerConnection`
 * expects for "no credential"). */
export function toRtcIceServers(servers: IceServer[]): RTCIceServer[] {
  return servers.map((s) => ({
    urls: s.urls,
    username: s.username ?? undefined,
    credential: s.credential ?? undefined,
  }));
}

/** One WebRTC peer session, client (answer) side. */
export class PeerSession {
  private readonly pc: RTCPeerConnectionLike;
  private readonly dataChannels = new Map<string, RTCDataChannel>();

  constructor(
    config: { iceServers: RTCIceServer[] },
    callbacks: PeerSessionCallbacks,
    factory: RTCPeerConnectionFactory = defaultFactory,
  ) {
    this.pc = factory({ iceServers: config.iceServers });

    this.pc.onicecandidate = (event) => {
      if (event.candidate) {
        callbacks.onIceCandidate?.(iceCandidateFromRtc(event.candidate));
      }
    };

    this.pc.ontrack = (event) => {
      const stream = event.streams[0];
      if (stream) {
        callbacks.onTrack?.(stream);
      }
    };

    this.pc.ondatachannel = (event) => {
      const dc = event.channel;
      this.dataChannels.set(dc.label, dc);
      callbacks.onDataChannel?.(dc.label, dc);
    };

    this.pc.onconnectionstatechange = () => {
      callbacks.onConnectionStateChange?.(this.pc.connectionState);
    };
  }

  /** Applies the host's offer, creates an answer, sets it as the local
   * description, and returns its SDP for the signaling layer to send back. */
  async acceptOffer(sdp: string): Promise<string> {
    await this.pc.setRemoteDescription({ type: "offer", sdp });
    const answer = await this.pc.createAnswer();
    await this.pc.setLocalDescription(answer);
    if (!answer.sdp) {
      throw new Error("createAnswer returned no sdp");
    }
    return answer.sdp;
  }

  /** Applies a remote ICE candidate received over signaling (trickle ICE). */
  async addRemoteIce(candidate: IceCandidate): Promise<void> {
    await this.pc.addIceCandidate(iceCandidateToInit(candidate));
  }

  /** The data channels opened by the host, keyed by label. */
  getDataChannel(label: string): RTCDataChannel | undefined {
    return this.dataChannels.get(label);
  }

  getStats(): Promise<RTCStatsReport> {
    return this.pc.getStats();
  }

  close(): void {
    this.pc.close();
  }
}

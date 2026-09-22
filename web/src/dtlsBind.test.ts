import { describe, expect, it } from "vitest";
import {
  authTag,
  decodeBase64Url,
  encodeBase64Url,
  fingerprintFromSdp,
  verifyAuthTag,
} from "./dtlsBind";

// Test vector shared with `host/src/dtls_bind.rs` (slice 3.2e) -- both
// implementations must produce this exact tag for these exact inputs, or
// the host and browser client would derive different tags for the same
// real login and every password-protected session would fail
// `verify_auth_tag`/`verifyAuthTag`.
const VECTOR_KEY = new Uint8Array(64).fill(0x01);
const VECTOR_ROLE = "offer";
const VECTOR_FINGERPRINT = "sha-256 AA:BB:CC";
const VECTOR_TAG = "ePsRNp-0qqZJLVlyAkuOVBJ5aAHjZxfQf27HgyY0Ncc";

describe("authTag", () => {
  it("matches the shared test vector", async () => {
    await expect(authTag(VECTOR_KEY, VECTOR_ROLE, VECTOR_FINGERPRINT)).resolves.toBe(VECTOR_TAG);
  });
});

describe("verifyAuthTag", () => {
  it("accepts the shared test vector", async () => {
    await expect(
      verifyAuthTag(VECTOR_KEY, VECTOR_ROLE, VECTOR_FINGERPRINT, VECTOR_TAG),
    ).resolves.toBe(true);
  });

  it("rejects a different role", async () => {
    const key = new Uint8Array(64).fill(0x02);
    const tag = await authTag(key, "offer", "sha-256 AA:BB");
    await expect(verifyAuthTag(key, "answer", "sha-256 AA:BB", tag)).resolves.toBe(false);
  });

  it("rejects a different fingerprint", async () => {
    const key = new Uint8Array(64).fill(0x02);
    const tag = await authTag(key, "offer", "sha-256 AA:BB");
    await expect(verifyAuthTag(key, "offer", "sha-256 AA:BC", tag)).resolves.toBe(false);
  });

  it("rejects a different key", async () => {
    const key = new Uint8Array(64).fill(0x02);
    const otherKey = new Uint8Array(64).fill(0x03);
    const tag = await authTag(key, "offer", "sha-256 AA:BB");
    await expect(verifyAuthTag(otherKey, "offer", "sha-256 AA:BB", tag)).resolves.toBe(false);
  });

  it("rejects garbage base64 instead of throwing", async () => {
    const key = new Uint8Array(64).fill(0x02);
    await expect(verifyAuthTag(key, "offer", "sha-256 AA:BB", "not valid base64!!")).resolves.toBe(
      false,
    );
  });
});

describe("fingerprintFromSdp", () => {
  it("finds a session-level fingerprint line", () => {
    const sdp =
      "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\na=fingerprint:sha-256 AB:CD:EF\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
    expect(fingerprintFromSdp(sdp)).toBe("sha-256 AB:CD:EF");
  });

  it("finds an m-section fingerprint line", () => {
    const sdp =
      "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=fingerprint:sha-256 11:22:33\r\n";
    expect(fingerprintFromSdp(sdp)).toBe("sha-256 11:22:33");
  });

  it("normalizes algorithm case and hex case", () => {
    const sdp = "a=fingerprint:SHA-256 ab:cd:ef\r\n";
    expect(fingerprintFromSdp(sdp)).toBe("sha-256 AB:CD:EF");
  });

  it("returns null when there is no fingerprint line", () => {
    const sdp =
      "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
    expect(fingerprintFromSdp(sdp)).toBeNull();
  });
});

describe("decodeBase64Url / encodeBase64Url", () => {
  it("round-trips arbitrary bytes", () => {
    const bytes = new Uint8Array([0, 1, 2, 253, 254, 255, 16, 32, 64, 128]);
    expect(decodeBase64Url(encodeBase64Url(bytes))).toEqual(bytes);
  });

  it("decodeBase64Url matches the shared test vector's key encoding", () => {
    // Same 64-byte all-0x01 key as `VECTOR_KEY`, round-tripped through the
    // URL-safe-no-pad alphabet `host/src/access.rs`'s `AccessRecordFile`
    // and `@serenity-kit/opaque`'s `sessionKey` both use.
    expect(decodeBase64Url(encodeBase64Url(VECTOR_KEY))).toEqual(VECTOR_KEY);
  });
});

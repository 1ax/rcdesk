import { describe, expect, it } from "vitest";
import {
  accountRowMode,
  assertionToServer,
  base64urlDecode,
  base64urlEncode,
  browserLabel,
  browserRowText,
  creationOptionsFromServer,
  isCancelError,
  isRevokedTokenError,
  passkeyFailText,
  passkeyRowText,
  registrationToServer,
  requestOptionsFromServer,
} from "./passkey";
import type { BrowserEntry } from "./generated/BrowserEntry";
import type { PasskeyEntry } from "./generated/PasskeyEntry";

function bytes(buf: ArrayBuffer | ArrayBufferView): number[] {
  return Array.from(
    buf instanceof ArrayBuffer ? new Uint8Array(buf) : new Uint8Array(buf.buffer),
  );
}

function buf(...values: number[]): ArrayBuffer {
  return new Uint8Array(values).buffer;
}

describe("base64url", () => {
  it("round-trips every padding length", () => {
    for (const values of [[], [1], [1, 2], [1, 2, 3], [1, 2, 3, 4], [255, 254, 253, 252, 251]]) {
      const encoded = base64urlEncode(buf(...values));
      expect(encoded).not.toContain("=");
      expect(bytes(base64urlDecode(encoded))).toEqual(values);
    }
  });

  it("uses - and _ instead of + and /", () => {
    // 0xfb 0xff 0xbf -> "+/+/" in standard base64
    expect(base64urlEncode(buf(0xfb, 0xff, 0xbf))).toBe("-_-_");
    expect(bytes(base64urlDecode("-_-_"))).toEqual([0xfb, 0xff, 0xbf]);
  });

  it("decodes padded and standard-alphabet input too", () => {
    expect(bytes(base64urlDecode("AQI="))).toEqual([1, 2]);
    expect(bytes(base64urlDecode("+/+/"))).toEqual([0xfb, 0xff, 0xbf]);
  });

  it("encodes typed array views honoring offset", () => {
    const view = new Uint8Array([9, 1, 2, 9]).subarray(1, 3);
    expect(base64urlEncode(view)).toBe("AQI");
  });
});

describe("creationOptionsFromServer", () => {
  const fixture = JSON.stringify({
    publicKey: {
      rp: { name: "rcdesk", id: "rcdesk.app" },
      user: { id: "AQID", name: "owner", displayName: "owner" },
      challenge: "BAUG",
      pubKeyCredParams: [{ type: "public-key", alg: -7 }],
      timeout: 60000,
      excludeCredentials: [{ type: "public-key", id: "Bwg", transports: ["internal"] }],
      authenticatorSelection: { residentKey: "required", requireResidentKey: true, userVerification: "preferred" },
      attestation: "none",
    },
  });

  it("turns challenge, user.id and excludeCredentials ids into buffers", () => {
    const opts = creationOptionsFromServer(fixture);
    expect(bytes(opts.challenge as ArrayBuffer)).toEqual([4, 5, 6]);
    expect(bytes(opts.user.id as ArrayBuffer)).toEqual([1, 2, 3]);
    expect(opts.excludeCredentials).toHaveLength(1);
    expect(bytes(opts.excludeCredentials![0].id as ArrayBuffer)).toEqual([7, 8]);
    expect(opts.excludeCredentials![0].transports).toEqual(["internal"]);
  });

  it("keeps the other fields as they are", () => {
    const opts = creationOptionsFromServer(fixture);
    expect(opts.rp).toEqual({ name: "rcdesk", id: "rcdesk.app" });
    expect(opts.user.name).toBe("owner");
    expect(opts.pubKeyCredParams).toEqual([{ type: "public-key", alg: -7 }]);
    expect(opts.timeout).toBe(60000);
    expect(opts.attestation).toBe("none");
    expect(opts.authenticatorSelection?.residentKey).toBe("required");
  });

  it("works without excludeCredentials", () => {
    const json = JSON.parse(fixture);
    delete json.publicKey.excludeCredentials;
    const opts = creationOptionsFromServer(JSON.stringify(json));
    expect(opts.excludeCredentials).toBeUndefined();
  });

  it("rejects malformed options", () => {
    expect(() => creationOptionsFromServer("{}")).toThrow();
  });
});

describe("requestOptionsFromServer", () => {
  const fixture = JSON.stringify({
    publicKey: {
      challenge: "BAUG",
      timeout: 60000,
      rpId: "rcdesk.app",
      allowCredentials: [],
      userVerification: "preferred",
    },
    mediation: "conditional",
  });

  it("decodes the challenge and drops the server's mediation", () => {
    const opts = requestOptionsFromServer(fixture);
    expect(bytes(opts.challenge as ArrayBuffer)).toEqual([4, 5, 6]);
    expect(opts.rpId).toBe("rcdesk.app");
    expect(opts.userVerification).toBe("preferred");
    expect(opts.allowCredentials).toEqual([]);
    expect(JSON.stringify(opts)).not.toContain("mediation");
    expect("mediation" in opts).toBe(false);
  });

  it("decodes allowCredentials ids", () => {
    const json = JSON.parse(fixture);
    json.publicKey.allowCredentials = [{ type: "public-key", id: "AQI" }];
    const opts = requestOptionsFromServer(JSON.stringify(json));
    expect(bytes(opts.allowCredentials![0].id as ArrayBuffer)).toEqual([1, 2]);
  });
});

describe("registrationToServer", () => {
  it("produces the RegisterPublicKeyCredential shape", () => {
    const json = JSON.parse(
      registrationToServer({
        id: "AQI",
        rawId: buf(1, 2),
        type: "public-key",
        response: {
          attestationObject: buf(3, 4),
          clientDataJSON: buf(5, 6),
          getTransports: () => ["internal", "hybrid"],
        },
        getClientExtensionResults: () => ({ credProps: { rk: true }, prf: { enabled: true } }),
      }),
    );
    expect(json).toEqual({
      id: "AQI",
      rawId: "AQI",
      response: {
        attestationObject: "AwQ",
        clientDataJSON: "BQY",
        transports: ["internal", "hybrid"],
      },
      type: "public-key",
      extensions: { credProps: { rk: true } },
    });
  });

  it("omits transports when the browser has none and tolerates no extensions API", () => {
    const json = JSON.parse(
      registrationToServer({
        id: "AQI",
        rawId: buf(1, 2),
        type: "public-key",
        response: { attestationObject: buf(3), clientDataJSON: buf(4) },
      }),
    );
    expect("transports" in json.response).toBe(false);
    expect(json.extensions).toEqual({});
  });
});

describe("assertionToServer", () => {
  it("produces the PublicKeyCredential shape", () => {
    const json = JSON.parse(
      assertionToServer({
        id: "AQI",
        rawId: buf(1, 2),
        type: "public-key",
        response: {
          authenticatorData: buf(3),
          clientDataJSON: buf(4),
          signature: buf(5, 6),
          userHandle: buf(7),
        },
        getClientExtensionResults: () => ({}),
      }),
    );
    expect(json).toEqual({
      id: "AQI",
      rawId: "AQI",
      response: {
        authenticatorData: "Aw",
        clientDataJSON: "BA",
        signature: "BQY",
        userHandle: "Bw",
      },
      extensions: {},
      type: "public-key",
    });
  });

  it("sends a null userHandle when there is none", () => {
    const json = JSON.parse(
      assertionToServer({
        id: "x",
        rawId: buf(1),
        type: "public-key",
        response: {
          authenticatorData: buf(1),
          clientDataJSON: buf(1),
          signature: buf(1),
          userHandle: null,
        },
      }),
    );
    expect(json.response.userHandle).toBeNull();
  });
});

describe("browserLabel", () => {
  const cases: [string, string][] = [
    [
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
      "Chrome · macOS",
    ],
    [
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
      "Safari · macOS",
    ],
    [
      "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
      "Safari · iOS",
    ],
    [
      "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/126.0.6478.153 Mobile/15E148 Safari/604.1",
      "Chrome · iOS",
    ],
    [
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.2592.87",
      "Edge · Windows",
    ],
    [
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0",
      "Firefox · Windows",
    ],
    [
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:128.0) Gecko/20100101 Firefox/128.0",
      "Firefox · macOS",
    ],
    [
      "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36",
      "Chrome · Android",
    ],
    [
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 OPR/112.0.0.0",
      "Opera · Windows",
    ],
    [
      "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
      "Firefox · Linux",
    ],
  ];
  for (const [ua, label] of cases) {
    it(label, () => {
      expect(browserLabel(ua)).toBe(label);
    });
  }

  it("falls back to 'Браузер' for empty or unknown", () => {
    expect(browserLabel("")).toBe("Браузер");
    expect(browserLabel("curl/8.0")).toBe("Браузер");
  });
});

describe("account screen logic", () => {
  const pk: PasskeyEntry = { id: "p1", user_agent: "", created_at: 1000n, last_used_at: 1000n };

  it("accountRowMode", () => {
    expect(accountRowMode(false, [])).toBe("hidden");
    expect(accountRowMode(true, null)).toBe("hidden");
    expect(accountRowMode(true, [])).toBe("guest");
    expect(accountRowMode(true, [pk])).toBe("account");
  });

  it("passkeyRowText", () => {
    const text = passkeyRowText(
      { ...pk, user_agent: "", created_at: 1000n, last_used_at: 1000n + 7200n },
      1000 + 7200 + 3 * 86400,
    );
    expect(text.title).toBe("Браузер");
    expect(text.status).toBe("создан 3 дня назад · последний вход 3 дня назад");
    const old = passkeyRowText({ ...pk, created_at: 0n, last_used_at: 3600n }, 3600 + 120);
    expect(old.status).toContain("последний вход 2 минуты назад");
  });

  it("browserRowText", () => {
    const entry: BrowserEntry = {
      id: "b1",
      user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0",
      created_at: 0n,
      last_used_at: 100n,
      current: false,
    };
    expect(browserRowText(entry, 130)).toEqual({
      title: "Firefox · Windows",
      status: "был только что",
    });
    expect(browserRowText({ ...entry, current: true }, 130).status).toBe("этот браузер");
  });

  it("isCancelError", () => {
    expect(isCancelError({ name: "NotAllowedError" })).toBe(true);
    expect(isCancelError({ name: "AbortError" })).toBe(true);
    expect(isCancelError({ name: "SecurityError" })).toBe(false);
    expect(isCancelError(new Error("passkey failed"))).toBe(false);
    expect(isCancelError(null)).toBe(false);
  });

  it("passkeyFailText", () => {
    expect(passkeyFailText("login")).toBe("Не удалось войти с passkey");
    expect(passkeyFailText("register")).toBe("Не удалось добавить passkey");
  });

  it("isRevokedTokenError needs both the message and a saved token", () => {
    expect(isRevokedTokenError("not authenticated", "tok")).toBe(true);
    expect(isRevokedTokenError("not authenticated", null)).toBe(false);
    expect(isRevokedTokenError("unknown pin", "tok")).toBe(false);
  });
});

import { describe, expect, it } from "vitest";
import { ConnectGuard } from "./connectGuard";

describe("ConnectGuard", () => {
  it("accepts a matching device id", () => {
    const g = new ConnectGuard();
    g.request("dev-1");
    expect(g.onJoined("dev-1")).toBe("accept");
  });

  it("accepts any device id for a PIN join (null)", () => {
    const g = new ConnectGuard();
    g.request(null);
    expect(g.onJoined("whatever")).toBe("accept");
  });

  it("reports mismatch when another device answers", () => {
    const g = new ConnectGuard();
    g.request("dev-1");
    expect(g.onJoined("dev-2")).toBe("mismatch");
    expect(g.pending).toBe(false);
  });

  it("reports unexpected without a request", () => {
    const g = new ConnectGuard();
    expect(g.onJoined("dev-1")).toBe("unexpected");
  });

  it("reports unexpected after cancel", () => {
    const g = new ConnectGuard();
    g.request("dev-1");
    g.cancel();
    expect(g.onJoined("dev-1")).toBe("unexpected");
  });

  it("consumes the request after one answer", () => {
    const g = new ConnectGuard();
    g.request(null);
    expect(g.onJoined("a")).toBe("accept");
    expect(g.onJoined("a")).toBe("unexpected");
  });

  it("toggles pending", () => {
    const g = new ConnectGuard();
    expect(g.pending).toBe(false);
    g.request("d");
    expect(g.pending).toBe(true);
    g.cancel();
    expect(g.pending).toBe(false);
    g.request(null);
    g.onJoined("d");
    expect(g.pending).toBe(false);
  });
});

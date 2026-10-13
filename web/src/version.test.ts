import { describe, expect, it } from "vitest";
import { formatClientVersion } from "./version";
// Без @types/node: файлы читаем через vite `?raw`.
import pkgRaw from "../package.json?raw";
import cargoRaw from "../../Cargo.toml?raw";

describe("formatClientVersion", () => {
  it("formats the client version string", () => {
    expect(formatClientVersion("0.1.0")).toBe("rcdesk client 0.1.0");
  });
});

// Клиент сравнивает версию хоста со своей (D50): она должна подниматься
// вместе с релизом, то есть совпадать с версией воркспейса Cargo.
describe("version sync", () => {
  it("web/package.json matches [workspace.package] in Cargo.toml", () => {
    const pkg = JSON.parse(pkgRaw) as { version: string };
    const cargo = cargoRaw;
    const section = /\[workspace\.package\]([\s\S]*?)(?:\n\[|$)/.exec(cargo);
    expect(section).not.toBeNull();
    const m = /^version\s*=\s*"([^"]+)"/m.exec(section![1]);
    expect(m).not.toBeNull();
    expect(pkg.version).toBe(m![1]);
  });
});

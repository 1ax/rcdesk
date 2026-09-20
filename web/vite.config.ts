import { defineConfig } from "vitest/config";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const pkg = JSON.parse(
  readFileSync(fileURLToPath(new URL("./package.json", import.meta.url)), "utf-8"),
) as { version: string };

export default defineConfig({
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
    // Short commit sha the build was made from, shown in the session panel
    // as `build <id>` (slice 2.6f) so a stale tab left open across a deploy
    // is obvious instead of silently missing new protocol handling. `dev`
    // outside CI (GITHUB_SHA unset), e.g. `npm run dev`/local `npm run build`.
    __BUILD_ID__: JSON.stringify(process.env.GITHUB_SHA?.slice(0, 7) ?? "dev"),
  },
  server: {
    proxy: {
      "/ws": {
        target: "ws://127.0.0.1:8080",
        ws: true,
      },
    },
  },
  test: {
    environment: "node",
  },
});

# rcdesk

Remote desktop in the browser. A Rust host agent for macOS (arm64) and Windows (x64) streams the
screen over WebRTC (H.264) to a web client that works in Safari and Chrome, with mouse, keyboard,
clipboard and file transfer over data channels. Production: https://rcdesk.app.

## Layout

- `proto/` — message types shared by Rust and TypeScript (generated via ts-rs)
- `host/` — host agent (`rcdesk-host` CLI, `rcdesk-agent` tray/menu-bar app)
- `server/` — signaling server (`rcdesk-server`)
- `web/` — web client (Vite + TypeScript)
- `installer/` — macOS `.pkg` and Windows Inno Setup installers
- `infra/` — production docker-compose (server + coturn), nginx config, deploy/backup/release scripts
- `vendor/` — patched copies of `rtc-sctp`, `rtc-turn`, `scap` (`[patch.crates-io]`)
- `docs/` — run/install instructions, audits

## Gates

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd web && npm run check && npm test && npm run build
```

The Rust toolchain is pinned in `rust-toolchain.toml`. Local dev run: `docs/dev-run.md`.

## Docs

- `ARCHITECTURE.md` — design decisions and stack
- `SLICES_LOG.md` — current state and history
- `CLAUDE.md` — working rules for Claude Code
- `docs/dev-run.md` — local run; `docs/host-macos.md`, `docs/host-windows.md` — host install
- `infra/README.md` — server, deploy, releases

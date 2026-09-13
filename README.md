# rcdesk

Remote desktop in the browser. A Rust host agent for macOS and Windows streams the
screen over WebRTC (H.264) to a web client that works in Safari and Chrome, with
mouse, keyboard, clipboard and file transfer over data channels.

**Status:** phase 1 (MVP) deployed at https://rcdesk.app; phase 2 in progress (Windows host works,
including a GDI capture fallback for machines without Direct3D 11). See `ARCHITECTURE.md` for design
and roadmap, `SLICES_LOG.md` for current state, `docs/host-windows.md` to run the host on Windows.

## Layout

- `proto/` — message types shared by Rust and TypeScript (generated via ts-rs)
- `host/` — host agent (`rcdesk-host`)
- `server/` — signaling server (`rcdesk-server`)
- `web/` — web client (Vite + TypeScript)
- `infra/` — docker-compose, Caddy, coturn

## Gates

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd web && npm run check && npm test && npm run build
```
